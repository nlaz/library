//! Chat: the librarian sidecar (apps/librarian) over stdio. The sidecar runs
//! the Apple Foundation Models agent loop; its tool calls come back as
//! `tool_request` lines and are executed in-process against the engine via
//! the shared library_core::tools — the same implementations the server's
//! HTTP routes use. Model sessions live in the sidecar, keyed by `conv`.

use std::path::{Path, PathBuf};

use library_core::ClipEmb;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::engine::{AppState, Engine, dev_root, engine};

pub(crate) struct ChatBridge {
    pub(crate) child: std::process::Child,
    pub(crate) stdin: std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    pub(crate) lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}

impl Drop for ChatBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn librarian_bin(app: &AppHandle) -> PathBuf {
    if let Ok(p) = std::env::var("LIBRARIAN_BIN") {
        return PathBuf::from(p);
    }
    // bundled resource in release, repo build at dev time
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join("librarian");
        if p.exists() {
            return p;
        }
    }
    dev_root().join("apps/librarian/.build/release/librarian")
}

fn spawn_chat(app: &AppHandle) -> Result<ChatBridge, String> {
    use std::io::BufRead;
    let bin = librarian_bin(app);
    // real collection names ride into the tool schema + instructions so the
    // model can scope searches without guessing
    let cols: Vec<String> =
        library_core::wire::read_collections(&app.state::<AppState>().settings.data)
            .into_keys()
            .collect();
    let mut child = std::process::Command::new(&bin)
        .args(["serve", "--tools-stdin", "--collections", &cols.join(",")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("librarian sidecar failed to start ({}): {e}", bin.display()))?;
    let stdin = std::sync::Arc::new(std::sync::Mutex::new(
        child.stdin.take().expect("piped stdin"),
    ));
    let mut lines = std::io::BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    match lines.next() {
        Some(Ok(l)) if l.contains("\"ready\"") => {}
        _ => return Err("librarian sidecar did not become ready".into()),
    }
    Ok(ChatBridge {
        child,
        stdin,
        lines,
    })
}

fn execute_tool(eng: &Engine, data: &Path, name: &str, args: &serde_json::Value) -> String {
    use library_core::tools;
    let figure_search = |q: &str, col: &str| -> String {
        let member = match tools::resolve_collection(data, col) {
            Ok(m) => m,
            Err(e) => return e.to_string(),
        };
        let qemb: Option<ClipEmb> = eng
            .clip_text
            .embed(vec![q.to_string()], None)
            .ok()
            .and_then(|mut v| v.pop())
            .and_then(|v| v.try_into().ok());
        let found = qemb
            .map(|e| {
                eng.images.read().expect("images lock poisoned").rtx(|r| {
                    library_core::image_search(&r, &e, library_core::IMG_FETCH, member.as_ref())
                })
            })
            .unwrap_or_default();
        tools::image_hits_for_tool(&found, data, tools::TOOL_K).to_string()
    };
    let q = args["query"].as_str().unwrap_or("");
    let col = args["collection"].as_str().unwrap_or("");
    match name {
        "search_library" => {
            let lib = eng.lib.read().expect("library lock poisoned");
            lib.rtx(|r| tools::search_tool(&r, &lib, data, q, col, tools::TOOL_K))
                .to_string()
        }
        "search_figures" => figure_search(q, col),
        "sample_page" => {
            // sidecar-injected session state, not a model-visible param
            let avoid: Vec<String> = args["avoid"]
                .as_str()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            tools::sample_page_tool(data, col, None, &avoid).to_string()
        }
        "read_pages" => {
            let doc = args["doc"].as_str().unwrap_or("");
            let from = args["from"].as_u64().map(|n| n as u32);
            let to = args["to"].as_u64().map(|n| n as u32);
            tools::read_pages_tool(data, doc, from, to).to_string()
        }
        "library_overview" => tools::overview_tool(data).to_string(),
        "list_collections" => tools::collections_tool(data).to_string(),
        _ => serde_json::json!({ "error": format!("unknown tool {name:?}") }).to_string(),
    }
}

/// One chat turn: forwards sidecar events to the webview as `chat:event`,
/// executes tool requests in-process, returns at `turn_end`. Runs on the
/// blocking pool; a wedged model is recovered by `chat_cancel` (the stop
/// button), which the sidecar honors between stream snapshots.
fn chat_turn_blocking(
    app: AppHandle,
    conv: String,
    messages: serde_json::Value,
) -> Result<(), String> {
    use std::io::Write;

    let state = app.state::<AppState>();
    let eng = engine(&state)?;
    let data = state.settings.data.clone();

    let mut guard = state.chat.lock().expect("chat bridge lock poisoned");
    if guard.is_none()
        || guard
            .as_mut()
            .is_some_and(|b| b.child.try_wait().is_ok_and(|s| s.is_some()))
    {
        let bridge = spawn_chat(&app)?;
        *state.chat_stdin.lock().expect("chat stdin lock poisoned") = Some(bridge.stdin.clone());
        *guard = Some(bridge);
    }
    // take the bridge out for the turn: any error path drops (and kills) the
    // child, a clean turn_end puts it back for the next turn
    let mut bridge = guard.take().expect("just spawned");

    let line = serde_json::json!({ "e": "turn", "conv": conv, "messages": messages });
    {
        let mut stdin = bridge.stdin.lock().expect("sidecar stdin lock poisoned");
        if writeln!(stdin, "{line}")
            .and_then(|_| stdin.flush())
            .is_err()
        {
            return Err("could not reach the librarian sidecar".into());
        }
    }

    loop {
        match bridge.lines.next() {
            Some(Ok(line)) => {
                let ev: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                match ev["e"].as_str() {
                    Some("turn_end") => {
                        *guard = Some(bridge);
                        return Ok(());
                    }
                    Some("tool_request") => {
                        let result = execute_tool(
                            &eng,
                            &data,
                            ev["name"].as_str().unwrap_or(""),
                            &ev["args"],
                        );
                        let resp = serde_json::json!({
                            "e": "tool_response", "id": ev["id"], "result": result,
                        });
                        let mut stdin = bridge.stdin.lock().expect("sidecar stdin lock poisoned");
                        if writeln!(stdin, "{resp}")
                            .and_then(|_| stdin.flush())
                            .is_err()
                        {
                            return Err("could not reach the librarian sidecar".into());
                        }
                    }
                    _ => {
                        let _ = app.emit("chat:event", line);
                    }
                }
            }
            _ => return Err("librarian sidecar exited early".into()), // EOF mid-turn
        }
    }
}

#[tauri::command]
pub(crate) async fn chat_turn(
    app: AppHandle,
    conv: String,
    messages: serde_json::Value,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || chat_turn_blocking(app, conv, messages))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) fn chat_cancel(state: State<'_, AppState>) {
    use std::io::Write;
    if let Some(stdin) = state
        .chat_stdin
        .lock()
        .expect("chat stdin lock poisoned")
        .as_ref()
    {
        let mut stdin = stdin.lock().expect("sidecar stdin lock poisoned");
        let _ = writeln!(stdin, "{}", serde_json::json!({ "e": "cancel" }));
        let _ = stdin.flush();
    }
}

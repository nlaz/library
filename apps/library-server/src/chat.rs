//! Chat rides a persistent `librarian serve` sidecar: sessions live in the
//! sidecar (warm model, native AFM transcripts per conversation), the server
//! serializes turns through a Mutex and relays NDJSON events as SSE. Client
//! disconnect mid-turn sends a `cancel` line and drains to `turn_end` so the
//! next turn never reads stale events. A wedged or dead child is dropped and
//! respawned on the next turn.

use std::sync::Arc;

use axum::response::sse::{Event, KeepAlive, Sse};

const CHAT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Collection names for the sidecar's --collections flag, set once in main.
pub(crate) static SIDECAR_COLLECTIONS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

struct Sidecar {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl Sidecar {
    async fn spawn() -> std::io::Result<Sidecar> {
        use tokio::io::AsyncBufReadExt;
        let bin = std::env::var("LIBRARIAN_BIN")
            .unwrap_or_else(|_| "apps/librarian/.build/release/librarian".into());
        let mut child = tokio::process::Command::new(&bin)
            .arg("serve")
            .args(
                SIDECAR_COLLECTIONS
                    .get()
                    .filter(|c| !c.is_empty())
                    .map(|c| vec!["--collections".to_string(), c.clone()])
                    .unwrap_or_default(),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let mut lines =
            tokio::io::BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        // wait for the ready line (model prewarm happens behind it)
        match tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line()).await {
            Ok(Ok(Some(l))) if l.contains("\"ready\"") => {}
            _ => {
                return Err(std::io::Error::other(
                    "librarian sidecar did not become ready",
                ));
            }
        }
        Ok(Sidecar {
            child,
            stdin,
            lines,
        })
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

type SharedSidecar = Arc<tokio::sync::Mutex<Option<Sidecar>>>;

fn sidecar_slot() -> SharedSidecar {
    static SLOT: std::sync::OnceLock<SharedSidecar> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(tokio::sync::Mutex::new(None)))
        .clone()
}

pub(crate) async fn chat(
    body: String,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use tokio::io::AsyncWriteExt;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let err_event = |msg: String| {
        Ok(Event::default()
            .event("error")
            .data(serde_json::json!({ "e": "error", "message": msg }).to_string()))
    };

    tokio::spawn(async move {
        let slot = sidecar_slot();
        let mut guard = slot.lock().await; // serializes turns
        if guard.as_mut().is_none_or(|s| !s.alive()) {
            *guard = match Sidecar::spawn().await {
                Ok(s) => Some(s),
                Err(e) => {
                    let _ = tx
                        .send(err_event(format!("librarian sidecar failed to start: {e}")))
                        .await;
                    return;
                }
            };
        }
        let sidecar = guard.as_mut().expect("just spawned");

        // request line: the turn envelope wrapping the client body
        let req: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let line = serde_json::json!({
            "e": "turn",
            "conv": req.get("conv").and_then(|c| c.as_str()).unwrap_or("default"),
            "messages": req.get("messages").cloned().unwrap_or(serde_json::json!([])),
        });
        if sidecar
            .stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .is_err()
        {
            *guard = None; // dead child; next turn respawns
            let _ = tx
                .send(err_event("could not reach the librarian sidecar".into()))
                .await;
            return;
        }

        let deadline = tokio::time::Instant::now() + CHAT_DEADLINE;
        let mut client_gone = false;
        loop {
            match tokio::time::timeout_at(deadline, sidecar.lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let name = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|v| v.get("e").and_then(|e| e.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "message".into());
                    if name == "turn_end" {
                        return; // turn complete; keep the child for the next one
                    }
                    if !client_gone
                        && tx
                            .send(Ok(Event::default().event(name).data(line)))
                            .await
                            .is_err()
                    {
                        // client disconnected: cancel, then drain to turn_end
                        client_gone = true;
                        let _ = sidecar.stdin.write_all(b"{\"e\":\"cancel\"}\n").await;
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) => {
                    *guard = None; // EOF/read error: child is gone
                    let _ = tx
                        .send(err_event("librarian sidecar exited early".into()))
                        .await;
                    return;
                }
                Err(_) => {
                    *guard = None; // wedged: drop it, kill_on_drop reaps
                    let _ = tx.send(err_event("chat turn timed out".into())).await;
                    return;
                }
            }
        }
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

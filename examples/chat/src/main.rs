//! A chat backend where fold is the source of truth and every update is
//! pushed to browsers over a websocket.
//!
//! The shape to steal for your own demo:
//!
//!   ws clients -> mpsc -> ingest thread (owns the fold Stream) -> watch -> ws clients
//!
//! One plain thread owns the database and does all writes; each write
//! commits a transaction, re-reads one consistent snapshot, and publishes
//! it on a tokio watch channel. Every websocket task just forwards
//! snapshots to its client. No locks, no async database code.

use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::State,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Html,
    routing::get,
};
use fold::pipeline::{Aggregate, KeyBy, terminal};
use fold::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMsg {
    id: u64,
    at_ms: u64,
    author: String,
    body: String,
}

/// What every client sees: the full log plus per-author message counts,
/// both materialized by fold and read from one snapshot.
#[derive(Debug, Clone, Default, Serialize)]
struct ChatState {
    messages: Vec<ChatMsg>,
    author_counts: Vec<(String, i64)>,
}

/// Read one consistent snapshot into a `ChatState`. A macro because the
/// pipeline type (and thus the reader tuple type) contains closures and
/// can't be written out.
macro_rules! snapshot {
    ($st:expr) => {
        $st.rtx(|(_, messages, author_counts)| {
            let mut messages: Vec<ChatMsg> = messages.iter().map(|(m, _)| m).collect();
            messages.sort_by_key(|m| m.id);

            let mut author_counts: Vec<(String, i64)> = author_counts.iter().collect();
            author_counts.sort_by(|a, b| b.1.cmp(&a.1));

            ChatState {
                messages,
                author_counts,
            }
        })
    };
}

fn main() {
    // fresh db per run keeps the demo deterministic; delete this line and
    // chat history survives restarts
    let db_path = std::env::temp_dir().join("the-library-chat.db");
    let _ = std::fs::remove_dir_all(&db_path);

    let (msg_tx, msg_rx) = mpsc::channel::<(String, String)>();
    let (state_tx, state_rx) = watch::channel(ChatState::default());
    std::thread::spawn(move || ingest(&db_path, msg_rx, state_tx));

    serve(msg_tx, state_rx);
}

/// Owns the fold stream: applies incoming messages, republishes snapshots.
fn ingest(
    db_path: &std::path::Path,
    rx: mpsc::Receiver<(String, String)>,
    state_tx: watch::Sender<ChatState>,
) {
    let mut st = Stream::new(
        db_path,
        (
            terminal::Count::new("messages_total"),
            // the durable message log
            terminal::Bag::<ChatMsg>::new("messages"),
            // fold flavor: an incrementally-maintained count per author.
            // Aggregate emits a changelog per key; Table materializes it
            // as the current count per author.
            KeyBy::new(
                |m: &ChatMsg| m.author.clone(),
                Aggregate::new(
                    "by_author",
                    |acc: &mut i64, _m: &ChatMsg, delta| *acc += delta as i64,
                    terminal::Table::new("author_counts"),
                ),
            ),
        ),
    );

    // ids continue from the persisted count, so history-across-restarts
    // works if you remove the cleanup in main
    let next_id = st.rtx(|(total, _, _)| total.get()) as u64;
    let _ = state_tx.send(snapshot!(st));

    for (id, (author, body)) in (next_id..).zip(rx) {
        let msg = ChatMsg {
            id,
            at_ms: now_ms(),
            author,
            body,
        };
        st.wtx(|tx| tx.insert(&msg));
        let _ = state_tx.send(snapshot!(st));
    }
}

#[tokio::main]
async fn serve(msg_tx: mpsc::Sender<(String, String)>, state_rx: watch::Receiver<ChatState>) {
    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_upgrade))
        .with_state((msg_tx, state_rx));

    let port: u16 = std::env::var("CHAT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = format!("0.0.0.0:{port}");
    println!("chat running on http://localhost:{port} (websocket at /ws)");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

type AppState = (mpsc::Sender<(String, String)>, watch::Receiver<ChatState>);

async fn ws_upgrade(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Per-client task: push every new snapshot down, feed every incoming
/// "author: body" line into the ingest thread.
async fn handle_socket(mut socket: WebSocket, (msg_tx, mut state_rx): AppState) {
    let state_json = |s: &ChatState| serde_json::to_string(s).unwrap();

    // greet with current history
    let hello = state_json(&state_rx.borrow_and_update());
    if socket.send(Message::text(hello)).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    return; // ingest thread gone
                }
                let update = state_json(&state_rx.borrow_and_update());
                if socket.send(Message::text(update)).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(Message::Text(line))) = incoming else {
                    return; // client closed or errored
                };
                // "alice: hi there" — anything without a colon is anonymous
                let (author, body) = match line.split_once(':') {
                    Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                    None => ("anon".to_string(), line.trim().to_string()),
                };
                if !body.is_empty() && msg_tx.send((author, body)).is_err() {
                    return;
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>bog chat</title></head>
<body>
<h1>bog chat</h1>
<div style="display:flex; gap:2rem;">
  <div>
    <ul id="log"></ul>
    <input id="name" placeholder="name" size="8" value="anon">
    <input id="text" placeholder="say something" size="40">
    <button id="send">send</button>
  </div>
  <div>
    <h3>messages per author</h3>
    <ul id="counts"></ul>
  </div>
</div>
<script>
const ws = new WebSocket(`ws://${location.host}/ws`);
ws.onmessage = (event) => {
  const state = JSON.parse(event.data);
  const log = document.getElementById("log");
  log.replaceChildren(...state.messages.map((m) => {
    const li = document.createElement("li");
    li.textContent = `${m.author}: ${m.body}`;
    return li;
  }));
  const counts = document.getElementById("counts");
  counts.replaceChildren(...state.author_counts.map(([author, n]) => {
    const li = document.createElement("li");
    li.textContent = `${author}: ${n}`;
    return li;
  }));
};
const send = () => {
  const text = document.getElementById("text");
  if (!text.value) return;
  ws.send(`${document.getElementById("name").value}: ${text.value}`);
  text.value = "";
};
document.getElementById("send").onclick = send;
document.getElementById("text").onkeydown = (e) => { if (e.key === "Enter") send(); };
</script>
</body>
</html>"#,
    )
}

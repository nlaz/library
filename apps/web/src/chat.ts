// The librarian chat panel: a collapsible right-side panel toggled from the
// header. Talks to POST /api/chat (SSE over fetch — EventSource can't POST),
// which relays the on-device model's token deltas and tool activity from a
// persistent sidecar. A `conv` id keys the sidecar's per-conversation model
// session; aborting the fetch cancels generation server-side.
//
// Citation chips come from tool events, not the model's prose — a 3B model
// mangles long doc ids, but the search hits it saw are exact. Prose
// citations ([doc-id p.N]) are additionally linkified against the docs seen
// in this turn's tool events.

type Chip = { doc: string; title: string | null; page: number };

type ChatEvent =
  | { e: "token"; text: string; replace?: boolean }
  | {
      e: "tool";
      name: string;
      status: "started" | "done";
      args?: Record<string, unknown>;
      summary?: string;
      hits?: Chip[];
    }
  | { e: "done"; content: string; ms: number }
  | { e: "cancelled" }
  | { e: "error"; message: string };

type Opts = {
  prettify(id: string): string;
  /** Desktop transport: chat rides Tauri invoke + events instead of SSE. */
  desktop?: {
    turn(
      conv: string,
      messages: { role: string; content: string }[],
      onEvent: (ev: unknown) => void,
    ): Promise<void>;
    cancel(): void;
  } | null;
};

const $panel = document.getElementById("chat")!;
const $toggle = document.getElementById("chat-toggle")!;
const $close = document.getElementById("chat-close")!;
const $stop = document.getElementById("chat-stop") as HTMLButtonElement;
const $clear = document.getElementById("chat-clear") as HTMLButtonElement;
const $log = document.getElementById("chat-log")!;
const $form = document.getElementById("chat-form") as HTMLFormElement;
const $input = document.getElementById("chat-input") as HTMLInputElement;

let opts: Opts = { prettify: (id) => id };
// full transcript for display + persistence; only the tail is sent (the
// sidecar session carries history natively — the tail only reseeds after
// a server-side eviction)
let transcript: { role: "user" | "assistant"; content: string }[] = [];
const SEND_LAST = 6;
let streaming = false;
let abort: AbortController | null = null;
// docs seen in this turn's tool events, for prose-citation linkify
let turnDocs = new Map<string, Chip>();

const STORE_KEY = "librarian-chat";
// per-tab conversation id, stable across reloads within the tab
const conv: string = (() => {
  const k = "librarian-conv";
  let v = sessionStorage.getItem(k);
  if (!v) {
    v = crypto.randomUUID();
    sessionStorage.setItem(k, v);
  }
  return v;
})();

export function initChat(o: Opts) {
  opts = o;
  $toggle.setAttribute("aria-expanded", "false");
  $toggle.addEventListener("click", () => {
    $panel.hidden = !$panel.hidden;
    $toggle.setAttribute("aria-expanded", String(!$panel.hidden));
    if (!$panel.hidden) {
      if (!$log.childElementCount) greeting();
      $input.focus();
    }
  });
  $close.addEventListener("click", () => close());
  $stop.addEventListener("click", () => {
    if (opts.desktop) opts.desktop.cancel();
    else abort?.abort();
  });
  $clear.addEventListener("click", () => {
    transcript = [];
    sessionStorage.removeItem(STORE_KEY);
    $log.replaceChildren();
    greeting();
  });
  $input.addEventListener("keydown", (e) => {
    e.stopPropagation(); // reader/search hotkeys must not fire while typing
    if (e.key === "Escape") close();
  });
  $form.addEventListener("submit", (e) => {
    e.preventDefault();
    const q = $input.value.trim();
    if (!q || streaming) return;
    $input.value = "";
    send(q);
  });

  // restore the conversation after a reload
  try {
    const saved = JSON.parse(sessionStorage.getItem(STORE_KEY) ?? "[]");
    if (Array.isArray(saved) && saved.length) {
      transcript = saved;
      for (const m of transcript) row(m.role).textContent = m.content;
    }
  } catch {
    // corrupt state — start fresh
  }
}

function close() {
  $panel.hidden = true;
  $toggle.setAttribute("aria-expanded", "false");
}

function persist() {
  sessionStorage.setItem(STORE_KEY, JSON.stringify(transcript.slice(-24)));
}

function greeting() {
  const r = document.createElement("div");
  r.className = "cmsg tool";
  r.textContent = "ask about your books — answers are searched, read, and cited";
  $log.append(r);
}

async function send(q: string) {
  transcript.push({ role: "user", content: q });
  persist();
  row("user").textContent = q;
  streaming = true;
  turnDocs = new Map();
  $input.disabled = true;
  $input.placeholder = "thinking…";
  $stop.hidden = false;
  abort = new AbortController();

  // created lazily so tool-activity rows land above the streamed answer
  const a: { el: HTMLElement | null } = { el: null };
  const assistant = () => (a.el ??= row("assistant"));
  let acc = "";
  const handle = (ev: ChatEvent) => {
    switch (ev.e) {
      case "token":
        acc = ev.replace ? ev.text : acc + ev.text;
        assistant().textContent = acc;
        follow();
        break;
      case "tool":
        toolRow(ev);
        follow();
        break;
      case "done":
        acc = ev.content;
        renderFinal(assistant(), acc);
        break;
      case "cancelled":
        if (acc) renderFinal(assistant(), acc);
        break;
      case "error":
        errorRow(ev.message);
        break;
    }
  };
  try {
    const messages = transcript.slice(-SEND_LAST);
    if (opts.desktop) {
      await opts.desktop.turn(conv, messages, (ev) => handle(ev as ChatEvent));
    } else {
      const res = await fetch("/api/chat", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ conv, messages }),
        signal: abort.signal,
      });
      if (!res.ok || !res.body) throw new Error(`chat: ${res.status}`);
      for await (const ev of sse(res.body)) handle(ev);
    }
    if (acc) transcript.push({ role: "assistant", content: acc });
    else a.el?.remove();
    persist();
  } catch (err) {
    if (abort?.signal.aborted) {
      // stopped by the user: keep whatever streamed
      if (acc) {
        renderFinal(assistant(), acc);
        transcript.push({ role: "assistant", content: acc });
        persist();
      } else a.el?.remove();
    } else {
      a.el?.remove();
      errorRow(`${err instanceof Error ? err.message : err}`);
    }
  } finally {
    streaming = false;
    abort = null;
    $stop.hidden = true;
    $input.disabled = false;
    $input.placeholder = "ask the library…";
    $input.focus();
    follow();
  }
}

/** Parse an SSE byte stream into chat events (data: lines carry the JSON). */
async function* sse(body: ReadableStream<Uint8Array>): AsyncGenerator<ChatEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let sep: number;
    while ((sep = buf.indexOf("\n\n")) >= 0) {
      const frame = buf.slice(0, sep);
      buf = buf.slice(sep + 2);
      for (const line of frame.split("\n")) {
        if (!line.startsWith("data:")) continue;
        try {
          yield JSON.parse(line.slice(5)) as ChatEvent;
        } catch {
          // keep-alive or malformed frame — skip
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

function row(kind: "user" | "assistant"): HTMLElement {
  const r = document.createElement("div");
  r.className = `cmsg ${kind}`;
  $log.append(r);
  follow();
  return r;
}

/** Final assistant text: linkify [doc-id p.N] prose citations into chips
 * for docs this turn's tools actually saw. */
function renderFinal(el: HTMLElement, text: string) {
  el.replaceChildren();
  const re = /\[([^\[\]]+?)\s+p\.?\s*(\d+)\]/g;
  let last = 0;
  for (const m of text.matchAll(re)) {
    const [full, rawDoc, page] = m;
    const hit = resolveDoc(rawDoc.trim());
    if (!hit) continue;
    el.append(text.slice(last, m.index));
    el.append(chip({ doc: hit.doc, title: hit.title, page: Number(page) }));
    last = m.index + full.length;
  }
  el.append(text.slice(last));
}

/** Match a prose citation's doc reference against docs seen in tool events. */
function resolveDoc(ref: string): Chip | null {
  const norm = (s: string) => s.toLowerCase().replace(/[^a-z0-9]+/g, "");
  const n = norm(ref);
  if (!n) return null;
  for (const [doc, c] of turnDocs) {
    if (norm(doc).includes(n) || n.includes(norm(doc))) return c;
    if (c.title && (norm(c.title).includes(n) || n.includes(norm(c.title)))) return c;
  }
  return null;
}

/** started: activity line; done: replace it, then chips for the hits. */
let pendingTool: HTMLElement | null = null;

function toolRow(ev: Extract<ChatEvent, { e: "tool" }>) {
  if (ev.status === "started") {
    pendingTool = document.createElement("div");
    pendingTool.className = "cmsg tool";
    const q = ev.args?.query ?? ev.args?.doc ?? "";
    pendingTool.textContent =
      ev.name === "read_pages" ? `reading ${q} p.${ev.args?.from}…` : `searching ${q ? `"${q}"` : ""}…`;
    $log.append(pendingTool);
    return;
  }
  const r = pendingTool ?? document.createElement("div");
  if (!pendingTool) {
    r.className = "cmsg tool";
    $log.append(r);
  }
  pendingTool = null;
  r.textContent = ev.summary ?? ev.name;
  const seen = new Set<string>();
  for (const hit of ev.hits ?? []) {
    turnDocs.set(hit.doc, hit);
    const key = `${hit.doc}|${hit.page}`;
    if (seen.has(key)) continue;
    seen.add(key);
    r.append(chip(hit));
  }
}

function chip(hit: Chip): HTMLElement {
  const b = document.createElement("button");
  b.className = "cchip";
  b.textContent = `${hit.title ?? opts.prettify(hit.doc)} · p.${hit.page}`;
  b.title = hit.doc;
  // the panel overlays the reader (higher z-index), so it stays open
  b.addEventListener("click", () => {
    location.hash = `#/read/${encodeURIComponent(hit.doc)}?p=${hit.page}`;
  });
  return b;
}

function errorRow(msg: string) {
  const r = document.createElement("div");
  r.className = "cmsg cerr";
  r.textContent = msg;
  $log.append(r);
}

function follow() {
  $log.scrollTop = $log.scrollHeight;
}

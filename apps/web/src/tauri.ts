// Desktop transport + desktop-only features (browse, ingest, drag & drop).
// Only imported when running inside Tauri — keep every @tauri-apps import in
// this module so the plain web build never touches them.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { confirm as confirmDialog, open as openDialog } from "@tauri-apps/plugin-dialog";
import type { Transport } from "./transport";
import type {
  AtlasResponse,
  CardRec,
  Collections,
  DocInfo,
  IngestEvent,
  NewCard,
  QueryMsg,
  StartupStatus,
  WireResponse,
} from "./types";

export class TauriTransport implements Transport {
  private cb: (msg: WireResponse) => void = () => {};

  async ready(): Promise<void> {
    if (await invoke<boolean>("ready")) return;
    await new Promise<void>((resolve) => {
      // subscribe first, then re-check, so the event can't slip between
      let un: (() => void) | undefined;
      listen("app:ready", () => {
        un?.();
        resolve();
      }).then((u) => {
        un = u;
        invoke<boolean>("ready").then((ok) => {
          if (ok) {
            un?.();
            resolve();
          }
        });
      });
    });
  }

  send(q: QueryMsg): void {
    invoke<WireResponse>("search", { query: q })
      .then((msg) => this.cb(msg))
      .catch(() => {}); // "warming up" — the next keystroke retries
  }

  onResponse(cb: (msg: WireResponse) => void): void {
    this.cb = cb;
  }

  complete(prefix: string): Promise<string[]> {
    return invoke<string[]>("complete", { prefix });
  }

  collections(): Promise<Collections> {
    return invoke<Collections>("collections");
  }
}

export function docs(): Promise<DocInfo[]> {
  return invoke<DocInfo[]>("docs");
}

/** What an add did, per file — one bad file no longer fails the batch. */
export type AddResult = {
  queued: string[];
  duplicates: number;
  skipped: string[];
  failed: [name: string, why: string][];
};

export function ingestPaths(paths: string[], collection: string | null): Promise<AddResult> {
  return invoke<AddResult>("ingest_paths", { paths, collection });
}

/** Native chooser for adding books. Folders are allowed and expand to their
 * contents, because dropping a folder of scans is the obvious thing to try
 * and the picker should accept whatever the drop handler does. */
export async function pickFiles(): Promise<string[]> {
  const picked = await openDialog({
    multiple: true,
    title: "Add to library",
    filters: [{ name: "Documents and scans", extensions: ["pdf", "png", "jpg", "jpeg", "heic"] }],
  });
  if (!picked) return [];
  return Array.isArray(picked) ? picked : [picked];
}

/** Choose a folder to watch, or to keep the library in. The native panel is
 * also what grants us access to the location, so this is the only way a
 * folder can be linked. */
export async function pickFolder(title: string): Promise<string | null> {
  const picked = await openDialog({ directory: true, multiple: false, title });
  return typeof picked === "string" ? picked : null;
}

export type AppSettings = { data: string; width: number };

export function getSettings(): Promise<AppSettings> {
  return invoke<AppSettings>("get_settings");
}

/** Set (empty string clears) a doc's display title. */
export function setTitle(doc: string, title: string): Promise<void> {
  return invoke("set_title", { doc, title });
}

/** Replace a doc's collection membership (empty list = none). */
export function setCollections(doc: string, collections: string[]): Promise<void> {
  return invoke("set_collections", { doc, collections });
}

/** Remove a doc from the library (its source file in data/pdfs is kept). */
export function deleteDoc(doc: string): Promise<void> {
  return invoke("delete_doc", { doc });
}

/** Select a doc's original file in Finder (adding a book moved it into the
 * library folder, so this is the way back to it). */
export function revealDoc(doc: string): Promise<void> {
  return invoke("reveal_doc", { doc });
}

/** Re-queue a doc whose ingest failed. */
export function retryDoc(doc: string): Promise<void> {
  return invoke("retry_doc", { doc });
}

export function confirmDelete(title: string): Promise<boolean> {
  return confirmDialog(
    `Remove “${title}” from the library? Its pages and search entries are deleted; the original file is kept.`,
    { title: "Delete document", kind: "warning" },
  );
}

/** One librarian chat turn: events stream via `chat:event`, the invoke
 * resolves at turn end. Payloads are the sidecar's NDJSON lines. */
export async function chatTurn(
  conv: string,
  messages: { role: string; content: string }[],
  onEvent: (ev: unknown) => void,
): Promise<void> {
  const un = await listen<string>("chat:event", (e) => {
    try {
      onEvent(JSON.parse(e.payload));
    } catch {
      // malformed line — skip
    }
  });
  try {
    await invoke("chat_turn", { conv, messages });
  } finally {
    un();
  }
}

/** Cancel the active chat turn (the sidecar stops between snapshots). */
export function chatCancel(): void {
  invoke("chat_cancel").catch(() => {});
}

/** Whether Apple Foundation Models can answer on this Mac. Cached in Rust;
 * an unavailable answer hides the librarian rather than failing at it. */
export function chatStatus(): Promise<{ available: boolean; reason: string | null }> {
  return invoke<{ available: boolean; reason: string | null }>("chat_status");
}

// --- marginalia: note-box cards ---------------------------------------------

export function listCards(): Promise<CardRec[]> {
  return invoke<CardRec[]>("list_cards");
}

export function createCard(input: NewCard): Promise<CardRec> {
  return invoke<CardRec>("create_card", { input });
}

export function updateCard(card: CardRec): Promise<CardRec> {
  return invoke<CardRec>("update_card", { card });
}

// --- corpus atlas ------------------------------------------------------------

export function atlas(refresh?: boolean): Promise<AtlasResponse> {
  return invoke<AtlasResponse>("atlas", { refresh });
}

export function onIngestProgress(cb: (e: IngestEvent) => void): void {
  listen<IngestEvent>("ingest:progress", (e) => cb(e.payload));
}

export function onAppError(cb: (msg: string) => void): void {
  listen<string>("app:error", (e) => cb(e.payload));
}

/** The latched launch-screen status, for the subscribe-then-recheck that
 * makes a startup finishing before the webview boots survivable. */
export function startupStatus(): Promise<StartupStatus> {
  return invoke<StartupStatus>("startup_status");
}

export function onStartupStatus(cb: (s: StartupStatus) => void): void {
  listen<StartupStatus>("app:status", (e) => cb(e.payload));
}

/** Engine start is stalled (e.g. the background indexer is mid-commit). */
export function onAppWaiting(cb: (msg: string) => void): void {
  listen<string>("app:waiting", (e) => cb(e.payload));
}

/** Native file drop. `over` fires on enter/hover, `leave` on exit/drop. */
export function onDragDrop(
  over: () => void,
  leave: () => void,
  drop: (paths: string[]) => void,
): void {
  getCurrentWebview().onDragDropEvent((e) => {
    if (e.payload.type === "enter" || e.payload.type === "over") over();
    else if (e.payload.type === "leave") leave();
    else if (e.payload.type === "drop") {
      leave();
      drop(e.payload.paths);
    }
  });
}

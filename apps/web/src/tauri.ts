// Desktop transport + desktop-only features (browse, ingest, drag & drop).
// Only imported when running inside Tauri — keep every @tauri-apps import in
// this module so the plain web build never touches them.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { confirm as confirmDialog } from "@tauri-apps/plugin-dialog";
import type { Transport } from "./transport";
import type { Collections, DocInfo, IngestEvent, QueryMsg, WireResponse } from "./types";

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

export function ingestPaths(
  paths: string[],
  collection: string | null,
  mode: "move" | "copy" = "copy",
): Promise<string[]> {
  return invoke<string[]>("ingest_paths", { paths, collection, mode });
}

/** Ask before relocating files into the library folder. */
export function confirmMove(names: string[], destDir: string): Promise<boolean> {
  const what =
    names.length === 1 ? `“${names[0]}”` : `${names.length} files`;
  return confirmDialog(
    `This will move ${what} into your library folder (${destDir}). Move ${names.length === 1 ? "it" : "them"}?`,
    { title: "Add to library", kind: "info" },
  );
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

export function onIngestProgress(cb: (e: IngestEvent) => void): void {
  listen<IngestEvent>("ingest:progress", (e) => cb(e.payload));
}

export function onAppError(cb: (msg: string) => void): void {
  listen<string>("app:error", (e) => cb(e.payload));
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

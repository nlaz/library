// Desktop transport + desktop-only features (browse, ingest, drag & drop).
// Only imported when running inside Tauri — keep every @tauri-apps import in
// this module so the plain web build never touches them.

import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { homeDir as homeDirPath } from "@tauri-apps/api/path";
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

// --- watched folders ---------------------------------------------------------

export type RootInfo = {
  id: string;
  path: string;
  is_default: boolean;
  state: string;
  added_at: number;
  last_scan_at: number;
  docs: number;
  available: boolean;
};

export function listRoots(): Promise<RootInfo[]> {
  return invoke<RootInfo[]>("list_roots");
}

export function linkRoot(path: string): Promise<RootInfo> {
  return invoke<RootInfo>("link_root", { path });
}

export function unlinkRoot(id: string): Promise<void> {
  return invoke("unlink_root", { id });
}

export function setDefaultRoot(id: string): Promise<void> {
  return invoke("set_default_root", { id });
}

export function storageUse(): Promise<{
  path: string;
  derived_bytes: number;
  index_bytes: number;
  model_bytes: number;
  page_bytes: number;
  page_budget_bytes: number;
  pinned_bytes: number;
}> {
  return invoke("storage_use");
}

/** Run the page-cache eviction sweep now; resolves with bytes freed. */
export function sweepCache(): Promise<number> {
  return invoke<number>("sweep_cache");
}

/** The user's home dir, so paths can be shown as `~/...`. */
export function homeDir(): Promise<string> {
  return homeDirPath();
}

/** Unlinking is the one destructive-looking action on the Settings page,
 * and the copy's job is to say the files are safe. */
export function confirmUnlink(message: string): Promise<boolean> {
  return confirmDialog(message, { title: "Remove folder", kind: "warning" });
}

export type AppSettings = { data: string; width: number };

export function getSettings(): Promise<AppSettings> {
  return invoke<AppSettings>("get_settings");
}

// --- preferences -------------------------------------------------------------
// meta.db's key-value settings table, not settings.json. The distinction is
// load-bearing: settings.json is bootstrap (where the data lives) and is
// parsed before the app can open anything; these are choices a running app
// changes about itself.
//
// The table stores strings. Booleans live here, in the one place that both
// writes and reads them, rather than as a convention Rust also has to know.

function getPref(key: string): Promise<string | null> {
  return invoke<string | null>("get_pref", { key });
}

export function setPref(key: string, value: string): Promise<void> {
  return invoke("set_pref", { key, value });
}

/** A stored flag. Anything but "1" is false — including the nothing a
 * fresh library has, and including a value we didn't write. Something
 * unreadable must read as off rather than as an error. */
export function getFlag(key: string): Promise<boolean> {
  return getPref(key).then((v) => v === "1");
}

export function setFlag(key: string, on: boolean): Promise<void> {
  return setPref(key, on ? "1" : "0");
}

/** Whether the librarian is offered. Unset reads as off — the chat panel
 * is something you ask for, not something you dismiss. */
export const CHAT_ENABLED = "chat.enabled";

// --- updates -----------------------------------------------------------------
// Checks are manual: nothing below runs unless someone presses the button in
// Settings. The work happens in Rust (update.rs), which is why none of the
// updater plugin's own commands are reachable from here.

export type Release = { version: string; notes: string | null };

/** False in a dev build, which runs from target/debug and has no bundle to
 * replace. */
export function updatesSupported(): Promise<boolean> {
  return invoke<boolean>("updates_supported");
}

/** Null when this is already the newest build. Rejects if the question
 * couldn't be asked — "up to date" must never stand in for "no answer". */
export function checkUpdate(): Promise<Release | null> {
  return invoke<Release | null>("check_update");
}

/** Download, verify and swap the bundle in place. Does not restart. */
export function installUpdate(): Promise<void> {
  return invoke("install_update");
}

export type UpdateProgress = { downloaded: number; total: number | null };

export function onUpdateProgress(cb: (p: UpdateProgress) => void): void {
  listen<UpdateProgress>("update:progress", (e) => cb(e.payload));
}

/** Quit and come back as the version just installed. */
export function restartApp(): void {
  invoke("restart_app").catch(() => {});
}

/** The running bundle's version, from tauri.conf.json by way of Info.plist. */
export function appVersion(): Promise<string> {
  return getVersion();
}

/** Set (empty string clears) a doc's display title. */
export function setTitle(doc: string, title: string): Promise<void> {
  return invoke("set_title", { doc, title });
}

/** Put a document on a shelf by moving its file into that folder. An empty
 * shelf moves it back to the top level of its watched folder. */
export function moveToShelf(doc: string, shelf: string): Promise<void> {
  return invoke("move_to_shelf", { doc, shelf });
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

/** Fired once, the first time the page-image cache is over its budget —
 * before anything is removed. Payload is the bytes the sweep would free. */
export function onCacheAnnounce(cb: (bytes: number) => void): void {
  listen<number>("cache:announce", (e) => cb(e.payload));
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

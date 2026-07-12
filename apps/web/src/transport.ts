// The one seam between the two hosts: the web build talks WebTransport to
// library-server, the desktop build calls Tauri commands in-process. Query
// dispatch and response handling are identical above this line.

import type { Collections, QueryMsg, WireResponse } from "./types";

export interface Transport {
  ready(): Promise<void>;
  /** Fire-and-forget; responses arrive via onResponse, possibly out of order. */
  send(q: QueryMsg): void;
  onResponse(cb: (msg: WireResponse) => void): void;
  collections(): Promise<Collections>;
}

export function isTauri(): boolean {
  return "__TAURI_INTERNALS__" in window;
}

export async function makeTransport(): Promise<Transport> {
  if (isTauri()) {
    const { TauriTransport } = await import("./tauri");
    return new TauriTransport();
  }
  const { WtTransport } = await import("./webtransport");
  return new WtTransport();
}

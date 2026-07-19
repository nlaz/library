// App-level handles shared by every module: set exactly once during startup
// (main.ts), read everywhere else via ESM live bindings. This is the single
// owner — no other module keeps its own copy.

import type { Transport } from "./transport";

export let transport: Transport;
export let desktop: typeof import("./tauri") | null = null;

export function setTransport(t: Transport) {
  transport = t;
}

export function setDesktop(d: typeof import("./tauri")) {
  desktop = d;
}

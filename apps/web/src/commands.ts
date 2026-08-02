// The command registry: every verb the card catalog can run.
//
// Nothing here implements anything. Each `run` calls the same function the
// button or the chord calls, so a row and its shortcut cannot drift apart —
// if they ever disagree, the row is the one that is wrong.
//
// `when` is how a command stays honest about where it applies: the web build
// has no filesystem, so the desktop-only commands are absent there rather
// than present and failing, the same way the drawer takes `edit: desktop ?
// … : null`. A view that is already open also drops its own command, because
// the catalog paints over it and "Performance" silently closing the perf
// view behind the scrim is not a thing anyone asked for.
//
// The list is built per call, not held at module scope: `when` depends on
// `desktop`, which is set asynchronously during startup, and on which
// surface is open right now.

import { atlasOpen, toggleAtlas } from "./atlas";
import { pickAndQueue } from "./ingest-ui";
import { keysOpen, toggleKeys } from "./keys";
import { notesOpen } from "./notebox";
import { perfOpen, togglePerf } from "./perf";
import { readerOpen } from "./reader";
import { openSettings, settingsOpen } from "./settings";
import { startCreate } from "./sheet";
import { desktop } from "./state";
import { openSearchPop } from "./viewer";

export type Cmd = {
  id: string;
  label: string;
  group: string;
  /** Rendered as a keycap on the row. Every chord here must also appear in
   * the shortcut sheet; keys-dom.test.ts is what makes that true. */
  chord?: string;
  /** False on a host that can never run this. Absent = every host.
   *
   * Kept apart from `when` because the two questions have different answers
   * for different readers: the catalog asks "can this be run right now", the
   * shortcut sheet asks "does this key exist on this machine at all". A view
   * that happens to be open should still be documented. */
  host?: () => boolean;
  /** False when the command does not apply at this moment. Absent = always. */
  when?: () => boolean;
  run: () => void | Promise<void>;
};

/** Navigate as a new leg of the trail — the catalog's book and note rows go
 * through here too, so that Escape from where they land returns to where the
 * catalog was opened. Guarded, because assigning the hash you are already on
 * fires no event and would strand a caller waiting for one. */
export function go(hash: string) {
  if ((location.hash || "#/") === hash) return;
  location.hash = hash;
}

/** Every command this build has, whether or not it applies at this moment —
 * the shortcut sheet's view, and the list the drift test checks. */
export function allCommands(): Cmd[] {
  const all: Cmd[] = [
    {
      id: "search",
      label: "Search the library",
      group: "commands",
      chord: "⌘F",
      run: openSearchPop,
    },
    {
      id: "note-new",
      label: "Start a note",
      group: "commands",
      chord: "c",
      run: () => startCreate(),
    },
    {
      id: "notes",
      label: "Notes",
      group: "commands",
      when: () => !notesOpen(),
      run: () => go("#/notes"),
    },
    {
      id: "add",
      label: "Add books…",
      group: "commands",
      chord: "⌘O",
      host: () => !!desktop,
      run: () => void pickAndQueue(),
    },
    {
      id: "library",
      label: "Back to the library",
      group: "commands",
      when: () => readerOpen() || notesOpen(),
      run: () => go("#/"),
    },
    {
      id: "settings",
      label: "Settings",
      group: "commands",
      chord: "⌘S",
      host: () => !!desktop,
      when: () => !settingsOpen(),
      run: () => void openSettings(),
    },
    {
      id: "atlas",
      label: "Corpus atlas",
      group: "commands",
      chord: "⌘/",
      when: () => !atlasOpen(),
      run: toggleAtlas,
    },
    {
      id: "perf",
      label: "Performance",
      group: "commands",
      chord: "⌘.",
      when: () => !perfOpen(),
      run: togglePerf,
    },
    {
      id: "keys",
      label: "Keyboard shortcuts",
      group: "commands",
      chord: "?",
      when: () => !keysOpen(),
      run: toggleKeys,
    },
  ];
  return all.filter((c) => !c.host || c.host());
}

/** Every command that applies right now, in the order an empty catalog
 * should show them. */
export function commands(): Cmd[] {
  return allCommands().filter((c) => !c.when || c.when());
}

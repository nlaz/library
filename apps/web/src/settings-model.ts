// ---------------------------------------------------------------------------
// The Settings view-model: what the folder list says, with no DOM in sight.
//
// The interesting decisions here are all about *not lying*. A folder can be
// linked and unreadable at the same time (an ejected drive), and the list is
// the only place that difference is visible — so "0 documents" and "we can't
// see it right now" must never render the same way.
// ---------------------------------------------------------------------------

export type RootInfo = {
  id: string;
  path: string;
  is_default: boolean;
  /** "watching" | "unavailable", as of the last scan. */
  state: string;
  added_at: number;
  last_scan_at: number;
  docs: number;
  /** Readable *now* — checked when the list was built, not last scan. */
  available: boolean;
};

export type RootRow = {
  id: string;
  /** `~/The Library`, not the full absolute path — the home prefix is noise. */
  label: string;
  /** The folder's own name, for the compact line. */
  name: string;
  isDefault: boolean;
  /** Present and readable. */
  ok: boolean;
  /** One clause under the path. Never empty: silence would read as fine. */
  status: string;
  /** Whether unlinking is offered. The last folder can't be unlinked — the
   * library would have nowhere to accept a drop. */
  canUnlink: boolean;
};

/** Strip `$HOME` back to `~`, the way every Mac app writes a path. */
export function tildify(path: string, home: string): string {
  if (home && path === home) return "~";
  if (home && path.startsWith(home.endsWith("/") ? home : `${home}/`)) {
    return `~/${path.slice(home.length).replace(/^\/+/, "")}`;
  }
  return path;
}

export function baseName(path: string): string {
  const parts = path.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || path;
}

/** Plural-aware document count. */
function docCount(n: number): string {
  return n === 1 ? "1 document" : `${n.toLocaleString()} documents`;
}

export function rootRows(roots: RootInfo[], home: string): RootRow[] {
  const canUnlink = roots.length > 1;
  return roots.map((r) => ({
    id: r.id,
    label: tildify(r.path, home),
    name: baseName(r.path),
    isDefault: r.is_default,
    ok: r.available,
    status: !r.available
      ? // the count is what we last knew, not what is there — say so
        `Can't see this folder right now · ${docCount(r.docs)} kept`
      : r.docs === 0
        ? "No documents yet"
        : docCount(r.docs),
    canUnlink,
  }));
}

/** Human bytes. Deliberately coarse: nobody needs 15.23 GB. */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0 MB";
  const gb = n / 1e9;
  if (gb >= 1) return `${gb.toFixed(gb >= 10 ? 0 : 1)} GB`;
  return `${Math.max(1, Math.round(n / 1e6))} MB`;
}

export type Storage = {
  path: string;
  derived_bytes: number;
  index_bytes: number;
  model_bytes: number;
};

/** The storage lines, in the order they're shown. Page renders dominate by
 * an order of magnitude, so they go first and are named for what they are. */
export function storageRows(s: Storage): [label: string, value: string][] {
  return [
    ["Page images and text", formatBytes(s.derived_bytes)],
    ["Search indexes", formatBytes(s.index_bytes)],
    ["Models", formatBytes(s.model_bytes)],
  ];
}

/** The sentence under the unlink button. Unlinking is the scariest thing on
 * this page and the copy has one job: say that the files are safe. */
export function unlinkWarning(row: RootRow): string {
  return `Remove “${row.name}” from the library? Your files stay exactly where they are — the app forgets its page images and search entries, and your notes are kept.`;
}

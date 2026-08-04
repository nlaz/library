// ---------------------------------------------------------------------------
// The Settings view-model: what the page says, with no DOM in sight.
//
// The interesting decisions here are all about *not lying*. A folder can be
// linked and unreadable at the same time (an ejected drive), and the list is
// the only place that difference is visible — so "0 documents" and "we can't
// see it right now" must never render the same way.
// ---------------------------------------------------------------------------

export type SectionId = "folders" | "storage" | "librarian" | "about";

/** The page, in the order it is read — and so also the rail, top to bottom. */
export const SECTIONS: { id: SectionId; label: string }[] = [
  { id: "folders", label: "Folders" },
  { id: "storage", label: "Storage" },
  { id: "librarian", label: "Librarian" },
  { id: "about", label: "About" },
];

/** How far down the viewport a heading has to climb before the rail calls
 * its section the one you are reading. */
const READING_LINE = 80;

/** Which rail entry is lit. `offsets` are the sections' distances from the
 * top of the scroller, in order; `scroll` describes the scroller itself.
 *
 * The second rule is the one that matters: normally the lit section is the
 * last whose heading has passed the reading line, but at the very bottom of
 * the scroll it is always the last section. Without that a final section
 * shorter than the viewport can never be reached, and the rail's last entry
 * is decoration. */
export function activeSection(
  scrollTop: number,
  offsets: number[],
  scroll?: { height: number; total: number },
): number {
  if (offsets.length <= 1) return 0;
  if (scroll && scroll.total > scroll.height && scrollTop + scroll.height >= scroll.total - 1) {
    return offsets.length - 1;
  }
  const line = scrollTop + READING_LINE;
  let i = 0;
  while (i + 1 < offsets.length && offsets[i + 1] <= line) i++;
  return i;
}

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
  /** Page renders — the part with a budget. */
  page_bytes?: number;
  /** 0 means no limit. */
  page_budget_bytes?: number;
  /** Renders of documents whose file is gone: not a cache, the only copy. */
  pinned_bytes?: number;
};

/** The storage lines, in the order they're shown. Page renders dominate by
 * an order of magnitude, so they go first and are named for what they are.
 *
 * The pinned row appears only when it is non-zero, and it exists because
 * the pane's closing sentence — everything here can be rebuilt from your
 * files — is true of every other line and false of that one. Folding those
 * bytes into a total that offers to free them would be a lie about the one
 * thing on this screen that cannot be undone. */
export function storageRows(s: Storage): [label: string, value: string][] {
  const rows: [string, string][] = [];
  if (s.page_bytes !== undefined) {
    const budget = s.page_budget_bytes ?? 0;
    rows.push([
      "Page images",
      budget > 0
        ? `${formatBytes(s.page_bytes)} of ${formatBytes(budget)}`
        : `${formatBytes(s.page_bytes)} (no limit)`,
    ]);
    if (s.pinned_bytes) {
      rows.push(["Kept for missing files", formatBytes(s.pinned_bytes)]);
    }
    // whatever `derived` holds beyond the renders: OCR, markdown, edits
    rows.push(["Text and notes", formatBytes(s.derived_bytes - s.page_bytes)]);
  } else {
    rows.push(["Page images and text", formatBytes(s.derived_bytes)]);
  }
  rows.push(["Search indexes", formatBytes(s.index_bytes)]);
  rows.push(["Models", formatBytes(s.model_bytes)]);
  return rows;
}

/** Budget choices, in bytes. 0 is "no limit".
 *
 * Decimal GB, matching `formatBytes` and — load-bearing — matching the
 * default in `cache.rs`. If the default is not one of these values the
 * `<select>` falls back to its first option, so the picker would sit there
 * confidently displaying a limit that is not the one in force. */
export const BUDGET_CHOICES: [label: string, bytes: number][] = [
  ["1 GB", 1e9],
  ["2 GB", 2e9],
  ["4 GB", 4e9],
  ["10 GB", 10e9],
  ["No limit", 0],
];

/** The default in `cache.rs`. Mirrored so the picker can be checked
 * against it — see the test. */
export const DEFAULT_BUDGET = 2e9;

/** The sentence under the pinned row. Its whole job is to say that these
 * bytes are not spare — and that the app will not touch them on its own. */
export const PINNED_NOTE =
  "These documents' files are no longer where the library last saw them, so their page images are the only copy. The Library never removes those on its own.";

/** The sentence under the unlink button. Unlinking is the scariest thing on
 * this page and the copy has one job: say that the files are safe. */
export function unlinkWarning(row: RootRow): string {
  return `Remove “${row.name}” from the library? Your files stay exactly where they are — the app forgets its page images and search entries, and your notes are kept.`;
}

// --- the librarian ----------------------------------------------------------

export type LibrarianRow = {
  /** Whether the toggle reads as on. Never true where it can't be run. */
  on: boolean;
  /** Whether it can be pressed at all. */
  can: boolean;
  /** What the button says. */
  label: string;
  /** The line under the description, or null when there is nothing to
   * explain. */
  warn: string | null;
};

/** Said when the probe reports no librarian but declines to say why —
 * a Mac that can't run it and offers no reason is still owed a sentence. */
const NO_LIBRARIAN =
  "Not on this Mac. The librarian needs macOS 26 and Apple Intelligence; everything else in the library works here.";

/** The librarian row. Two independent facts collapse into one control: can
 * this Mac run it, and has it been asked for. The toggle is drawn either
 * way — a machine that can't is told what it is missing rather than shown
 * a gap where a setting is on someone else's screen. */
export function librarianRow(s: {
  supported: boolean;
  reason: string | null;
  enabled: boolean;
}): LibrarianRow {
  // an unsupported Mac reads as off whatever the stored preference says:
  // the panel is not going to open, and a switch left on would be a lie
  const on = s.supported && s.enabled;
  return {
    on,
    can: s.supported,
    label: on ? "on" : "off",
    warn: s.supported ? null : (s.reason ?? NO_LIBRARIAN),
  };
}

// --- updates ----------------------------------------------------------------

/** Where the About section is in the one sequence it can walk: check, then
 * either nothing or a download, then a restart you choose the moment for. */
export type UpdateState =
  | { at: "idle" }
  | { at: "checking" }
  | { at: "current" }
  | { at: "found"; version: string; notes: string | null }
  | { at: "downloading"; version: string; downloaded: number; total: number | null }
  | { at: "staged"; version: string }
  | { at: "failed"; why: string };

export type UpdateRow = {
  /** The line beside the button. */
  status: string;
  /** What the button says, or null when there is nothing to press. */
  action: string | null;
  /** 0–1 while downloading and the size is known, else null. */
  progress: number | null;
  /** A second line: release notes, or why it failed. */
  detail: string | null;
};

/** The About row for a given state.
 *
 * Two things this deliberately does *not* do. It never says "up to date"
 * for a check that failed — a failure is its own state, because telling
 * someone they have the newest version when we couldn't reach the question
 * is the one wrong answer here that they would act on. And it offers no
 * button at all while something is in flight, rather than a disabled one:
 * there is nothing to press twice, and a greyed button invites waiting for
 * it to come back. */
export function updateRow(s: UpdateState): UpdateRow {
  switch (s.at) {
    case "idle":
      return { status: "", action: "Check for updates", progress: null, detail: null };
    case "checking":
      return { status: "Checking…", action: null, progress: null, detail: null };
    case "current":
      return { status: "Up to date", action: "Check for updates", progress: null, detail: null };
    case "found":
      return {
        status: `${s.version} is available`,
        action: "Update",
        progress: null,
        detail: s.notes,
      };
    case "downloading": {
      const size = s.total
        ? ` — ${formatBytes(s.downloaded)} of ${formatBytes(s.total)}`
        : "";
      return {
        status: `Downloading ${s.version}${size}`,
        action: null,
        // a bar that can't say how far along it is would only be an
        // animation; the byte count above is the honest version
        progress: s.total ? Math.min(1, s.downloaded / s.total) : null,
        detail: null,
      };
    }
    case "staged":
      return {
        status: `${s.version} is installed`,
        action: "Restart now",
        progress: null,
        detail: "It takes effect the next time the app opens.",
      };
    case "failed":
      return { status: "Couldn't update", action: "Try again", progress: null, detail: s.why };
  }
}

// ---------------------------------------------------------------------------
// ingest wiring (desktop only)
// ---------------------------------------------------------------------------

import { $dropzone, $home } from "./dom";
import { getCol, ingesting, loadCollections, renderHome, updateBookProgress } from "./home";
import { setOnboardPicker } from "./onboard";
import { formatBytes } from "./settings-model";
import { desktop } from "./state";
import { notify } from "./toast";

/** Sentence for what an add actually did. Silence is only correct when
 * everything worked — a skipped or failed file has to say so, because the
 * old handler dropped folders and unsupported files without a word. */
function addSummary(r: {
  queued: string[];
  duplicates: number;
  skipped: string[];
  failed: [string, string][];
}): string | null {
  const parts: string[] = [];
  if (r.duplicates) parts.push(`${r.duplicates} already in your library`);
  if (r.skipped.length) {
    const kinds = [...new Set(r.skipped.map((n) => n.split(".").pop()?.toLowerCase() ?? "?"))];
    parts.push(`${r.skipped.length} can't be read yet (${kinds.join(", ")})`);
  }
  for (const [name, why] of r.failed) parts.push(`${name}: ${why}`);
  if (!parts.length) return r.queued.length ? null : "nothing to add";
  return parts.join(" · ");
}

async function queueFiles(paths: string[]) {
  if (!desktop) return;
  if (!paths.length) return;
  try {
    const r = await desktop.ingestPaths(paths, getCol() || null);
    // queued books announce themselves by appearing on a shelf; everything
    // else needs explaining
    const msg = addSummary(r);
    if (msg) notify(msg, { sticky: r.failed.length > 0 });
  } catch (e) {
    notify(`add failed: ${e}`, { sticky: true });
  }
  renderHome();
}

/** Open the native chooser and queue whatever comes back. Shared by the
 * first-run panel's button and ⌘O — one path, so the folder handling and
 * the extension gate can't diverge between them. */
export async function pickAndQueue() {
  if (!desktop) return;
  try {
    await queueFiles(await desktop.pickFiles());
  } catch (e) {
    notify(`add failed: ${e}`, { sticky: true });
  }
}

export function wireDesktop() {
  if (!desktop) return;
  setOnboardPicker(() => void pickAndQueue());

  // ⌘O adds books from anywhere in the app, the way every other Mac app
  // opens a file — but never out from under someone typing.
  document.addEventListener("keydown", (e) => {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key !== "o") return;
    const t = e.target as HTMLElement | null;
    if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t?.isContentEditable)
      return;
    e.preventDefault();
    void pickAndQueue();
  });
  desktop.onDragDrop(
    () => ($dropzone.hidden = false),
    () => ($dropzone.hidden = true),
    (paths) => queueFiles(paths),
  );
  desktop.onIngestProgress((e) => {
    if (e.stage === "log") return;
    if (e.stage === "done" || e.stage === "error") {
      ingesting.delete(e.doc);
      // "done" needs no announcement — the book appears on the shelf
      if (e.stage === "error") notify(`ingest failed: ${e.message}`, { sticky: true });
      loadCollections().then(renderHome);
      return;
    }
    ingesting.set(e.doc, e);
    const elCard = $home.querySelector(`.book[data-doc="${CSS.escape(e.doc)}"]`);
    if (elCard) updateBookProgress(elCard, e);
    else renderHome(); // first event for a doc we haven't drawn yet
  });
  desktop.onAppError((msg) => {
    notify(msg, { sticky: true });
  });
  desktop.onAppWaiting((msg) => {
    notify(msg, { sticky: true });
  });
  // Said once, before any page image is removed. Sticky, because it is the
  // only notice given and it fires while the user is looking at something
  // else — a message about gigabytes that vanishes in four seconds is the
  // same as no message.
  desktop.onCacheAnnounce((bytes) => {
    notify(cacheAnnouncement(bytes), { sticky: true });
  });
}

/** What the one-time cache notice says. Its job is to make a number that
 * sounds alarming — several gigabytes about to be deleted — read as what it
 * is, and to point at the setting before it happens rather than after. */
export function cacheAnnouncement(bytes: number): string {
  return (
    `The Library keeps a limited number of page images and re-creates the rest ` +
    `from your files as you read. About ${formatBytes(bytes)} will be freed. ` +
    `Change the limit in Settings → Storage.`
  );
}

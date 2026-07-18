// ---------------------------------------------------------------------------
// ingest wiring (desktop only)
// ---------------------------------------------------------------------------

import { $dropzone, $home } from "./dom";
import { getCol, ingesting, loadCollections, renderHome, updateBookProgress } from "./home";
import { desktop } from "./state";
import { notify } from "./toast";

let libraryDir = ""; // <data>/pdfs, for the move-confirm dialog

async function queuePdfs(paths: string[]) {
  if (!desktop) return;
  const pdfs = paths.filter((p) => p.toLowerCase().endsWith(".pdf"));
  if (!pdfs.length) return;
  // the library owns its documents: adding a PDF MOVES it into the
  // library folder, and that never happens without the user saying so
  const names = pdfs.map((p) => p.split("/").pop() ?? p);
  if (!(await desktop.confirmMove(names, libraryDir))) return;
  try {
    const queued = await desktop.ingestPaths(pdfs, getCol() || null, "move");
    // queued docs show up on the shelves; only silence needs explaining
    if (!queued.length) notify("already in the queue");
  } catch (e) {
    notify(`add failed: ${e}`, { sticky: true });
  }
  renderHome();
}

export function wireDesktop() {
  if (!desktop) return;
  desktop
    .getSettings()
    .then((s) => (libraryDir = `${s.data}/pdfs`))
    .catch(() => {});
  desktop.onDragDrop(
    () => ($dropzone.hidden = false),
    () => ($dropzone.hidden = true),
    (paths) => queuePdfs(paths),
  );
  desktop.onIngestProgress((e) => {
    if (e.stage === "log") return;
    if (e.stage === "done" || e.stage === "error") {
      ingesting.delete(e.doc);
      // "done" needs no announcement — the book appears on the shelf
      if (e.stage === "error") notify(`ingest failed: ${e.message}`, { sticky: true });
      loadCollections();
      renderHome();
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
}

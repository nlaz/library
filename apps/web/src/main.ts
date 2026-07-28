// Bootstrap: routing, startup sequencing, and the cross-module wiring that
// belongs nowhere else (collections-tab clicks touch both home and search;
// the perf hotkey must outrank every other key layer). Everything else lives
// in the feature modules: dom / format / toast / viewer / home / ingest-ui /
// search.

import { atlasOpen, dismissAtlasSelection, toggleAtlas } from "./atlas";
import { initChat } from "./chat";
import { $cols, $q, $searchNav } from "./dom";
import { closeDrawer, initDrawer } from "./drawer";
import { docTitle, getDocList, prettify, setDocList } from "./format";
import { getCol, loadCollections, renderHome, setCol } from "./home";
import { wireDesktop } from "./ingest-ui";
import { closeNotes, notesOpen, openNotes, rerenderNotes } from "./notebox";
import { closeSheet, openSheet, sheetOpen, startCreate } from "./sheet";
import { dismissPerfPopover, perfOpen, setPerfTab, togglePerf } from "./perf";
import { closeReader, openReader, readerDoc, readerOpen } from "./reader";
import { initSearch, sendQuery, sentDoc, showSearch } from "./search";
import { desktop, setDesktop, setTransport, transport } from "./state";
import { notify } from "./toast";
import { isTauri, makeTransport } from "./transport";

// ---------------------------------------------------------------------------
// routing: #/ = home (shelves) or search results; #/read/<doc>?p=N = reader;
// #/notes = the ledger; #/notes/new|edit = the sheet
// ---------------------------------------------------------------------------

async function route() {
  const m = location.hash.match(/^#\/read\/([^?]+)(?:\?p=(\d+))?$/);
  const nm = location.hash.match(/^#\/notes(?:\?card=([^&]+))?$/);
  const sm = location.hash.match(/^#\/notes\/(new|edit)(?:\?card=([^&]+))?$/);
  closeDrawer(); // drawer is per-doc; any navigation invalidates it
  if (m) {
    closeNotes();
    closeSheet();
    const doc = decodeURIComponent(m[1]);
    // no explicit ?p= -> the reader resumes the remembered position
    openReader(doc, await pagesOf(doc), m[2] ? Number(m[2]) : undefined, docTitle(doc));
  } else if (sm) {
    closeReader();
    closeNotes();
    await openSheet(sm[1] as "new" | "edit", sm[2] ? decodeURIComponent(sm[2]) : null);
  } else if (nm) {
    closeReader();
    closeSheet();
    await openNotes(nm[1] ? decodeURIComponent(nm[1]) : null);
  } else {
    closeReader();
    closeNotes();
    closeSheet();
  }
  // crossing the library/reader boundary re-scopes the query: in-flight
  // answers for the old scope are dropped by seq, this refreshes the new one
  sentDoc.clear();
  if ($q.value.trim()) sendQuery();
  $searchNav.hidden = !readerOpen();
}
window.addEventListener("hashchange", route);

/** Total page count for a doc, so the reader knows how far it can scroll.
 * The desktop build already has this in `docList`; the web build has no
 * such command, so it asks the server directly. */
async function pagesOf(id: string): Promise<number> {
  const info = getDocList().find((d) => d.id === id);
  if (info) return info.pages;
  try {
    const res = await fetch(`/api/pages/${encodeURIComponent(id)}`);
    if (res.ok) return (await res.json()).pages;
  } catch {
    // offline/unreachable — fall through to the generous fallback below
  }
  return 9999;
}

// window + capture phase: the perf view sits above every other layer, so its
// keys must run before all the document-level handlers elsewhere (and before
// inputs' stopPropagation). Cmd+. toggles; Escape closes exactly this layer.
window.addEventListener(
  "keydown",
  (e) => {
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === ".") {
      e.preventDefault();
      e.stopPropagation();
      togglePerf();
    } else if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === "/") {
      e.preventDefault();
      e.stopPropagation();
      toggleAtlas();
    } else if (e.key === "Escape" && perfOpen()) {
      e.preventDefault();
      e.stopPropagation();
      // innermost layer first: a term popover closes before the view does
      if (!dismissPerfPopover()) togglePerf();
    } else if (e.key === "Escape" && atlasOpen()) {
      e.preventDefault();
      e.stopPropagation();
      // innermost layer first: an active theme deselects before the view
      // closes (perf sits above the atlas, so its branch runs first)
      if (!dismissAtlasSelection()) toggleAtlas();
    } else if (perfOpen() && !e.metaKey && !e.ctrlKey && !e.altKey && "1234".includes(e.key)) {
      // this handler outranks inputs' stopPropagation, so it must not steal
      // digits from someone typing
      const t = (e.target as HTMLElement | null)?.tagName;
      if (t === "INPUT" || t === "TEXTAREA") return;
      e.preventDefault();
      e.stopPropagation();
      setPerfTab((["search", "ingest", "agent", "memory"] as const)[Number(e.key) - 1]);
    }
  },
  true,
);

// c starts a note from any surface (routing closes whatever's open) — but
// never over the perf/atlas layers, and never while writing somewhere
document.addEventListener("keydown", (e) => {
  if (e.key !== "c" || e.metaKey || e.ctrlKey || e.altKey) return;
  if (sheetOpen() || perfOpen() || atlasOpen()) return;
  const t = e.target as HTMLElement | null;
  if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t?.isContentEditable)
    return;
  e.preventDefault();
  startCreate();
});

async function main() {
  if (isTauri()) setDesktop(await import("./tauri"));
  setTransport(await makeTransport());
  wireDesktop();
  showSearch(false);
  renderHome();

  initChat({
    prettify,
    desktop: desktop ? { turn: desktop.chatTurn, cancel: desktop.chatCancel } : null,
  });

  initDrawer({
    currentDoc: readerDoc,
    getDoc: async (id) => {
      if (desktop) {
        setDocList(await desktop.docs());
        const d = getDocList().find((x) => x.id === id);
        if (d) return d;
      } else {
        try {
          const res = await fetch(`/api/doc/${encodeURIComponent(id)}`);
          if (res.ok) return await res.json();
        } catch {
          // offline/unreachable — fall through to the empty shape below
        }
      }
      return { id, title: null, pages: 0, collections: [], status: null };
    },
    getCollections: () => transport.collections(),
    prettify,
    // no write path in the web build — the drawer renders read-only
    edit: desktop ? { setTitle: desktop.setTitle, setCollections: desktop.setCollections } : null,
    onChanged: async (id) => {
      await renderHome(await loadCollections());
      // a rename must show in the reader chrome immediately
      if (readerDoc() === id) {
        document.getElementById("reader-title")!.textContent = docTitle(id);
      }
    },
    onError: (msg) => notify(msg, { sticky: true }),
  });

  await transport.ready();
  loadCollections();
  renderHome().then(route); // route needs page counts for #/read links
  initSearch();

  $cols.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest("button");
    if (!btn) return;
    // no "Everything" tab: clicking the active collection again clears it
    setCol(getCol() === btn.dataset.col ? "" : btn.dataset.col!);
    for (const b of $cols.children) b.classList.toggle("on", b === btn && getCol() !== "");
    if (notesOpen()) rerenderNotes(); // the tabs scope the ledger too
    else if ($q.value.trim()) sendQuery();
    else renderHome();
  });
}

main().catch((e) => {
  notify(`startup failed: ${e?.message ?? e}`, { sticky: true });
});

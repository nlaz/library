import { pageImg, pageUrl } from "./assets";
import { closeReader, openReader } from "./reader";
import { isTauri, makeTransport, type Transport } from "./transport";
import type { Collections, DocInfo, IngestEvent, WireHit, WireResponse } from "./types";

const $q = document.getElementById("q") as HTMLInputElement;
const $cols = document.getElementById("cols")!;
const $kind = document.getElementById("kind")!;
let col = ""; // "" = everything, else a collection name
let kind = ""; // "" = all, "text", "images"
const $status = document.getElementById("status")!;
const $home = document.getElementById("home")!;
const $results = document.getElementById("results")!;
const $overlay = document.getElementById("overlay")!;
const $viewerLabel = document.getElementById("viewer-label")!;
const $viewerRead = document.getElementById("viewer-read")!;
const $viewerClose = document.getElementById("viewer-close")!;
const $pageImg = document.getElementById("page-img") as HTMLImageElement;
const $pageHl = document.getElementById("page-hl")!;
const $addBtn = document.getElementById("add-btn")!;
const $dropzone = document.getElementById("dropzone")!;

const FULL_DEBOUNCE_MS = 120;
const PREVIEW_H = 190; // px, keep in sync with .preview height in style.css

let seq = 0;
let rendered = 0; // highest seq drawn; anything older is stale
let transport: Transport;
let desktop: typeof import("./tauri") | null = null;
let docList: DocInfo[] = [];
const ingesting = new Map<string, IngestEvent>();

// ---------------------------------------------------------------------------
// routing: #/ = home (shelves) or search results; #/read/<doc>?p=N = reader
// ---------------------------------------------------------------------------

function route() {
  const m = location.hash.match(/^#\/read\/([^?]+)(?:\?p=(\d+))?$/);
  if (m) {
    const doc = decodeURIComponent(m[1]);
    const info = docList.find((d) => d.id === doc);
    const title = info ? displayTitle(info) : prettify(doc);
    // no explicit ?p= -> the reader resumes the remembered position
    openReader(doc, info?.pages ?? 9999, m[2] ? Number(m[2]) : undefined, title);
  } else {
    closeReader();
  }
}
window.addEventListener("hashchange", route);

function prettify(id: string): string {
  return id
    .split("-")
    .map((w) => (w.length > 2 ? w[0].toUpperCase() + w.slice(1) : w))
    .join(" ");
}

function displayTitle(d: DocInfo): string {
  return d.title ?? prettify(d.id);
}

// ---------------------------------------------------------------------------
// collections tabs
// ---------------------------------------------------------------------------

async function loadCollections() {
  const cols = await transport.collections();
  // the active collection can vanish (last doc removed) — fall back to all
  if (col && !(col in cols)) {
    col = "";
    $cols.children[0]?.classList.add("on");
  }
  // idempotent: keep the "Everything" button, rebuild the rest
  for (const b of [...$cols.children].slice(1)) b.remove();
  for (const name of Object.keys(cols)) {
    const btn = document.createElement("button");
    btn.dataset.col = name;
    btn.textContent = name;
    const n = document.createElement("span");
    n.className = "n";
    n.textContent = String(cols[name].length);
    btn.append(n);
    if (name === col) btn.classList.add("on");
    $cols.append(btn);
  }
}

// ---------------------------------------------------------------------------
// home: shelves of books (desktop only — needs the docs command)
// ---------------------------------------------------------------------------

async function renderHome() {
  if (!desktop) return;
  const [ds, cols] = await Promise.all([desktop.docs(), transport.collections()]);
  docList = ds;

  const byId = new Map(ds.map((d) => [d.id, d]));
  const shelves: [string, DocInfo[]][] = Object.entries(cols)
    .map(([name, ids]) => [name, ids.map((id) => byId.get(id)).filter(Boolean)] as [string, DocInfo[]])
    .filter(([, docs]) => docs.length > 0);
  const sorted = new Set(Object.values(cols).flat());
  const unsorted = ds.filter((d) => !sorted.has(d.id));
  if (unsorted.length) shelves.push(["Unsorted", unsorted]);

  const visible = col ? shelves.filter(([name]) => name === col) : shelves;
  $home.replaceChildren(
    ...visible.map(([name, docs]) => {
      const shelf = document.createElement("section");
      shelf.className = "shelf";
      const h = document.createElement("h2");
      h.textContent = name;
      const row = document.createElement("div");
      row.className = "books";
      row.append(...docs.map((d) => bookCard(d, cols)));
      shelf.append(h, row);
      return shelf;
    }),
  );
  if (!visible.length) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "the library is empty — drop a PDF anywhere";
    $home.replaceChildren(empty);
  }
}

function bookCard(d: DocInfo, cols: Collections): HTMLElement {
  const el = document.createElement("div");
  el.className = "book";
  el.dataset.doc = d.id;

  const cover = document.createElement("div");
  cover.className = "cover";
  if (d.pages > 0) {
    const img = document.createElement("img");
    img.loading = "lazy";
    img.src = pageImg(d.id, 1);
    cover.append(img);
  }
  const title = document.createElement("div");
  title.className = "btitle";
  title.textContent = displayTitle(d);
  const sub = document.createElement("div");
  sub.className = "bsub";
  sub.textContent = d.pages ? `${d.pages} pages` : "queued";

  el.append(cover, title, sub);

  if (d.processing) {
    el.classList.add("processing");
    const bar = document.createElement("div");
    bar.className = "progress";
    const fill = document.createElement("div");
    fill.className = "fill";
    bar.append(fill);
    cover.append(bar);
    updateBookProgress(el, ingesting.get(d.id));
  } else {
    el.addEventListener("click", () => {
      location.hash = `#/read/${encodeURIComponent(d.id)}`;
    });
    if (desktop) el.append(bookMenu(el, d, cols));
  }
  return el;
}

// ---------------------------------------------------------------------------
// book-card "…" menu: rename / collections / delete (desktop only)
// ---------------------------------------------------------------------------

function closeMenus() {
  for (const m of document.querySelectorAll(".book-menu")) m.remove();
}
document.addEventListener("click", closeMenus);

function bookMenu(card: HTMLElement, d: DocInfo, cols: Collections): HTMLElement {
  const btn = document.createElement("button");
  btn.className = "bmenu";
  btn.textContent = "⋯";
  btn.title = "Rename, collections, delete";
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    const open = card.querySelector(".book-menu");
    closeMenus();
    if (!open) card.append(menuPanel(card, d, cols));
  });
  return btn;
}

function menuPanel(card: HTMLElement, d: DocInfo, cols: Collections): HTMLElement {
  const panel = document.createElement("div");
  panel.className = "book-menu";
  panel.addEventListener("click", (e) => e.stopPropagation());

  const rename = document.createElement("button");
  rename.textContent = "Rename";
  rename.addEventListener("click", () => {
    panel.remove();
    renameInline(card, d);
  });

  const colList = document.createElement("div");
  colList.className = "mcols";
  const checked = () =>
    [...colList.querySelectorAll<HTMLInputElement>("input[type=checkbox]")]
      .filter((c) => c.checked)
      .map((c) => c.dataset.col!);
  const apply = async (names: string[]) => {
    try {
      await desktop!.setCollections(d.id, names);
    } catch (e) {
      $status.textContent = `collections: ${e}`;
    }
    await loadCollections();
    renderHome();
  };
  for (const name of Object.keys(cols)) {
    const row = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.dataset.col = name;
    box.checked = d.collections.includes(name);
    box.addEventListener("change", () => apply(checked()));
    row.append(box, name);
    colList.append(row);
  }
  const newCol = document.createElement("input");
  newCol.type = "text";
  newCol.placeholder = "new collection…";
  newCol.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter" && newCol.value.trim()) {
      apply([...checked(), newCol.value.trim()]);
    }
    if (e.key === "Escape") closeMenus();
  });

  const del = document.createElement("button");
  del.className = "danger";
  del.textContent = "Delete…";
  del.addEventListener("click", async () => {
    closeMenus();
    if (!(await desktop!.confirmDelete(displayTitle(d)))) return;
    try {
      await desktop!.deleteDoc(d.id);
      localStorage.removeItem(`pos:${d.id}`);
      if (location.hash.startsWith(`#/read/${encodeURIComponent(d.id)}`)) location.hash = "#/";
      $status.textContent = `deleted ${displayTitle(d)}`;
    } catch (e) {
      $status.textContent = `delete failed: ${e}`;
    }
    await loadCollections();
    renderHome();
  });

  panel.append(rename, colList, newCol, del);
  return panel;
}

function renameInline(card: HTMLElement, d: DocInfo) {
  const title = card.querySelector<HTMLElement>(".btitle");
  if (!title) return;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "brename";
  input.value = displayTitle(d);
  input.addEventListener("click", (e) => e.stopPropagation());
  let done = false;
  const commit = async () => {
    if (done) return;
    done = true;
    // storing the prettified id would freeze the fallback; treat it as "unset"
    const v = input.value.trim() === prettify(d.id) ? "" : input.value;
    try {
      await desktop!.setTitle(d.id, v);
    } catch (e) {
      $status.textContent = `rename: ${e}`;
    }
    renderHome();
  };
  input.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter") commit();
    if (e.key === "Escape") {
      done = true;
      renderHome();
    }
  });
  input.addEventListener("blur", commit);
  title.replaceChildren(input);
  input.focus();
  input.select();
}

const STAGE_LABEL: Record<string, string> = {
  ocr: "reading pages",
  embed: "indexing text",
  figures: "finding figures",
  clip: "indexing figures",
  indexing: "committing",
};

function updateBookProgress(el: Element, e?: IngestEvent) {
  const sub = el.querySelector(".bsub");
  const fill = el.querySelector<HTMLElement>(".progress .fill");
  if (!e || !sub || !fill) return;
  const frac = e.total > 0 ? e.done / e.total : 0;
  fill.style.width = `${Math.round(frac * 100)}%`;
  const n = e.total > 0 ? ` ${e.done}/${e.total}` : "";
  sub.textContent = `${STAGE_LABEL[e.stage] ?? e.stage}${n}`;
}

// ---------------------------------------------------------------------------
// ingest wiring (desktop only)
// ---------------------------------------------------------------------------

async function queuePdfs(paths: string[]) {
  if (!desktop) return;
  const pdfs = paths.filter((p) => p.toLowerCase().endsWith(".pdf"));
  if (!pdfs.length) return;
  try {
    const queued = await desktop.ingestPaths(pdfs, col || null);
    $status.textContent = queued.length
      ? `queued: ${queued.join(", ")}`
      : "already in the queue";
  } catch (e) {
    $status.textContent = `add failed: ${e}`;
  }
  renderHome();
}

function wireDesktop() {
  if (!desktop) return;
  $addBtn.hidden = false;
  $addBtn.addEventListener("click", async () => {
    queuePdfs(await desktop!.pickPdfs());
  });
  desktop.onDragDrop(
    () => ($dropzone.hidden = false),
    () => ($dropzone.hidden = true),
    (paths) => queuePdfs(paths),
  );
  desktop.onIngestProgress((e) => {
    if (e.stage === "log") return;
    if (e.stage === "done" || e.stage === "error") {
      ingesting.delete(e.doc);
      $status.textContent =
        e.stage === "done" ? `added ${e.doc}` : `ingest failed: ${e.message}`;
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
    $status.textContent = msg;
  });
}

// ---------------------------------------------------------------------------
// search results (unchanged card/viewer logic, URLs via pageUrl)
// ---------------------------------------------------------------------------

function hlBoxes(boxes: [number, number, number, number][]): HTMLElement[] {
  return boxes.map(([x, y, w, h]) => {
    const b = document.createElement("div");
    b.className = "hl";
    b.style.left = `${x * 100}%`;
    b.style.top = `${y * 100}%`;
    b.style.width = `${w * 100}%`;
    b.style.height = `${h * 100}%`;
    return b;
  });
}

function card(hit: WireHit): { el: HTMLElement; place: () => void } {
  const el = document.createElement("div");
  el.className = "card";

  const isImage = hit.kind === "image";
  if (isImage) el.classList.add("img");

  const pv = document.createElement("div");
  pv.className = "preview";
  const inner = document.createElement("div");
  inner.className = "pv-inner";
  const img = document.createElement("img");
  img.src = pageUrl(hit.img);
  img.loading = "lazy";
  // an image card's crop IS the figure — an amber wash over the whole
  // preview would just be noise, so boxes only decorate text cards
  inner.append(img, ...(isImage ? [] : hlBoxes(hit.boxes)));
  pv.append(inner);

  const loc = document.createElement("div");
  loc.className = "loc";
  loc.textContent = `${hit.doc} · p.${hit.page}${isImage ? " · figure" : ""}`;
  const meta = document.createElement("div");
  meta.className = "meta";
  meta.append(loc);
  if (!isImage) {
    const snip = document.createElement("div");
    snip.className = "snip";
    for (const w of hit.snippet) {
      const node = w.m ? document.createElement("mark") : document.createElement("span");
      node.textContent = w.t;
      snip.append(node, " ");
    }
    meta.append(snip);
  }
  el.append(pv, meta);
  el.addEventListener("click", () => openViewer(hit));

  // zoom so the chunk's text block (server-provided crop, which excludes the
  // scan's white margins) fills the card width, then pan the first match to
  // the vertical center; needs the card in the DOM and the image loaded
  const place = () => {
    const w = pv.clientWidth;
    const pvH = pv.clientHeight || PREVIEW_H;
    if (!w || !img.naturalWidth) return;
    const [cx, cy, cw] = hit.crop;
    const dispW = w / Math.min(Math.max(cw, 0.05), 1);
    const dispH = dispW * (img.naturalHeight / img.naturalWidth);
    inner.style.width = `${dispW}px`;
    inner.style.height = `${dispH}px`;
    const yc = hit.boxes.length ? hit.boxes[0][1] + hit.boxes[0][3] / 2 : cy + 0.1;
    const offY = Math.max(0, Math.min(yc * dispH - pvH / 2, dispH - pvH));
    const offX = Math.max(0, Math.min(cx * dispW, dispW - w));
    inner.style.transform = `translate(${-offX}px, ${-offY}px)`;
  };
  if (!img.complete) img.addEventListener("load", place);
  return { el, place };
}

function render(msg: WireResponse) {
  const q = $q.value.trim();
  if (!q) return;

  $status.textContent = `${msg.hits.length} hits · ${msg.phase} · ${(
    msg.us / 1000
  ).toFixed(1)}ms`;

  const cards = msg.hits.map(card);
  $results.replaceChildren(...cards.map((c) => c.el));
  requestAnimationFrame(() => cards.forEach((c) => c.place()));

  if (msg.hits.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "no matches";
    $results.replaceChildren(empty);
  }
}

let viewerHit: WireHit | null = null;

function openViewer(hit: WireHit) {
  viewerHit = hit;
  $viewerLabel.textContent = `${hit.doc} — page ${hit.page}`;
  $viewerRead.hidden = !desktop;
  $pageHl.replaceChildren(...hlBoxes(hit.boxes));
  const src = pageUrl(hit.img);
  const reveal = () => {
    const first = $pageHl.firstElementChild;
    if (first) first.scrollIntoView({ block: "center" });
  };
  if ($pageImg.src.endsWith(src)) {
    reveal();
  } else {
    $pageImg.src = src;
    $pageImg.addEventListener("load", reveal, { once: true });
  }
  $overlay.hidden = false;
}

function closeViewer() {
  $overlay.hidden = true;
}

$viewerRead.addEventListener("click", () => {
  if (!viewerHit) return;
  closeViewer();
  location.hash = `#/read/${encodeURIComponent(viewerHit.doc)}?p=${viewerHit.page}`;
});
$viewerClose.addEventListener("click", closeViewer);
$overlay.addEventListener("click", (e) => {
  if (e.target === $overlay) closeViewer();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !$overlay.hidden) closeViewer();
});

// ---------------------------------------------------------------------------
// mode switching: home shelves when the query is empty, results otherwise
// ---------------------------------------------------------------------------

function showSearch(active: boolean) {
  $results.hidden = !active;
  $home.hidden = active;
}

async function main() {
  $status.textContent = "connecting…";
  if (isTauri()) desktop = await import("./tauri");
  transport = await makeTransport();
  wireDesktop();
  showSearch(false);
  renderHome();

  await transport.ready();
  $status.textContent = "ready";
  loadCollections();
  renderHome().then(route); // route needs page counts for #/read links
  transport.onResponse((msg) => {
    if (msg.seq < rendered) return; // superseded while in flight
    rendered = msg.seq;
    render(msg);
  });

  const send = (mode: "instant" | "full") => {
    const q = $q.value.trim();
    if (!q) {
      rendered = ++seq;
      $status.textContent = "ready";
      $results.replaceChildren();
      showSearch(false);
      renderHome();
      return;
    }
    showSearch(true);
    transport.send({ seq: ++seq, q, mode, col, kind });
  };

  let fullTimer: ReturnType<typeof setTimeout>;
  $q.addEventListener("input", () => {
    if (location.hash.startsWith("#/read/")) location.hash = "#/";
    send("instant"); // lexical, every keystroke
    clearTimeout(fullTimer);
    fullTimer = setTimeout(() => send("full"), FULL_DEBOUNCE_MS); // hybrid, on pause
  });
  $cols.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest("button");
    if (!btn) return;
    col = btn.dataset.col!;
    for (const b of $cols.children) b.classList.toggle("on", b === btn);
    if ($q.value.trim()) send("full");
    else renderHome();
  });
  $kind.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest("button");
    if (!btn) return;
    kind = btn.dataset.kind!;
    for (const b of $kind.children) b.classList.toggle("on", b === btn);
    send("full");
  });
}

main().catch((e) => {
  $status.textContent = `startup failed: ${e?.message ?? e}`;
});

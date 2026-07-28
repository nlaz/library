// One evidence anchor, rendered the ledger's way: a passage is its quote
// over a citation line, a region is the page scan windowed to the mark's
// bbox beside one. Shared by the notes timeline and the sheet so evidence
// reads identically wherever it appears.

import { pageImg } from "./assets";
import { docTitle } from "./format";
import { cropCSS } from "./notebox-model";
import type { QuoteAnchor } from "./types";

const trunc = (s: string, n: number) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);

export function citeLine(q: QuoteAnchor): string {
  return `${docTitle(q.doc)} · p. ${q.page} ↗`;
}

/** The whole row is the jump to the marked page. */
export function evidenceEl(q: QuoteAnchor): HTMLElement {
  const row = document.createElement("button");
  row.className = "lev";
  row.title = "Open the marked page";

  if (q.kind === "region") {
    const css = cropCSS(q.bbox);
    if (css) {
      const crop = document.createElement("span");
      crop.className = "lev-crop";
      const img = document.createElement("img");
      img.loading = "lazy";
      img.decoding = "async";
      img.alt = "";
      img.src = pageImg(q.doc, q.page);
      img.style.width = css.width;
      img.style.left = css.left;
      img.style.top = css.top;
      crop.append(img);
      row.append(crop);
    }
  }

  const meta = document.createElement("span");
  meta.className = "lev-meta";
  if (q.kind === "text") {
    const quote = document.createElement("span");
    quote.className = "lquote";
    quote.textContent = `“${trunc(q.text, 90)}”`;
    meta.append(quote);
  }
  const cite = document.createElement("span");
  cite.className = "lcite";
  cite.textContent = citeLine(q);
  meta.append(cite);
  row.append(meta);

  row.addEventListener("click", (e) => {
    e.stopPropagation(); // the entry's select-click must not fire
    location.hash = `#/read/${q.doc}?p=${q.page}`;
  });
  return row;
}

import { isTauri } from "./transport";

/**
 * Server wire hits carry `/pages/<doc>/page-NNNN.jpg`. The web build serves
 * that path over HTTP; the desktop build serves it via the `pages://` custom
 * protocol (macOS shape: pages://localhost/<doc>/<file>).
 */
export function pageUrl(img: string): string {
  return isTauri() ? "pages://localhost" + img.slice("/pages".length) : img;
}

export function pageImg(doc: string, page: number): string {
  return pageUrl(`/pages/${doc}/page-${String(page).padStart(4, "0")}.jpg`);
}

/** Per-page OCR words ({page, words: [{t,x,y,w,h}]}), for the text layer. */
export function ocrUrl(doc: string, page: number): string {
  const p = `/${encodeURIComponent(doc)}/page-${String(page).padStart(4, "0")}.json`;
  return isTauri() ? "ocr://localhost" + p : "/ocr" + p;
}

// Element lookups for the static chrome in index.html, resolved once at
// startup and shared by every module. Pure lookups only — no state, no
// listeners.

export const $q = document.getElementById("q") as HTMLInputElement;
export const $cols = document.getElementById("cols")!;
export const $home = document.getElementById("home")!;
export const $search = document.getElementById("search")!;
export const $stats = document.getElementById("stats")!;
export const $results = document.getElementById("results")!;
export const $more = document.getElementById("more")!;
export const $main = document.querySelector("main")!;
export const $overlay = document.getElementById("overlay")!;
export const $viewerLabel = document.getElementById("viewer-label")!;
export const $viewerRead = document.getElementById("viewer-read")!;
export const $viewerClose = document.getElementById("viewer-close")!;
export const $pageImg = document.getElementById("page-img") as HTMLImageElement;
export const $pageHl = document.getElementById("page-hl")!;
export const $dropzone = document.getElementById("dropzone")!;
export const $searchPop = document.getElementById("search-pop")!;
export const $ac = document.getElementById("ac") as HTMLUListElement;
export const $searchNav = document.getElementById("search-nav")!;
export const $searchCount = document.getElementById("search-count")!;
export const $searchPrev = document.getElementById("search-prev")!;
export const $searchNext = document.getElementById("search-next")!;
export const $toast = document.getElementById("toast")!;

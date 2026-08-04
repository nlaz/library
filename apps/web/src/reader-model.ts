// Pure reader policy, kept out of reader.ts so it can be tested without a
// scroller, an IntersectionObserver, or a network.

/** Attempts after the first before a page image gives up and shows a
 * placeholder. Two, because the transient case this covers — the render
 * queue refusing admission — clears in about a second. */
export const PAGE_RETRIES = 2;

/** How long to wait before retry `attempt` (1-based), or null to give up.
 *
 * A page image can 404 because its render was evicted and is being made
 * again, or 503 because the render queue is full and shed the request. An
 * `img` error event says which of those it was — nothing, it carries no
 * status — so the policy is attempt-based and deliberately patient rather
 * than clever. Backing off matters: a search can put twenty-odd page
 * requests in flight at once, and retrying them all on the same short timer
 * just rebuilds the queue that shed them. */
export function pageRetryDelay(attempt: number): number | null {
  if (attempt < 1 || attempt > PAGE_RETRIES) return null;
  // 400ms, then 1200ms — inside the reader's ~1-2s of prefetch runway
  return 400 * 3 ** (attempt - 1);
}

/** What to tell someone when a page image will not load.
 *
 * The reason comes from the `x-library-reason` header the page protocol
 * sets, when we have it. We usually do not — an `img` element's error event
 * exposes no headers — so the default has to be honest about uncertainty
 * without being alarming: a page that failed twice is usually still coming. */
export function pageErrorText(reason?: string | null): string {
  switch (reason) {
    case "no-source":
      return "This document's file has moved, so this page can't be recreated.";
    case "offline":
      return "This document is in iCloud and hasn't been downloaded yet.";
    case "render-failed":
      return "This page couldn't be recreated from the file.";
    default:
      return "This page didn't load.";
  }
}

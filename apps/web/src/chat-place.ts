// Placement for chat tool-activity rows. Lives outside chat.ts so it can be
// unit-tested without that module's top-level DOM lookups.

/** Tool rows are provenance for the answer, so they belong above it: insert
 * before the streaming assistant row when one exists in the log (late tool
 * events used to append below the lazily-created answer), else append. */
export function placeToolRow(log: HTMLElement, el: HTMLElement, anchor: HTMLElement | null): void {
  if (anchor && anchor.parentElement === log) log.insertBefore(el, anchor);
  else log.append(el);
}

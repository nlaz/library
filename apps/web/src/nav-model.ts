// ---------------------------------------------------------------------------
// Where "back" goes. The app has no browser chrome, so every ← has to know —
// and be able to name — the surface it returns to. A book is reached from
// the shelves, from a search hit, from an atlas trail, from a note's
// evidence; until this module existed, all four went back to the shelves.
//
// Pure rules over a stack of hashes; nav.ts keeps the one live copy.
// ---------------------------------------------------------------------------

/** How much trail is kept. Hopping citations forever must not grow an
 * unbounded stack, and nobody unwinds a dozen surfaces by hand. */
const DEPTH = 12;

/** Longest a book title may be on a back button before it's cut. */
const MAX_LABEL = 28;

export type SurfaceKind = "library" | "read" | "notes" | "sheet" | "settings";

export type Surface = { kind: SurfaceKind; key: string; doc: string };

/** A hash's surface identity. Two hashes with the same key are the same
 * place: `?p=` and `?card=` are movements *within* a surface, not trips
 * between them, and recording them would make ← a no-op. */
export function surfaceOf(hash: string): Surface {
  const read = hash.match(/^#\/read\/([^?]+)/);
  if (read) {
    const doc = decodeURIComponent(read[1]);
    return { kind: "read", key: `read:${doc}`, doc };
  }
  if (/^#\/notes\/(new|edit)/.test(hash)) return { kind: "sheet", key: "sheet", doc: "" };
  if (/^#\/notes/.test(hash)) return { kind: "notes", key: "notes", doc: "" };
  if (/^#\/settings/.test(hash)) return { kind: "settings", key: "settings", doc: "" };
  return { kind: "library", key: "library", doc: "" };
}

/** Record a trip from `from` to `to`. */
export function pushNav(trail: string[], from: string, to: string): string[] {
  const f = surfaceOf(from);
  const t = surfaceOf(to);
  if (f.key === t.key) return trail; // a move inside one surface
  // A draft is never somewhere to return *into*: leaving the sheet saves
  // and closes it, so the hash that opened it is already spent.
  if (f.kind === "sheet") return trail;
  // Arriving somewhere already on the trail is a return, however it was
  // reached — unwind to it rather than looping A → B → A → B forever.
  const seen = trail.findIndex((h) => surfaceOf(h).key === t.key);
  if (seen >= 0) return trail.slice(0, seen);
  return [...trail, from].slice(-DEPTH);
}

/** The surface the current one was entered from, and the trail without it. */
export function popNav(trail: string[]): { to: string | null; trail: string[] } {
  if (!trail.length) return { to: null, trail };
  return { to: trail[trail.length - 1], trail: trail.slice(0, -1) };
}

/** Drop every leg that points at `doc` — it has been deleted, and a ← into
 * a missing book is worse than one step too few. */
export function forgetDoc(trail: string[], doc: string): string[] {
  return trail.filter((h) => surfaceOf(h).doc !== doc);
}

/** Every surface but a book, which is named by its title. */
const LABEL: Record<Exclude<SurfaceKind, "read">, string> = {
  library: "library",
  notes: "notes",
  sheet: "note",
  settings: "settings",
};

/** What to call a hash on a back button. Book titles come from the caller
 * (this file stays free of the doc list) and are cut to fit. */
export function navLabel(hash: string, title: (doc: string) => string): string {
  const s = surfaceOf(hash);
  if (s.kind === "read") {
    const t = title(s.doc);
    return t.length > MAX_LABEL ? `${t.slice(0, MAX_LABEL - 1)}…` : t;
  }
  return LABEL[s.kind];
}

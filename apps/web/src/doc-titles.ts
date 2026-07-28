// The notes surfaces cite documents by title, but the web build only
// learns titles lazily (the desktop build's docs() command has no web
// counterpart — just per-doc /api/doc). This tops up format.ts's docList
// for a set of ids so docTitle() stops falling back to prettified ids.

import { getDocList, setDocList } from "./format";
import { desktop } from "./state";
import type { DocInfo } from "./types";

export async function ensureDocTitles(ids: string[]): Promise<void> {
  const known = new Set(getDocList().map((d) => d.id));
  const missing = [...new Set(ids)].filter((id) => !known.has(id));
  if (!missing.length) return;
  if (desktop) {
    setDocList(await desktop.docs());
    return;
  }
  const fetched = await Promise.all(
    missing.map(async (id): Promise<DocInfo | null> => {
      try {
        const res = await fetch(`/api/doc/${encodeURIComponent(id)}`);
        return res.ok ? ((await res.json()) as DocInfo) : null;
      } catch {
        return null; // offline — docTitle's prettify fallback carries it
      }
    }),
  );
  setDocList([...getDocList(), ...fetched.filter((d): d is DocInfo => d !== null)]);
}

// Host seam for marginalia writes/reads: Tauri commands on desktop, the
// library-server HTTP routes on the plain web build. Both reach the same
// core functions server-side, so callers never care which host they're on.

import { desktop } from "./state";
import type { AnnotRec, CardRec, NeighborCard, NewCard, ThreadProposal } from "./types";

async function j<T>(res: Promise<Response>): Promise<T> {
  const r = await res;
  if (!r.ok) throw new Error((await r.text()) || `${r.status}`);
  return r.status === 204 ? (undefined as T) : ((await r.json()) as T);
}

const json = (method: string, body: unknown): RequestInit => ({
  method,
  headers: { "content-type": "application/json" },
  body: JSON.stringify(body),
});

export function listAnnotations(doc: string): Promise<AnnotRec[]> {
  return desktop
    ? desktop.listAnnotations(doc)
    : j(fetch(`/api/annotations/${encodeURIComponent(doc)}`));
}

export function saveAnnotation(annot: AnnotRec): Promise<AnnotRec> {
  return desktop ? desktop.saveAnnotation(annot) : j(fetch("/api/annotations", json("POST", annot)));
}

export function deleteAnnotation(doc: string, id: string): Promise<void> {
  return desktop
    ? desktop.deleteAnnotation(doc, id)
    : j(fetch(`/api/annotations/${encodeURIComponent(doc)}/${encodeURIComponent(id)}`, { method: "DELETE" }));
}

export function listCards(): Promise<CardRec[]> {
  return desktop ? desktop.listCards() : j(fetch("/api/cards"));
}

export function createCard(input: NewCard): Promise<CardRec> {
  return desktop ? desktop.createCard(input) : j(fetch("/api/cards", json("POST", input)));
}

export function updateCard(card: CardRec): Promise<CardRec> {
  return desktop ? desktop.updateCard(card) : j(fetch("/api/cards", json("PUT", card)));
}

export function proposeThread(text: string): Promise<ThreadProposal | null> {
  return desktop
    ? desktop.proposeThread(text)
    : j(fetch("/api/cards/propose_thread", json("POST", { text })));
}

export function cardNeighbors(id: string, k = 8): Promise<NeighborCard[]> {
  return desktop
    ? desktop.cardNeighbors(id, k)
    : j(fetch(`/api/cards/${encodeURIComponent(id)}/neighbors?k=${k}`));
}

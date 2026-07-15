# Plan: productionize the librarian chat agent

*Follows the go verdict in [../spikes/chat-spike.md](../spikes/chat-spike.md).
The spike pipeline (web panel → SSE → Swift AFM sidecar → in-process search)
works end-to-end; this plan hardens it into a real feature, ordered so the
highest-risk item from the findings (confabulation) dies first.*

## Phase 1 — kill the confabulation vector (small, do first)

The one dangerous failure: empty tool results read as content and the model
invents answers with fake citations.

- `apps/library-server/src/main.rs` (`read_doc_text`): when the sliced text
  is empty or under ~40 chars, return
  `{"error": "pages N-M of <doc> are blank or image-only — pick a different page"}`
  instead of `{"text": ""}`. The spike proved the model handles explicit
  errors correctly (it relays them; it never invents around them).
- Same guard in `/api/search`: zero text hits → include
  `"note": "no strong matches — the library may not cover this"` so the
  model abstains honestly instead of stretching weak hits.

## Phase 2 — search tuned for agents, not keystrokes

The retrieval battery showed the search stack is tuned for type-ahead UI, and
agents trip over exactly those tunings.

- **`complete` flag** (`library-core::search` + `/api/search?complete=false`):
  skip `TermDict::complete` prefix expansion for agent queries — complete
  words in, complete words matched ("micro" must stop matching "microscope").
- **Absolute confidence**: expose the top hit's raw BM25 (not just
  rel-to-top) in `/api/search`; `search_library` maps it to
  `strong | weak | none` phrasing in the tool result.
- **One hop instead of two**: fold the top text hit's full page (from
  `data/text/<doc>.md`, ~1 page ≈ 400 tokens) into the `search_library`
  result as `"top_hit_page_text"`. Fixes plausibility-navigation (P5 read
  intro pages instead of the hit pages) and covers the shallow-paraphrase
  problem where snippets can't answer. Budget: 6 slim hits + 1 full page
  fits 4k comfortably alongside instructions + history.
- **Images opt-in**: `kind="all"` tool calls return text only; the model
  passes `kind="images"` when the user asks for figures (it already does
  this — sometimes too eagerly, which is why text must be the default).

## Phase 3 — sidecar lifecycle + cancellation

Spawn-per-turn works but costs ~6 s cold TTFT on the first turn and can't be
interrupted.

- **Persistent sidecar**: `librarian serve` mode — long-lived process, one
  NDJSON request/response cycle per line over stdin/stdout (same event
  schema, plus a `{"e":"turn_end"}` sentinel). Server keeps one child,
  respawns on exit. Warm TTFT drops to ~0.6 s. Keep `turn` mode for the
  eval harness.
- **Cancellation**: client abort (fetch `AbortController` on a stop button /
  panel close / new message) → server drops the SSE stream → send
  `{"cancel": true}` line to the sidecar → sidecar cancels the Swift task.
  Today a closed tab leaves the model generating to nobody.
- **Session reuse for multi-turn**: replace the "history folded into the
  prompt" hack with a per-conversation `LanguageModelSession` kept alive in
  the serve process (AFM sessions carry the transcript natively, including
  tool results — better grounding on follow-ups like "read me more of that
  page"). Evict after ~10 min idle; on `exceededContextWindowSize`, rebuild
  from the last user message (the retry path that already exists).

## Phase 4 — desktop (Tauri) parity

`apps/library-app` shares `library-core` but has no HTTP plane, so the
sidecar's tools can't call back into it as-is.

- Give the sidecar a `--tools-via stdin` mode: tool calls emit
  `{"e":"tool_request", id, name, args}` and block on a
  `{"tool_response", id, result}` line. The host executes tools itself.
- Rust side: extract the spike's tool implementations (slim-hit search,
  page-slice read, collections) from `library-server/src/main.rs` into
  `library-core` so server (HTTP handlers) and app (Tauri commands +
  tool_request handler) share one implementation.
- Tauri: `chat_turn` command streaming events via the existing event-emit
  pattern (`ingest:progress` precedent); reuse `chat.ts` with a transport
  switch, same as search already does.

## Phase 5 — UX polish

- **Stop button** in `#chat-top` while streaming (wires to Phase 3
  cancellation).
- **Prose citations → chips**: linkify `[doc-id p.N]` patterns in the final
  assistant text against docs seen in this turn's tool events (the exact-id
  problem is already solved by chips; this catches inline references).
- **Transcript persistence**: keep the conversation in `sessionStorage` so a
  reload doesn't eat it; "clear" button resets.
- **Empty/edge states**: greeting hint on first open ("ask about your
  books…"); offline state when `/api/chat` is unreachable; keep the
  existing error rows.
- **A11y pass**: `aria-live="polite"` on `#chat-log`, focus management on
  open/close, `aria-expanded` on the toggle.

## Phase 6 — regression evals (cheap insurance)

- Extend `examples/librarian-eval` fixtures with the Phase 1/2 behaviors:
  blank-page probe must return the error string; "micro" must not match
  "microscope"; expected-miss queries must produce the weak-hits note and an
  abstaining answer.
- Add `librarian-eval e2e`: 5 canned questions through `POST /api/chat`
  asserting event shape (tool → tokens → done) and that citations resolve to
  real doc/page pairs.
- Run before/after each phase; the probe battery is the safety net for
  prompt or tool-shape changes.

## Explicitly out of scope (follow-ups)

- Guided-generation features (recipe index, catalog mining) — biggest
  capability unlock from the spike, but a separate feature with its own plan.
- Streaming search-as-you-type integration between chat and the main grid.
- Multi-model routing / Ollama fallback (only if AFM quality becomes the
  bottleneck; the harness can re-judge any candidate).

## Verification

1. `cargo test -p library-core` (new: page-slice + confidence unit tests) and
   the Phase 6 eval fixtures green.
2. Blank-page probe: ask about a known-blank page → answer contains "blank",
   no invented facts, no fake citation.
3. Warm-turn TTFT < 1 s (persistent sidecar); stop button halts tokens < 500 ms.
4. Desktop: same 5 e2e questions through the Tauri build.
5. Memory soak: 20-turn conversation with reader open, no swap growth
   (Activity Monitor), sidecar count stays at 1.

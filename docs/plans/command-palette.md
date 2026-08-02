# Plan: the card catalog (⌘K)

**Status: shipped.** All six phases landed. Where the built thing differs from
what was planned, the plan has been corrected below and the reason given —
three decisions changed on contact with the code, and each is noted inline as
*Changed:*.

*Follows the design in [../command-palette.md](../command-palette.md). Ordered
so the riskiest thing lands first: not the ranking or the rows, but the key
layering — a palette that steals Escape from the reader or fails to open over
perf is worse than no palette, and it is the one part that cannot be
retrofitted safely.*

Web-only work; no Rust changes in any phase. Every row calls a function that
already exists.

## New files

```
apps/web/src/palette.ts             the surface: DOM, focus, stages
apps/web/src/palette-model.ts       pure: match, rank, group, stage stack
apps/web/src/palette-model.test.ts
apps/web/src/palette-dom.test.ts
apps/web/src/commands.ts            the registry
apps/web/src/keys-dom.test.ts       the registry/sheet drift test (phase 6)
apps/web/src/styles/palette.css
```

Touched: `index.html` (the `#palette` shell), `main.ts` (`initPalette` plus two
extracted seams), `nav.ts` (`recentTrail`), `styles/index.css` (one import),
`keys.ts` (a `⌘K` row and `documentedKeys`).

*Changed: no `dom.ts` edit.* `dom.ts` is for chrome several modules share; a
self-contained overlay looks its own elements up, which is what `keys.ts` and
`settings.ts` already do. All five refs stayed in `palette.ts`.

The `-model.ts` split is the house pattern (`nav-model`, `sheet-keys`,
`search-ghost`): everything decidable without a DOM goes there and is unit
tested, and `palette.ts` stays a renderer.

## Phase 1 — the shell and the layer discipline

Nothing but navigation commands in it. The point of this phase is the key
stack, and it should be reviewed as if that were the whole feature.

- `index.html`: `#palette` (scrim + card, `role="dialog"`) between `#settings`
  and `#toast`; header reads `card catalog`; a real `<input id="pal-q">`, a
  `<ul id="pal-list" role="listbox">`, and a footer hint row.
- `palette.css`: z-index **16**, above perf. Copy the `#keys` scrim
  (`color-mix(in srgb, var(--ink) 42%, transparent)`) and the `#keys-card`
  bezel verbatim — same drawer, same cabinet. Card pinned near the top of the
  viewport rather than centred, so the list grows downward without the input
  moving.
- A `⌘K`/`Ctrl+K` branch and an `Escape && paletteOpen()` branch, both on
  window + capture, both `preventDefault` and `stopPropagation`.

  *Changed: they live in `palette.ts`, not `main.ts`.* Registration order is
  what makes them outrank everything, and `main.ts` imports `palette.ts`, so
  the listener is in place before `main.ts` adds its own — the same argument
  `nav.ts` already makes for its listener. Two things follow. It is testable:
  `main.ts` calls `main()` at module scope, so no suite can import it, and a
  branch in there could never have been covered. And it keeps the chord next
  to the state it guards instead of a file away from it.
- Focus: latch `document.activeElement` on open, restore it on close. Closing
  must put the caret back in `#q` if that is where it came from.
- `commands.ts`: the registry type and the first entries — settings, perf,
  atlas, notes, shortcut sheet, new note, add books, back to library. Each is
  `{ id, label, group, chord?, host?, when?, run }`.

  *Changed: `host` and `when` are separate predicates.* One question is "can
  this build ever run this" (no filesystem in the browser) and the other is
  "does it apply this second" (its view is already open). They only looked
  like one predicate until phase 6, where the shortcut sheet needed the first
  without the second: a poster documents ⌘S whether or not settings is open.
- `?` sheet gains a `⌘K` row so the palette is discoverable the same way
  everything else is.

**Done when:** ⌘K opens over every layer including perf and from inside the
search field; Escape closes exactly one thing; running a row does what its
chord does.

## Phase 2 — the catalog

- `palette-model.ts`: subsequence fuzzy match scoring word-boundary and prefix
  hits above interior ones, ties broken by shorter label; `rank()` bins the
  survivors into books / shelves / notes / commands with a per-group cap so no
  one kind floods the list. Groups are ordered by their best row rather than
  by a fixed priority, so there is no ordering rule to keep in sync.
- Sources: `getDocList()` from `format.ts` (already the single owner of the
  shared doc list), `transport.collections()`, and `listCards()` from
  `marginalia-api.ts`. Read on open, not on every keystroke.
- Rows render title plus mono metadata on the right — shelf and page count for
  a book, book count for a shelf, relative date for a note.
- ⏎ navigates by setting `location.hash`, so the nav trail records the jump
  and Escape from the reader returns where the user actually was.
- An empty box shows no books at all. A library of a few hundred has no
  meaningful "first six", and opening onto an arbitrary slice of one is a
  census nobody asked for.
- The no-match row: "Search the library for …" hands the query to ⌘F.

  *Changed: no "Ask the librarian" row.* The chat surface is deliberately off
  — its glyph is hidden in `index.html` and `probe_chat` refuses below
  `CHAT_MIN_MACOS`. A palette row would have been a second, unguarded way in
  to a feature someone switched off on purpose. It belongs in the same commit
  that turns chat on, not this one.

**Done when:** any book, shelf or note is reachable by typing part of its
name, and a query with no matches offers the handoff instead of an empty list.

One scoring rule was got wrong first and is worth recording, because it is the
whole point of the feature. The matcher charged a gap penalty for every skip,
including one that landed on a word boundary — which ranked long titles below
short ones for no reason a reader would recognise. Half this library is called
"Kittler · Gramophone, Film, Typewriter" and nobody types that from the front.
The penalty now applies only to interior letters reached by skipping; a word
start is a word start wherever it sits.

## Phase 3 — context

- `palette.ts` asks the surface modules what is open (`readerOpen`/`readerDoc`)
  and puts a leading group in front of the recents.
- Reader group: markup mode, show/hide marks, the info drawer, jump to a page,
  rename, file into a collection, show in Finder, remove — all existing calls,
  gated to desktop where the command is Tauri-only. Markup mode and the drawer
  have no exported opener, so those two rows click their own buttons; reaching
  past a button to its module's private state would be exactly the duplication
  the registry exists to prevent.
- Recents come off a new `recentTrail()` in `nav.ts` (it already keeps 12
  legs), which returns a copy — a caller able to hold the live array could
  reorder somebody's way back.
- Context reorders defaults only. Typing still reaches everything.

  *Changed: no `localStorage` reader positions in the recents.* The trail
  already carries `?p=` on each leg, so the page number was there for free;
  a second source would have needed its own dedup against the first.

**Done when:** ⌘K in the reader leads with that book's verbs, and the same
verbs are still reachable by name from the library.

## Phase 4 — stages

- Stage stack in `palette-model.ts`: push on a `⇥`-marked row, pop on `⌫` at
  an empty input or Escape (Escape pops one stage; from the root stage it
  closes). Breadcrumb in the header. `⌫` only fires on an empty field, so it
  never eats a character somebody is deleting.
- Stages: a book's verbs, rename (free text → `set_title`), file into a
  collection (`move_to_shelf`, with a "New collection …" row — the only place
  the app offers to make one), jump to a page, and remove.
- A stage marked `raw` means the query is content rather than a filter: its
  rows are built *from* what was typed, so matching them against it again
  would be marking a string for containing itself.
- Destructive rows confirm as a stage, not a `confirm()` dialog: remove pushes
  a stage whose only row is the confirmation, and `⌫` walks back out of it.
- The hash does not move until a stage commits.
- Two seams came out of `main.ts` rather than being reimplemented: `chooseCol`
  (the header tabs' collection filter, now shared with the shelf rows) and
  `docChanged` (the repaint after a rename or reshelve, already written for
  the reader's drawer, now passed to both).

**Done when:** a book can be renamed and reshelved end to end from ⌘K, and
backing out of a half-finished stage leaves nothing behind.

## Phase 5 — the state section

- Read `status` off the doc list and the live `ingesting` map `home.ts` already
  keeps; render the attention block above everything when anything is
  non-terminal or failed.
- A `failed` row carries a Retry action (`retry_doc`); everything else is
  informational.
- Absent entirely on a settled library, and absent as soon as anything is
  typed — a book's own row carries the same state in its metadata, and the
  section must not be a second copy of the library.

**Done when:** a failed ingest is retryable from ⌘K without hunting for its
shelf card.

## Phase 6 — hold `?` and the registry together

*Changed: checked, not generated.* The plan was for `keys.ts` to derive its
command rows from the registry. Building it made the cost visible and the
benefit small:

- Only five of the sheet's ~20 rows are commands. The rest are keys no registry
  could know about — the reader's scroll keys, `[[`, the Escape ladders — so
  the drift the file warns about would have stayed unaddressed for most of it.
- The sheet's prose is longer and warmer than a command label wants to be
  ("press again to cycle everything → figures → text & notes"). Generating from
  the registry means moving that prose into a file about verbs.
- Row order within a section is editorial. ⌘K is the headline of *Views* and
  drag-and-drop is the friendliest thing in *Library*; a merge rule that put
  all generated rows first or last would have lost both.

So: `keys.ts` keeps its hand-written table and exports `documentedKeys()`, and
`keys-dom.test.ts` fails if any registry command has a chord the sheet does not
document. Same guarantee against drift, none of the coupling — and it is
honest about which file is the source of truth for prose and which for verbs.

**Done when:** adding a command with a chord and forgetting the sheet turns the
suite red, naming the chord and the command that is missing.

## Out of scope

- Sigil routing (`>`, `#`, `@`, `:`, `?`). Designed, deliberately deferred —
  the plain ranked list has to be good first, or the sigils are a workaround
  for bad ranking.
- Any Rust change: ingest cancellation, card deletion, virtual collections.
- Search-result rows inside the palette. That is ⌘F's job by the thesis.

## Verification

1. `npm --prefix apps/web run typecheck` and `npm --prefix apps/web test`
   green, with new tests for the matcher, the grouping caps, and the stage
   stack (pure), plus a `chrome-fixture` DOM test per phase.
2. Key-stack regression, by hand and worth doing every phase: ⌘K from inside
   `#q`; from the reader with markup mode on; over perf, atlas and settings.
   In each case Escape returns exactly to that state, not past it.
3. The Escape ladders that already exist still work with the palette closed —
   markup popover → markup mode → drawer → reader is the one to check.
4. `c` and `?` do not fire while the palette input has focus.
5. End-to-end through the `verify` skill against `library-server`: open ⌘K,
   jump to a book by title, reshelve it, confirm the shelf grid agrees.
6. A11y: `role="listbox"` with `aria-activedescendant` tracking the selection,
   the dialog labelled, focus restored on close.

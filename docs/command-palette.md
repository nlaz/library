# The card catalog (⌘K)

**Status: shipped.** The build lives in `apps/web/src/palette.ts` and the files
beside it; the order it was built in is [plans/command-palette.md](plans/command-palette.md).
This document is the argument for the shape, and the record of what was
rejected. Where the build differs from the diagrams below, the plan notes the
change and the reason — the three that matter are: Escape and ⌘K live in
`palette.ts` rather than `main.ts`; there is no "Ask the librarian" handoff
while chat is deliberately off; and the shortcut sheet is *checked* against
the registry rather than generated from it.

## Why this app wants one

`apps/web/src/keys.ts` opens with an admission:

> Every key this app answers to is discoverable nowhere else — the chrome is
> deliberately bare, so this list is the only place the vocabulary is written
> down. It is data, not markup, for exactly that reason: a shortcut added in a
> feature module and not added here is the bug this file exists to make
> obvious.

That file is already a command registry. It is data, it is grouped, it is
keyed — it just cannot execute anything, and nothing enforces that it stays
true. A palette is the version of `keys.ts` that runs, and registration stops
being a courtesy: a command that is not in the registry is not invokable, so
it cannot drift out of the list.

The bare chrome is a deliberate choice and this does not undo it. The palette
adds one door, not a toolbar; it is summoned and it goes away, and everything
behind it is still reached the same way it was before.

## The thesis: ⌘K is not another search box

⌘F is already a real search: BM25 and HNSW over page text, CLIP over figures,
streamed, with inline completion. If ⌘K is also "a box you type into", the app
has two of them and every user guesses wrong at least once.

```
        ⌘F — into the books              ⌘K — around the library
       ─────────────────────────        ──────────────────────────
        hybrid lexical + semantic        fuzzy match over the names
        over page text and figures       of things and of verbs
        "where is this said?"            "take me there" / "do this"
        a result is a page               a result is a place or an act
        engine round-trip, streamed      local, instant, no engine
```

⌘F searches *content*. ⌘K searches *the furniture* — documents by title,
shelves, notes, settings, and every command in the app. In this app's own
register that second thing has a name: it is the **card catalog**, the drawer
you go to when you know what you want but not where it is shelved. That is
the name in the header, and the reason the surface exists.

The two must never dead-end into each other's territory. A ⌘K query that
matches nothing offers to hand itself to ⌘F, and a ⌘F result that is really a
navigation ("open this book") is already a click away. Neither pretends to be
the other.

## Anatomy

Square corners (`--r: 0px`), a dither scrim, a `.divot` bezel, mono for
metadata. It should read as another drawer in the same cabinet as `#keys` and
`#settings`, not as a Raycast transplant dropped into a warm-gray app.

```
┌──────────────────────────────────────────────────────────┐
│  card catalog                                         ✕  │
├──────────────────────────────────────────────────────────┤
│                                                          │
│   ▸ _                                                    │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  RECENT                                                  │
│    Kittler · Gramophone, Film, Typewriter      read p.88 │
│    Sontag · On Photography                     read p.12 │
│                                                          │
│  COMMANDS                                                │
│    Add books…                                         ⌘O │
│    Settings                                           ⌘S │
│    Start a note                                        c │
├──────────────────────────────────────────────────────────┤
│  ↑↓ move    ⏎ open    ⇥ drill in           esc close     │
└──────────────────────────────────────────────────────────┘
```

The footer earns its space here in a way it would not in most apps. Every row
that has a direct chord shows it, so the palette teaches the bare chrome while
it is being used, and a user graduates off it. That is the same job the
first-run panel does for `?`, done continuously.

## What it holds

Four kinds of row, ranked into one list, grouped under mono labels:

```
   BOOKS      every doc, by title → open in the reader
   SHELVES    every collection    → filter the library to it
   NOTES      every card          → open in the ledger
   COMMANDS   every verb          → run it
```

Plain typing searches all four. A leading sigil narrows to one drawer — the
expert path, never the only path:

```
   type…      the palette becomes               backed by
  ──────────────────────────────────────────────────────────────
   (empty)    recents + likely next steps       nav trail, docs
   plain      everything, ranked                all of the below
   >          commands only                     the registry
   #          shelves and collections           collections
   @          notes and cards                   list_cards
   :          pages within the open book        reader state
   ?          the shortcut vocabulary           the registry
```

Sigils are a phase-2 luxury. The plain ranked list is the feature.

## Jump to a book by name

The highest-value single behavior, and it needs no new backend: `docs` already
returns id, title, name, shelf, page count and ingest status, and `format.ts`
already owns the shared `docList` that every other module reads through.

```
┌──────────────────────────────────────────────────────────┐
│   ▸ gramo_                                               │
├──────────────────────────────────────────────────────────┤
│  BOOKS                                                   │
│  ▸ Gramophone, Film, Typewriter         Media Theory 315p│
│    Grammars of Creation                    Criticism 288p│
│                                                          │
│  NOTES                                                   │
│    the gramophone as a writing machine        3 days ago │
│                                                          │
│  ─────────────────────────────────────────────────────── │
│    Search the library for "gramo"                     ⇧⏎ │
└──────────────────────────────────────────────────────────┘
```

That last row is the load-bearing one. The palette never shows an empty state
with nothing to do in it; when it has no answer of its own it hands the query
to the thing that does.

```
┌──────────────────────────────────────────────────────────┐
│   ▸ what does deleuze say about the fold_                │
├──────────────────────────────────────────────────────────┤
│  no book, note, or command by that name                  │
│                                                          │
│    Search the library for it                          ⏎  │
│    Ask the librarian                                 ⇧⏎  │
└──────────────────────────────────────────────────────────┘
```

The librarian row is conditional on `chat_status`, exactly like the header
button: absent rather than dead on a Mac without Apple Foundation Models.

## It knows where you are

This is what separates a palette from a menu. The same keystroke in the reader
offers the verbs that apply to the book in front of you — all of which already
exist as commands (`set_title`, `move_to_shelf`, `reveal_doc`, `delete_doc`).

```
  reading  #/read/kittler-gramophone?p=88
┌──────────────────────────────────────────────────────────┐
│   ▸ _                                                    │
├──────────────────────────────────────────────────────────┤
│  THIS BOOK — Gramophone, Film, Typewriter                │
│    Markup mode                                        ⌘U │
│    Show marks                                          m │
│    Jump to page…                                       : │
│    Rename…                                            ⇥  │
│    File into a collection…                            ⇥  │
│    Show in Finder                                        │
│                                                          │
│  LIBRARY                                                 │
│    Back to the shelves                               esc │
│    Start a note here                                   c │
└──────────────────────────────────────────────────────────┘
```

Context changes only what is *offered first*. Everything global stays
reachable by typing — the palette narrows the default, never the vocabulary.

## Drilling in, rather than spawning dialogs

Rows marked `⇥` push a second stage into the same box; `⌫` on an empty input
pops back out. This is how "move to shelf" and "rename" happen without
inventing new modals, and it is the only place the UI can offer to *create* a
collection — which today requires knowing that a shelf is a depth-1 folder.

```
  stage 1                            ⇥
 ┌───────────────────────────────┐  ────▶
 │  File into a collection…   ⇥  │
 └───────────────────────────────┘

  stage 2   card catalog  ›  File into ▸ media_       ⌫ pops back
 ┌──────────────────────────────────────────────────────────┐
 │    Media Theory                                 14 books │
 │    Media Archaeology                             6 books │
 │  ─────────────────────────────────────────────────────── │
 │    + New collection "media_"                             │
 └──────────────────────────────────────────────────────────┘
```

A stage is a stack, not a route: the hash does not change until a stage
commits, so cancelling out of one leaves no trace on the nav trail.

## Surfacing state the chrome currently hides

Ingest status is per-doc and real (`queued`, `preparing`, `staged`,
`text_ready`, `ready`, `failed`) but you only see it if you happen to be
looking at the right shelf card. The empty palette is a good place for the
library to say what it is doing, and the only place a `failed` book can be
retried without hunting for it.

```
┌──────────────────────────────────────────────────────────┐
│   ▸ _                                                    │
├──────────────────────────────────────────────────────────┤
│  ⣿⣿⣿⣿⣿⣿⣶⣤⣀  3 books indexing · 1 needs attention        │
│                                                          │
│    Berger · Ways of Seeing            ocr    page 41/176 │
│    Anon scan 2024-11-03               failed   Retry  ⏎  │
├──────────────────────────────────────────────────────────┤
│  RECENT                                                  │
└──────────────────────────────────────────────────────────┘
```

This section is present only when there is something to report. An empty
palette on a settled library shows recents and nothing else.

## The layer stack — the actual hard part

The app has a carefully ordered Escape stack and four overlays that each own a
global chord. Where ⌘K sits in `main.ts`'s capture handler is the real design
decision; the visuals are the easy half.

```
  z    layer               chord     ⌘K here should…
 ─────────────────────────────────────────────────────────────
 16    card catalog        ⌘K        (top of both stacks)
 15    perf                ⌘.        open over it
 14    settings            ⌘S        open over it
 14    keys sheet          ?         open over it
 13    atlas               ⌘/        open over it
 11    chat                          open over it
 10    page viewer                   offer this page's verbs
  9    search popover      ⌘F        hand off, never compete
 ──    reader/notes/sheet/home       offer that surface's verbs
```

Three rules, and they are the ones to hold on to under review pressure:

1. **⌘K is checked first**, above the `⌘.` branch. Perf owns the top of the
   key stack today; the palette takes that spot because it must open from
   inside a text field, and from over perf, without exception.
2. **Escape from the palette pops only the palette.** It never unwinds a layer
   underneath, and the surface that had focus gets it back. A new Escape
   branch goes *before* the settings/keys/perf/atlas branches.
3. **The palette does not re-implement any verb.** Every row calls the same
   function the button or chord calls. If a row and a chord can disagree,
   the row is wrong.

Two guards fall out for free. `?` and `c` already refuse to fire when the
event target is an `HTMLInputElement`, and the perf `1234` branch already
skips `INPUT` — so a real `<input>` in the palette is safe from all three
without touching them. That is a reason to use a real input rather than a
contenteditable.

## Consequences for `keys.ts`

Once commands are a registry with labels, groups and chords, `keys.ts`'s
`GROUPS` should be *derived* from it rather than maintained beside it. The `?`
sheet keeps its own rows for the things that are not commands — drag and drop,
`[[`, "click a book" — and takes the rest from the registry. The comment at
the top of the file stops describing a hazard and starts describing a
mechanism.

## Rejected

- **Making ⌘K the content search.** It would duplicate ⌘F, and the good
  version of it already exists with completion, kind cycling and streaming.
  Rejected in favour of the furniture/content split above.
- **Replacing ⌘F with ⌘K entirely** (one box, mode-switched). Tempting, and
  wrong for this app: reader find is a different scope with its own
  Enter/⇧Enter semantics and its own popover, and collapsing them would cost
  the in-reader find flow to buy consistency nobody asked for.
- **Retiring the `?` sheet.** The palette is a better index but a worse poster.
  `?` is one keystroke that shows everything at once with no typing, which is
  what a newcomer needs; the palette is for someone who already knows what
  they want. They share data, not a surface.
- **A header button for the palette.** The chrome is bare on purpose. The way
  in is the key, said out loud by the first-run panel and the palette footer.
- **Fuzzy-matching page content locally.** Out of scope by the thesis, and the
  corpus is far too large for a client-side index.

## Known limits

Three things the palette will want and the backend cannot do yet: cancelling
an in-flight ingest, deleting a card (only archive, via `filed: true`), and
creating or renaming a collection without moving files, since a shelf *is* a
depth-1 folder. The first two are small additions; the third is a real design
question about whether collections should ever be virtual, and is out of scope
here.

The web build degrades: `docs`, roots, reveal, delete, retry and rename are
Tauri-only. The palette filters its registry by host the same way the drawer
already takes `edit: desktop ? … : null`, and on web the BOOKS rows come from
`collections()` with `docTitle()`'s prettified fallback.

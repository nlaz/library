// ---------------------------------------------------------------------------
// keys: the shortcut sheet behind the `?` key. It has no glyph in the
// header — the way in is the key itself, said out loud by the first-run
// panel and nowhere else.
//
// Every key this app answers to is discoverable nowhere else — the chrome is
// deliberately bare, so this list is the only place the vocabulary is
// written down. It is data, not markup, for exactly that reason: a shortcut
// added in a feature module and not added here is the bug this file exists
// to make obvious.
// ---------------------------------------------------------------------------

import { chatUnavailableReason } from "./chat";

type Row = [keys: string, what: string];
type Group = { title: string; rows: Row[] };

const GROUPS: Group[] = [
  {
    title: "Search",
    rows: [
      ["⌘F", "Search the library — press again to cycle everything → figures → text & notes"],
      ["Esc", "Close the search box and clear the query"],
      ["Enter / ⇧Enter", "Next / previous match, while reading"],
    ],
  },
  {
    title: "Library",
    rows: [
      ["drag & drop", "Add PDFs or images — anywhere on the window"],
      ["⌘O", "Add books with the file chooser"],
      ["click a book", "Open it in the reader"],
      ["⋯", "Rename, file into a collection, or delete"],
    ],
  },
  {
    title: "Reader",
    rows: [
      ["Space / ⇟ ⇞", "Page down / up"],
      ["Home / End", "Jump to the first / last page"],
      ["⌘U", "Markup mode — draw on the page"],
      ["m", "Show or hide existing marks"],
      ["ⓘ", "Title, collections, and the marks you've made"],
      ["Esc", "One step back: the drawer, then the book you came from"],
    ],
  },
  {
    title: "Notes",
    rows: [
      ["c", "Start a note, from any surface — Esc returns you to it"],
      ["j / k", "Walk down / up the ledger"],
      ["Enter", "Write in the open entry (double-clicking it does too)"],
      ["[[", "Link another note while writing"],
      ["Esc", "One step back: the open entry, the filters, then out"],
    ],
  },
  {
    title: "Views",
    rows: [
      ["⌘S", "Settings — your folders, the librarian, and updates"],
      ["⌘.", "Performance — 1 / 2 / 3 / 4 pick a section"],
      ["⌘/", "Corpus atlas"],
      ["?", "This list"],
      ["Esc", "Close whichever of them is open"],
    ],
  },
];

const $sheet = document.getElementById("keys")!;
const $body = document.getElementById("keys-body")!;

export function keysOpen(): boolean {
  return !$sheet.hidden;
}

export function toggleKeys() {
  if (keysOpen()) closeKeys();
  else openKeys();
}

export function closeKeys() {
  $sheet.hidden = true;
}

function openKeys() {
  if (!$body.childElementCount) render();
  $sheet.hidden = false;
}

function render() {
  $body.replaceChildren(
    ...GROUPS.map((g) => {
      const sec = document.createElement("section");
      const h = document.createElement("h3");
      h.textContent = g.title;
      const dl = document.createElement("dl");
      for (const [keys, what] of g.rows) {
        const dt = document.createElement("dt");
        // one <kbd> per token so each key gets its own cap; the separators
        // between them ("/", "+") stay outside, unstyled
        for (const part of keys.split(/(\s*[/+]\s*)/)) {
          if (/^\s*[/+]\s*$/.test(part)) dt.append(part);
          else if (part.trim()) {
            const k = document.createElement("kbd");
            k.textContent = part.trim();
            dt.append(k);
          }
        }
        const dd = document.createElement("dd");
        dd.textContent = what;
        dl.append(dt, dd);
      }
      sec.append(h, dl);
      return sec;
    }),
  );

  // The librarian is removed from the chrome on machines without Apple
  // Intelligence. Say so here rather than leaving an unexplained gap where
  // a screenshot or a friend's Mac shows a button this one doesn't have.
  const why = chatUnavailableReason();
  if (why) {
    const note = document.createElement("p");
    note.className = "keys-note";
    note.textContent = why;
    $body.append(note);
  }
}

document.getElementById("keys-close")!.addEventListener("click", closeKeys);
$sheet.addEventListener("click", (e) => {
  if (e.target === $sheet) closeKeys(); // click the scrim, not the card
});

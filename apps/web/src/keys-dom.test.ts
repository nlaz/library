// The shortcut sheet's one structural obligation: it documents every chord
// the card catalog can run.
//
// keys.ts opens by naming its own weakness — a shortcut added in a feature
// module and not added there is the bug the file exists to make obvious — and
// for years the only thing enforcing that was whoever wrote the commit. For
// the subset of keys that are commands, this test is that enforcement.
//
// The sheet is not generated from the registry, deliberately. Most of what it
// documents is not a command (the reader's scroll keys, `[[`, the Escape
// ladders), and its prose is longer and warmer than a command label should
// be. Checking is the right relationship between the two, not merging.

import { beforeAll, expect, it } from "vitest";
import { mountChrome } from "./chrome-fixture";

let keys: typeof import("./keys");
let commands: typeof import("./commands");

beforeAll(async () => {
  await mountChrome();
  keys = await import("./keys");
  commands = await import("./commands");
});

it("documents every chord the catalog can run", () => {
  const documented = new Set(keys.documentedKeys());
  const missing = commands
    .allCommands()
    .filter((c) => c.chord && !documented.has(c.chord))
    .map((c) => `${c.chord} (${c.label})`);

  expect(missing).toEqual([]);
});

it("has a row for the catalog itself", () => {
  // ⌘K cannot be a command — it opens the box the commands live in — so it is
  // the one entry no registry check would catch
  expect(keys.documentedKeys()).toContain("⌘K");
});

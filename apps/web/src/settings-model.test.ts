import { describe, expect, it } from "vitest";
import {
  activeSection,
  baseName,
  formatBytes,
  librarianRow,
  PINNED_NOTE,
  type RootInfo,
  rootRows,
  SECTIONS,
  storageRows,
  tildify,
  unlinkWarning,
  type UpdateState,
  updateRow,
} from "./settings-model";

const HOME = "/Users/someone";

function root(over: Partial<RootInfo> = {}): RootInfo {
  return {
    id: "r1",
    path: `${HOME}/The Library`,
    is_default: true,
    state: "watching",
    added_at: 1,
    last_scan_at: 2,
    docs: 3,
    available: true,
    ...over,
  };
}

describe("tildify", () => {
  it("writes paths the way a Mac app does", () => {
    expect(tildify(`${HOME}/The Library`, HOME)).toBe("~/The Library");
    expect(tildify(HOME, HOME)).toBe("~");
  });

  it("leaves paths outside home alone", () => {
    expect(tildify("/Volumes/Archive/Scans", HOME)).toBe("/Volumes/Archive/Scans");
  });

  it("does not mangle a sibling directory that merely shares a prefix", () => {
    // /Users/someone-else must not become ~-else
    expect(tildify("/Users/someone-else/Docs", HOME)).toBe("/Users/someone-else/Docs");
  });

  it("survives a home path with a trailing slash", () => {
    expect(tildify(`${HOME}/The Library`, `${HOME}/`)).toBe("~/The Library");
  });
});

describe("rootRows", () => {
  it("counts documents in plain language", () => {
    const [one, many, none] = rootRows(
      [
        root({ id: "a", docs: 1 }),
        root({ id: "b", docs: 1234 }),
        root({ id: "c", docs: 0 }),
      ],
      HOME,
    );
    expect(one.status).toBe("1 document");
    expect(many.status).toBe("1,234 documents");
    expect(none.status).toBe("No documents yet");
  });

  it("distinguishes an empty folder from one it cannot see", () => {
    // the whole reason the list shows state: an ejected drive still has
    // its documents, and must not read as an empty folder
    const [row] = rootRows([root({ docs: 412, available: false })], HOME);
    expect(row.ok).toBe(false);
    expect(row.status).toContain("Can't see this folder");
    expect(row.status).toContain("412 documents kept");
  });

  it("never offers to unlink the only folder", () => {
    const [solo] = rootRows([root()], HOME);
    expect(solo.canUnlink).toBe(false);

    const two = rootRows([root({ id: "a" }), root({ id: "b", is_default: false })], HOME);
    expect(two.every((r) => r.canUnlink)).toBe(true);
  });

  it("marks exactly the default", () => {
    const rows = rootRows(
      [root({ id: "a", is_default: true }), root({ id: "b", is_default: false })],
      HOME,
    );
    expect(rows.filter((r) => r.isDefault)).toHaveLength(1);
  });
});

describe("formatBytes", () => {
  it("is coarse on purpose", () => {
    expect(formatBytes(15_200_000_000)).toBe("15 GB");
    expect(formatBytes(2_500_000_000)).toBe("2.5 GB");
    expect(formatBytes(703_000_000)).toBe("703 MB");
  });

  it("never shows a scary zero for a small non-empty cache", () => {
    expect(formatBytes(400_000)).toBe("1 MB");
    expect(formatBytes(0)).toBe("0 MB");
    expect(formatBytes(Number.NaN)).toBe("0 MB");
  });
});

describe("storageRows", () => {
  const base = {
    path: "/x",
    derived_bytes: 15_000_000_000,
    index_bytes: 800_000_000,
    model_bytes: 400_000_000,
  };

  it("leads with the renders, which dominate", () => {
    const rows = storageRows(base);
    expect(rows[0]).toEqual(["Page images and text", "15 GB"]);
    expect(rows).toHaveLength(3);
  });

  it("shows the renders against their budget once there is one", () => {
    const rows = storageRows({
      ...base,
      page_bytes: 3_200_000_000,
      page_budget_bytes: 4_000_000_000,
      pinned_bytes: 0,
    });
    expect(rows[0]).toEqual(["Page images", "3.2 GB of 4.0 GB"]);
    // the rest of `derived` is the non-evictable text side
    expect(rows[1]).toEqual(["Text and notes", "12 GB"]);
  });

  it("says so when the budget is off rather than implying one", () => {
    const rows = storageRows({ ...base, page_bytes: 3_200_000_000, page_budget_bytes: 0 });
    expect(rows[0]).toEqual(["Page images", "3.2 GB (no limit)"]);
  });

  // The one line on this pane that is not re-creatable. It gets its own row
  // rather than hiding inside a total that offers to free it, because the
  // pane's closing promise is false of exactly these bytes.
  it("breaks out renders that are the only copy, and only when there are some", () => {
    const rows = storageRows({
      ...base,
      page_bytes: 3_200_000_000,
      page_budget_bytes: 4_000_000_000,
      pinned_bytes: 282_000_000,
    });
    expect(rows[1]).toEqual(["Kept for missing files", "282 MB"]);

    const none = storageRows({ ...base, page_bytes: 3_200_000_000, pinned_bytes: 0 });
    expect(none.map(([l]) => l)).not.toContain("Kept for missing files");
  });

  it("warns that pinned renders are the only copy", () => {
    expect(PINNED_NOTE).toMatch(/only copy/);
    expect(PINNED_NOTE).toMatch(/never removes those on its own/);
  });
});

describe("unlinkWarning", () => {
  it("says the files are safe, because that is the whole question", () => {
    const [row] = rootRows([root({ path: "/Volumes/Archive/Scans" })], HOME);
    const msg = unlinkWarning(row);
    expect(msg).toContain("Scans");
    expect(msg).toMatch(/files stay exactly where they are/);
    expect(msg).toMatch(/notes are kept/);
  });
});

describe("baseName", () => {
  it("names the folder, trailing slash or not", () => {
    expect(baseName("/Volumes/Archive/Scans")).toBe("Scans");
    expect(baseName("/Volumes/Archive/Scans/")).toBe("Scans");
  });
});

describe("librarianRow", () => {
  const REASON = "The librarian needs macOS 26 or newer — the rest of the library works here.";

  it("is off by default on a Mac that could run it", () => {
    const r = librarianRow({ supported: true, reason: null, enabled: false });
    expect(r).toMatchObject({ on: false, can: true, label: "off", warn: null });
  });

  it("is on once asked for", () => {
    const r = librarianRow({ supported: true, reason: null, enabled: true });
    expect(r).toMatchObject({ on: true, can: true, label: "on", warn: null });
  });

  it("cannot be pressed where the models are missing", () => {
    const r = librarianRow({ supported: false, reason: REASON, enabled: false });
    expect(r.can).toBe(false);
    expect(r.warn).toBe(REASON);
  });

  it("reads off on an unsupported Mac even when the preference says on", () => {
    // the panel is not going to open — a switch left on would be a lie,
    // and the preference survives for a Mac that can honour it later
    const r = librarianRow({ supported: false, reason: REASON, enabled: true });
    expect(r.on).toBe(false);
    expect(r.label).toBe("off");
  });

  it("always has something to say when it cannot run", () => {
    // the probe can decline to give a reason; silence next to a dead
    // control is the one outcome that explains nothing
    const r = librarianRow({ supported: false, reason: null, enabled: false });
    expect(r.warn).toBeTruthy();
    expect(r.warn).toContain("macOS 26");
  });
});

describe("activeSection", () => {
  // four sections, each 600px tall, in a 500px-tall scroller
  const offsets = [0, 600, 1200, 1800];
  const scroll = { height: 500, total: 2400 };

  it("lights the section whose heading has passed the reading line", () => {
    expect(activeSection(0, offsets, scroll)).toBe(0);
    expect(activeSection(540, offsets, scroll)).toBe(1);
    expect(activeSection(1150, offsets, scroll)).toBe(2);
  });

  it("does not light a section whose heading is still below the line", () => {
    // 519 + 80 = 599, one pixel short of the second section
    expect(activeSection(519, offsets, scroll)).toBe(0);
  });

  it("lights the last section at the bottom of the scroll", () => {
    // the exception that makes the rail's last entry reachable: a final
    // section shorter than the viewport never gets its heading to the line
    const short = [0, 600, 1200, 1900];
    expect(activeSection(1400, short, { height: 500, total: 1900 })).toBe(3);
  });

  it("does not claim the bottom when there is nothing to scroll", () => {
    expect(activeSection(0, offsets, { height: 900, total: 900 })).toBe(0);
  });

  it("survives a page that has not been measured yet", () => {
    expect(activeSection(0, [])).toBe(0);
    expect(activeSection(0, [0])).toBe(0);
  });
});

describe("updateRow", () => {
  it("offers a check when nothing has happened yet", () => {
    expect(updateRow({ at: "idle" }).action).toBe("Check for updates");
  });

  it("offers nothing to press while something is in flight", () => {
    // there is no second press that helps, and a greyed button invites
    // waiting for it to come back
    expect(updateRow({ at: "checking" }).action).toBeNull();
    const dl: UpdateState = { at: "downloading", version: "0.2.0", downloaded: 0, total: 100 };
    expect(updateRow(dl).action).toBeNull();
  });

  it("never reports a failed check as up to date", () => {
    // the one wrong answer here that someone would act on
    const r = updateRow({ at: "failed", why: "the network is down" });
    expect(r.status).not.toMatch(/up to date/i);
    expect(r.detail).toBe("the network is down");
    expect(r.action).toBe("Try again");
  });

  it("names the version it found, and shows its notes", () => {
    const r = updateRow({ at: "found", version: "0.2.0", notes: "faster search" });
    expect(r.status).toContain("0.2.0");
    expect(r.action).toBe("Update");
    expect(r.detail).toBe("faster search");
  });

  it("measures the download when it can", () => {
    const r = updateRow({
      at: "downloading",
      version: "0.2.0",
      downloaded: 20_000_000,
      total: 40_000_000,
    });
    expect(r.progress).toBeCloseTo(0.5);
    expect(r.status).toContain("20 MB of 40 MB");
  });

  it("shows no bar when the size is unknown", () => {
    // a bar that can't say how far along it is would only be an animation
    const r = updateRow({
      at: "downloading",
      version: "0.2.0",
      downloaded: 20_000_000,
      total: null,
    });
    expect(r.progress).toBeNull();
    expect(r.status).toBe("Downloading 0.2.0");
  });

  it("does not overrun a bar when the body is longer than advertised", () => {
    const r = updateRow({ at: "downloading", version: "0.2.0", downloaded: 150, total: 100 });
    expect(r.progress).toBe(1);
  });

  it("says the update is already installed before asking to restart", () => {
    // the restart is a convenience, not the installation — someone who
    // ignores it must not think they lost the download
    const r = updateRow({ at: "staged", version: "0.2.0" });
    expect(r.status).toContain("installed");
    expect(r.action).toBe("Restart now");
    expect(r.detail).toMatch(/next time the app opens/);
  });
});

describe("SECTIONS", () => {
  it("has a label for every id, and no duplicates", () => {
    const ids = SECTIONS.map((s) => s.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(SECTIONS.every((s) => s.label.length > 0)).toBe(true);
  });
});

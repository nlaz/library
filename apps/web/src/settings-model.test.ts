import { describe, expect, it } from "vitest";
import {
  baseName,
  formatBytes,
  type RootInfo,
  rootRows,
  storageRows,
  tildify,
  unlinkWarning,
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
  it("leads with the renders, which dominate", () => {
    const rows = storageRows({
      path: "/x",
      derived_bytes: 15_000_000_000,
      index_bytes: 800_000_000,
      model_bytes: 400_000_000,
    });
    expect(rows[0]).toEqual(["Page images and text", "15 GB"]);
    expect(rows).toHaveLength(3);
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

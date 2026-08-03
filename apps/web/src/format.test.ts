import { beforeEach, describe, expect, it } from "vitest";
import { displayTitle, docTitle, prettify, setDocList, stageCount } from "./format";
import type { DocInfo } from "./types";

const doc = (over: Partial<DocInfo> & { id: string }): DocInfo => ({
  title: null,
  pages: 10,
  collections: [],
  processing: false,
  status: null,
  ...over,
});

describe("prettify", () => {
  it("title-cases kebab-case ids", () => {
    expect(prettify("moby-dick")).toBe("Moby Dick");
    expect(prettify("gardening-encyclopedia-1911")).toBe("Gardening Encyclopedia 1911");
  });

  it("leaves words of two characters or fewer untouched", () => {
    expect(prettify("art-of-war")).toBe("Art of War");
    expect(prettify("a-la-carte")).toBe("a la Carte");
  });

  it("passes through 'Title (Author) (source)'-shaped ids, capitalizing the first word only", () => {
    // no dashes -> the whole id is a single "word": first letter upper-cased,
    // the rest (including the parenthesized author/source) kept verbatim
    expect(prettify("The Iliad (Homer) (archive.org)")).toBe("The Iliad (Homer) (archive.org)");
    expect(prettify("catalogue of birds (Sharpe) (bhl)")).toBe("Catalogue of birds (Sharpe) (bhl)");
  });

  it("capitalizes opaque scan ids as one word (underscores untouched)", () => {
    expect(prettify("bub_gb_wTLLxvVeyEIC")).toBe("Bub_gb_wTLLxvVeyEIC");
    expect(prettify("b29326679_0002")).toBe("B29326679_0002");
  });

  it("keeps empty segments from doubled dashes empty", () => {
    // "a--b" splits to ["a", "", "b"]; the short-word guard leaves "" alone
    expect(prettify("foo--bar")).toBe("Foo  Bar");
  });
});

describe("displayTitle", () => {
  it("prefers the stored title", () => {
    expect(displayTitle(doc({ id: "moby-dick", title: "Moby-Dick; or, The Whale" }))).toBe(
      "Moby-Dick; or, The Whale",
    );
  });

  it("falls back to the prettified id when the title is null", () => {
    expect(displayTitle(doc({ id: "moby-dick" }))).toBe("Moby Dick");
  });

  it("names an untitled book after its file, not its minted id", () => {
    // the shelf before this read "D01713Fa82Ad0" for every book the user
    // had not renamed by hand
    expect(displayTitle(doc({ id: "d01713fa82ad0", name: "Artusi 1891" }))).toBe("Artusi 1891");
    // shown verbatim: prettify is a slug rule, and these hyphens belong to
    // whoever named the file
    expect(
      displayTitle(doc({ id: "d0be9d5137367", name: "Julia Child - Mastering the Art" })),
    ).toBe("Julia Child - Mastering the Art");
    // the download tag came off in library-core::naming, before the name
    // ever reached the browser — this side does not second-guess it
    expect(displayTitle(doc({ id: "d1e6b398c12bc", name: "A taste of India" }))).toBe(
      "A taste of India",
    );
    // a title the user set still wins over the file it came from
    expect(
      displayTitle(doc({ id: "d0be9d5137367", name: "escoffier_ocr_v2", title: "Le Guide Culinaire" })),
    ).toBe("Le Guide Culinaire");
    // the browser build sends no name at all
    expect(displayTitle(doc({ id: "moby-dick", name: null }))).toBe("Moby Dick");
  });
});

describe("docTitle", () => {
  beforeEach(() => setDocList([]));

  it("resolves a known doc through displayTitle", () => {
    setDocList([doc({ id: "moby-dick", title: "Moby-Dick; or, The Whale" })]);
    expect(docTitle("moby-dick")).toBe("Moby-Dick; or, The Whale");
  });

  it("uses the prettified-id fallback for a known doc without a title", () => {
    setDocList([doc({ id: "art-of-war" })]);
    expect(docTitle("art-of-war")).toBe("Art of War");
  });

  it("prettifies the raw id for unknown docs", () => {
    expect(docTitle("secret-garden")).toBe("Secret Garden");
  });
});

describe("stageCount", () => {
  it("counts pages for ordinary pipeline stages", () => {
    expect(stageCount("ocr", 12, 300)).toBe(" 12/300");
  });

  it("counts the one-time model fetch in MB, not bytes", () => {
    expect(stageCount("download", 84 << 20, 335 << 20)).toBe(" 84/335 MB");
  });

  it("says nothing when there is no denominator", () => {
    expect(stageCount("indexing", 0, 0)).toBe("");
  });
});

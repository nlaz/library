// OCR cleanup helper for The Library, backed by the on-device Apple
// Foundation Model. Reads cached OCR pages, asks the model for short
// {original, corrected} repairs, gates them mechanically, then re-judges
// each survivor with an A/B forced-choice pass (the model is a poor
// yes/no verifier of its own edits but a good context-based chooser
// between two readings — measured 9/10 vs 4/10 on a labeled sample).
//
//   clean-pages --ocr-dir data/ocr/<doc> --out-dir data/edits/<doc> [--pages 1,5]
//
// Per page it writes <out-dir>/page-NNNN.json:
//   {"page": N, "edits": [{"original": s, "corrected": s, "verified": b}]}
// where 'original' quotes the page's dehyphenated text (see fusedTokens —
// the Rust applier re-derives the same fusion and anchors by exact match,
// so any divergence just voids the edit). Pages with an existing edits
// file are skipped; progress goes to stdout as "clean <done>/<total>".
//
// Built with DynamicGenerationSchema throughout: the @Generable macro
// plugin does not ship with CommandLineTools.

import Foundation
import FoundationModels

// MARK: - OCR page IO

struct W: Codable { let t: String }
struct Page: Codable { let page: Int; let words: [W] }

struct EditOut: Codable {
    let original: String
    let corrected: String
    let verified: Bool
}
struct PageOut: Codable { let page: Int; let edits: [EditOut] }

// MARK: - Text shaping (mirrors the Rust side; divergence only voids edits)

/// Lowercased alphanumeric tokens of length > 1 — `library_core::tokenize`.
func tokenize(_ s: String) -> [String] {
    s.split(whereSeparator: \.isWhitespace).compactMap { t in
        let clean = String(t.unicodeScalars.filter { CharacterSet.alphanumerics.contains($0) }).lowercased()
        return clean.count > 1 ? clean : nil
    }
}

/// Fuse hyphenated line breaks: a token ending in a lone '-' followed by a
/// lowercase-initial token. Drops the hyphen when the fused word occurs
/// elsewhere in the document, keeps it otherwise (compounds).
func fusedTokens(_ words: [String], vocab: Set<String>) -> [String] {
    var out: [String] = []
    for w in words {
        if let prev = out.last, prev.count > 1, prev.hasSuffix("-"), !prev.hasSuffix("--"),
           let c = w.first, c.isLowercase {
            let fused = String(prev.dropLast()) + w
            let known = tokenize(fused).first.map { vocab.contains($0) } ?? false
            out[out.count - 1] = known ? fused : prev + w
            continue
        }
        out.append(w)
    }
    return out
}

func levenshtein(_ a: String, _ b: String) -> Int {
    let a = Array(a.unicodeScalars), b = Array(b.unicodeScalars)
    if a.isEmpty { return b.count }
    var row = Array(0...b.count)
    for (i, ca) in a.enumerated() {
        var prev = row[0]
        row[0] = i + 1
        for (j, cb) in b.enumerated() {
            let cur = row[j + 1]
            row[j + 1] = min(row[j] + 1, cur + 1, prev + (ca == cb ? 0 : 1))
            prev = cur
        }
    }
    return row[b.count]
}

/// Mechanical gate: short, anchored, near-miss edits only. The model must
/// not be able to rewrite prose wholesale no matter what it returns.
func gate(original: String, corrected: String, text: String) -> Bool {
    if original == corrected { return false }
    if original.count > 40 || original.isEmpty || corrected.isEmpty { return false }
    if !text.contains(original) { return false }
    // compare ignoring spaces/hyphens so pure join/split repairs pass
    let squash = { (s: String) in s.replacingOccurrences(of: " ", with: "").replacingOccurrences(of: "-", with: "") }
    let (o, c) = (squash(original), squash(corrected))
    if o == c { return true } // pure join/split
    return levenshtein(o, c) <= max(2, o.count / 4)
}

// MARK: - Model passes

func makeFindSchema() throws -> GenerationSchema {
    let edit = DynamicGenerationSchema(
        name: "Edit",
        properties: [
            .init(name: "original",
                  description: "The damaged text, quoted EXACTLY as it appears in the input",
                  schema: DynamicGenerationSchema(type: String.self)),
            .init(name: "corrected",
                  description: "The repaired text",
                  schema: DynamicGenerationSchema(type: String.self)),
        ]
    )
    let root = DynamicGenerationSchema(
        name: "PageEdits",
        properties: [
            .init(name: "edits",
                  description: "OCR damage repairs; empty if the text is clean",
                  schema: DynamicGenerationSchema(arrayOf: DynamicGenerationSchema(referenceTo: "Edit")))
        ]
    )
    return try GenerationSchema(root: root, dependencies: [edit])
}

func makeVerifySchema() throws -> GenerationSchema {
    let root = DynamicGenerationSchema(
        name: "Verdict",
        properties: [
            .init(name: "answer",
                  description: "Which snippet the printed page actually showed",
                  schema: DynamicGenerationSchema(name: "Choice", anyOf: ["A", "B"]))
        ]
    )
    return try GenerationSchema(root: root, dependencies: [])
}

// The example is load-bearing for recall on character-level damage; when
// the model echoes it verbatim on a page that doesn't contain it, the
// anchor gate rejects the echo.
let findInstructions = """
You proofread OCR output from scanned books. Report only text damaged \
by the OCR process: wrong, missing, or extra characters (a scan may \
show "preadsheets" where the book printed "spreadsheets", or "S10" \
for "$10"); words merged or split into nonsense; stray punctuation \
inside words.
Do NOT report grammar, style, archaic usage, or proper nouns unless \
they are clearly OCR-damaged. Never invent text that is not present.
'original' must quote the damaged text exactly as it appears. Keep each \
edit as short as possible. Most passages are clean: if you find no OCR \
damage, return an empty edits array.
"""

// Framing matters here: asking which reading the page "showed" makes the
// model defend line-break artifacts (the page really did show "Visi- Calc").
// Ask for the correct continuous text instead.
let verifyInstructions = """
A passage was scanned from a printed book with OCR, which sometimes \
damages words: characters misread, spaces inserted inside words, or \
line-break hyphens left in the middle of words. You are shown the \
passage and two candidate readings, A and B, for one marked snippet. \
Decide which candidate is the correct continuous text — the words the \
author wrote, free of OCR damage and line-break artifacts. Answer A or B.
"""

struct Candidate { let original: String; let corrected: String }

/// Words per find-pass slice. Whole pages make the model summarize damage
/// into one long (gated-out) edit; short slices keep edits granular.
let SLICE = 120
let SLICE_OVERLAP = 20

func findEdits(page: Int, tokens: [String], schema: GenerationSchema) async -> [Candidate] {
    var out: [Candidate] = []
    var start = 0
    while start < tokens.count {
        let end = min(start + SLICE, tokens.count)
        let text = tokens[start..<end].joined(separator: " ")
        let session = LanguageModelSession(instructions: findInstructions)
        // cap the output: on damage-dense text a greedy decode can babble
        // edits until input+output exhausts the 4096-token window. A slice
        // that still errors (or overflows the cap) is skipped, not the page.
        do {
            let resp = try await session.respond(
                to: "Find OCR damage in this passage:\n\n\(text)",
                schema: schema,
                options: GenerationOptions(sampling: .greedy, maximumResponseTokens: 512)
            )
            let edits = try resp.content.value([GeneratedContent].self, forProperty: "edits")
            out += try edits.map {
                Candidate(
                    original: try $0.value(String.self, forProperty: "original"),
                    corrected: try $0.value(String.self, forProperty: "corrected")
                )
            }
        } catch {
            FileHandle.standardError.write(Data("page \(page) words \(start)-\(end): \(error)\n".utf8))
        }
        if end == tokens.count { break }
        start += SLICE - SLICE_OVERLAP
    }
    return out
}

/// ~30 words of context around the edit's anchor in the fused text.
func contextWindow(_ text: String, around original: String) -> String {
    guard let r = text.range(of: original) else { return text }
    let toks = text[..<r.lowerBound].split(separator: " ")
    let before = toks.suffix(15).joined(separator: " ")
    let after = text[r.upperBound...].split(separator: " ").prefix(15).joined(separator: " ")
    return "\(before) \(original) \(after)"
}

/// A/B forced choice; letter assignment randomized per edit index to
/// defeat position bias, deterministic so reruns are reproducible.
func verifyEdit(_ c: Candidate, index: Int, text: String, schema: GenerationSchema) async throws -> Bool {
    let flip = index % 2 == 1
    let (a, b) = flip ? (c.corrected, c.original) : (c.original, c.corrected)
    let session = LanguageModelSession(instructions: verifyInstructions)
    let prompt = """
    Passage: \(contextWindow(text, around: c.original))
    Snippet in question: the part reading '\(c.original)'
    A: '\(a)'
    B: '\(b)'
    """
    let resp = try await session.respond(
        to: prompt, schema: schema,
        options: GenerationOptions(sampling: .greedy, maximumResponseTokens: 32))
    let ans = try resp.content.value(String.self, forProperty: "answer")
    return (ans == "B") != flip // true = model chose 'corrected'
}

// MARK: - Main

func fail(_ msg: String, code: Int32) -> Never {
    FileHandle.standardError.write(Data((msg + "\n").utf8))
    exit(code)
}

@main
struct CleanPages {
    static func main() async {
        setbuf(stdout, nil) // progress lines are parsed live through a pipe
        var ocrDir: String? = nil
        var outDir: String? = nil
        var pagesArg: String? = nil
        var throttleMs = 750
        var args = ArraySlice(CommandLine.arguments.dropFirst())
        while let a = args.popFirst() {
            switch a {
            case "--ocr-dir": ocrDir = args.popFirst()
            case "--out-dir": outDir = args.popFirst()
            case "--pages": pagesArg = args.popFirst()
            case "--throttle-ms": throttleMs = args.popFirst().flatMap { Int($0) } ?? throttleMs
            default: fail("unknown argument: \(a)", code: 64)
            }
        }
        guard let ocrDir, let outDir else {
            fail("usage: clean-pages --ocr-dir <dir> --out-dir <dir> [--pages 1,5,9] [--throttle-ms 750]", code: 64)
        }

        guard case .available = SystemLanguageModel.default.availability else {
            fail("apple intelligence model unavailable: \(SystemLanguageModel.default.availability)", code: 2)
        }

        let fm = FileManager.default
        try? fm.createDirectory(atPath: outDir, withIntermediateDirectories: true)
        guard let names = try? fm.contentsOfDirectory(atPath: ocrDir) else {
            fail("cannot read \(ocrDir)", code: 66)
        }
        let only: Set<Int>? = pagesArg.map { Set($0.split(separator: ",").compactMap { Int($0) }) }

        var pages: [Page] = []
        for name in names.sorted() where name.hasSuffix(".json") {
            let url = URL(fileURLWithPath: ocrDir).appendingPathComponent(name)
            guard let data = try? Data(contentsOf: url),
                  let page = try? JSONDecoder().decode(Page.self, from: data) else {
                fail("bad OCR json: \(url.path)", code: 65)
            }
            pages.append(page)
        }

        // doc-wide vocabulary for the hyphen-fusion rule — built from ALL
        // pages before any --pages filter, or a partial run would fuse
        // hyphens differently than the Rust applier and void its own edits
        var vocab = Set<String>()
        for p in pages { for w in p.words { tokenize(w.t).forEach { vocab.insert($0) } } }
        if let only { pages.removeAll { !only.contains($0.page) } }

        let findSchema: GenerationSchema
        let verifySchema: GenerationSchema
        do {
            findSchema = try makeFindSchema()
            verifySchema = try makeVerifySchema()
        } catch {
            fail("schema error: \(error)", code: 70)
        }

        var done = 0
        for page in pages {
            done += 1
            let out = URL(fileURLWithPath: outDir).appendingPathComponent(String(format: "page-%04d.json", page.page))
            if fm.fileExists(atPath: out.path) {
                print("clean \(done)/\(pages.count)")
                continue
            }

            let tokens = fusedTokens(page.words.map(\.t), vocab: vocab)
            let text = tokens.joined(separator: " ")
            var edits: [EditOut] = []
            var seen = Set<String>()
            for (i, c) in await findEdits(page: page.page, tokens: tokens, schema: findSchema).enumerated() {
                guard gate(original: c.original, corrected: c.corrected, text: text),
                      seen.insert(c.original).inserted else { continue }
                do {
                    let ok = try await verifyEdit(c, index: i, text: text, schema: verifySchema)
                    edits.append(EditOut(original: c.original, corrected: c.corrected, verified: ok))
                } catch {
                    // an unverifiable edit is dropped, not the page
                    FileHandle.standardError.write(
                        Data("page \(page.page) verify '\(c.original)': \(error)\n".utf8))
                }
            }

            let enc = JSONEncoder()
            enc.outputFormatting = [.sortedKeys]
            guard let json = try? enc.encode(PageOut(page: page.page, edits: edits)) else { continue }
            let tmp = out.appendingPathExtension("tmp")
            try? json.write(to: tmp)
            try? fm.moveItem(at: tmp, to: out)
            print("clean \(done)/\(pages.count)")
            // breathing room between pages: this run shares the machine
            // with whatever the user is doing, and slower beats swapping
            if throttleMs > 0, done < pages.count {
                try? await Task.sleep(nanoseconds: UInt64(throttleMs) * 1_000_000)
            }
        }
    }
}

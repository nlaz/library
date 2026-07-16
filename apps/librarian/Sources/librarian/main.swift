// The librarian sidecar: Apple Foundation Models agent loop for The Library.
//
// Modes:
//   librarian serve           long-lived: one NDJSON request per stdin line
//                             {"e":"turn","conv":"<id>","messages":[...]}  → events, then {"e":"turn_end"}
//                             {"e":"cancel"}                               → cancel the active turn
//                             Sessions are kept per conv id (native AFM
//                             transcripts: tool results stay grounded across
//                             follow-ups), evicted after 10 min idle.
//   librarian turn            one-shot: stdin {"messages":[...]}, NDJSON events, exit
//   librarian probe <file>    run a capability-probe fixture, print NDJSON with
//                             a final {"e":"result", ...} line
//
// Tools call back into library-server over HTTP (LIBRARIAN_BASE, default
// http://127.0.0.1:8080) so search/content logic stays in Rust
// (library_core::tools, shared with the desktop app).
//
// Built macro-free (@Generable needs full Xcode): both the tool-call schemas
// and the final answer's schema (see ANSWER_SCHEMA) are hand-built
// GenerationSchemas over GeneratedContent. The final answer uses guided
// generation against ANSWER_SCHEMA so its format rules hold at decode time.

import Foundation
import FoundationModels

let BASE = ProcessInfo.processInfo.environment["LIBRARIAN_BASE"] ?? "http://127.0.0.1:8080"
/// --tools-stdin: tools go to the host process over stdio instead of HTTP
/// (the desktop app has no HTTP plane; it executes tools in-process).
let TOOLS_VIA_STDIN = CommandLine.arguments.contains("--tools-stdin")
/// --collections "a,b,c": real collection names from the host, woven into
/// tool schemas and instructions so the model can scope without guessing.
let COLLECTIONS: String = {
    guard let i = CommandLine.arguments.firstIndex(of: "--collections"),
        CommandLine.arguments.count > i + 1
    else { return "" }
    return CommandLine.arguments[i + 1]
}()
let COLLECTION_HINT =
    COLLECTIONS.isEmpty
    ? "Optional: search only one collection"
    : "Optional: search only one collection (\(COLLECTIONS))"

// MARK: - NDJSON emitter (stdout is the wire; serialize writes)

final class Emitter: @unchecked Sendable {
    private let lock = NSLock()
    func line(_ obj: [String: Any]) {
        lock.lock()
        defer { lock.unlock() }
        guard let d = try? JSONSerialization.data(withJSONObject: obj) else { return }
        FileHandle.standardOutput.write(d)
        FileHandle.standardOutput.write(Data([0x0a]))
    }
}
let emit = Emitter()

/// Tool-call log for probe stats.
final class Recorder: @unchecked Sendable {
    private let lock = NSLock()
    private(set) var calls: [[String: Any]] = []
    func record(_ name: String, _ args: [String: Any]) {
        lock.lock()
        defer { lock.unlock() }
        calls.append(["name": name, "args": args])
    }
}
let recorder = Recorder()

// MARK: - stdin tool bridge (desktop host executes tools in-process)

final class StdinToolBridge: @unchecked Sendable {
    static let shared = StdinToolBridge()
    private let lock = NSLock()
    private var next = 0
    private var pending: [Int: CheckedContinuation<String, Never>] = [:]

    func request(_ name: String, _ args: [String: Any]) async -> String {
        let id: Int = {
            lock.lock()
            defer { lock.unlock() }
            next += 1
            return next
        }()
        return await withCheckedContinuation { c in
            lock.lock()
            pending[id] = c
            lock.unlock()
            emit.line(["e": "tool_request", "id": id, "name": name, "args": args])
        }
    }

    func resolve(id: Int, result: String) {
        lock.lock()
        let c = pending.removeValue(forKey: id)
        lock.unlock()
        c?.resume(returning: result)
    }
}

/// Route a tool call to the host: stdio bridge (desktop) or HTTP (server).
func callTool(_ name: String, _ args: [String: Any], path: String, query: [String: String]) async -> String {
    if TOOLS_VIA_STDIN {
        return await StdinToolBridge.shared.request(name, args)
    }
    return await get(path, query)
}

// MARK: - HTTP back to library-server

func get(_ path: String, _ query: [String: String] = [:]) async -> String {
    var comps = URLComponents(string: BASE + path)!
    if !query.isEmpty {
        comps.queryItems = query.map { URLQueryItem(name: $0.key, value: $0.value) }
    }
    do {
        let (data, _) = try await URLSession.shared.data(from: comps.url!)
        return String(data: data, encoding: .utf8) ?? "{\"error\":\"non-utf8 response\"}"
    } catch {
        return "{\"error\":\"library server unreachable: \(error.localizedDescription)\"}"
    }
}

func json(_ s: String) -> Any? {
    try? JSONSerialization.jsonObject(with: Data(s.utf8))
}

// MARK: - Tools

/// Shared search plumbing: emit started/done tool events with hit chips,
/// route to /api/search (text or figures — the `kind` param picks).
func runSearch(toolName: String, query: String, collection: String, kind: String) async -> String {
    recorder.record(toolName, ["query": query, "collection": collection])
    emit.line([
        "e": "tool", "name": toolName, "status": "started",
        "args": ["query": query, "collection": collection],
    ])
    var params = ["q": query, "k": "6"]
    if !kind.isEmpty { params["kind"] = kind }
    if !collection.isEmpty { params["col"] = collection }
    let body = await callTool(
        toolName, ["query": query, "collection": collection],
        path: "/api/search", query: params)
    // surface hit chips to the UI even when the model's citations are sloppy
    var chips: [[String: Any]] = []
    if let obj = json(body) as? [String: Any], let hits = obj["hits"] as? [[String: Any]] {
        chips = hits.map { h in
            ["doc": h["doc"] ?? "", "title": h["title"] ?? NSNull(), "page": h["page"] ?? 0]
        }
    }
    emit.line([
        "e": "tool", "name": toolName, "status": "done",
        "summary": "\(chips.count) hits", "hits": chips,
    ])
    return body
}

struct SearchLibrary: Tool {
    let name = "search_library"
    let description =
        "Search the text of the library's scanned books (full-text + semantic). Returns matching passages with title, page, and snippet. Plain keywords work best."

    var parameters: GenerationSchema {
        GenerationSchema(
            type: GeneratedContent.self,
            properties: [
                .init(name: "query", description: "Search terms", type: String.self),
                .init(name: "collection", description: COLLECTION_HINT, type: String?.self),
            ])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        let query = (try? arguments.value(String.self, forProperty: "query")) ?? ""
        let col = ((try? arguments.value(String?.self, forProperty: "collection")) ?? nil) ?? ""
        return await runSearch(toolName: name, query: query, collection: col, kind: "")
    }
}

struct SearchFigures: Tool {
    let name = "search_figures"
    let description =
        "Find pictures, photographs, diagrams, and maps in the books. Only for requests about images — for facts, recipes, or any text question, use search_library instead."

    var parameters: GenerationSchema {
        GenerationSchema(
            type: GeneratedContent.self,
            properties: [
                .init(name: "query", description: "What the figure shows", type: String.self),
                .init(name: "collection", description: COLLECTION_HINT, type: String?.self),
            ])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        let query = (try? arguments.value(String.self, forProperty: "query")) ?? ""
        let col = ((try? arguments.value(String?.self, forProperty: "collection")) ?? nil) ?? ""
        return await runSearch(toolName: name, query: query, collection: col, kind: "images")
    }
}

/// Pages served to one conversation, so repeat sampling ("another one")
/// walks new shelves. Host-side session state — the model never sees it;
/// SamplePage injects it as an `avoid` arg the tool impl filters on.
final class SeenPages: @unchecked Sendable {
    private let lock = NSLock()
    private var pages: [String] = []
    func add(_ p: String) {
        lock.lock()
        defer { lock.unlock() }
        if !pages.contains(p) {
            pages.append(p)
            if pages.count > 40 { pages.removeFirst() }
        }
    }
    func csv() -> String {
        lock.lock()
        defer { lock.unlock() }
        return pages.joined(separator: ",")
    }
}

/// The sample fetch, shared by the model-visible tool and the plan
/// pre-pass (browse turns run it host-side — see plannedPrompt).
func fetchSample(collection col: String, seen: SeenPages) async -> String {
    recorder.record("sample_page", ["collection": col])
    emit.line(["e": "tool", "name": "sample_page", "status": "started", "args": ["collection": col]])
    var params: [String: String] = [:]
    if !col.isEmpty { params["col"] = col }
    let avoid = seen.csv()
    if !avoid.isEmpty { params["avoid"] = avoid }
    let body = await callTool(
        "sample_page", ["collection": col, "avoid": avoid], path: "/api/sample", query: params)
    var summary = "sampled a page"
    var chips: [[String: Any]] = []
    if let obj = json(body) as? [String: Any] {
        if let err = obj["error"] as? String {
            summary = err
        } else if let d = obj["doc"] as? String {
            let page = obj["page"] ?? 0
            summary = "opened \(d) p.\(page)"
            chips = [["doc": d, "title": obj["title"] ?? NSNull(), "page": page]]
            if let p = obj["page"] as? Int { seen.add("\(d):\(p)") }
        }
    }
    emit.line(["e": "tool", "name": "sample_page", "status": "done", "summary": summary, "hits": chips])
    return body
}

struct SamplePage: Tool {
    let name = "sample_page"
    let description =
        "Open one page of the library at random and return its text. Use when the user leaves the choice of material to you — open-ended, browsing, or inspiration asks with no specific topic to search for. Pass collection to browse one shelf."
    let seen: SeenPages

    var parameters: GenerationSchema {
        GenerationSchema(
            type: GeneratedContent.self,
            properties: [
                .init(name: "collection", description: COLLECTION_HINT, type: String?.self)
            ])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        let col = ((try? arguments.value(String?.self, forProperty: "collection")) ?? nil) ?? ""
        return await fetchSample(collection: col, seen: seen)
    }
}

struct ReadPages: Tool {
    let name = "read_pages"
    let description =
        "Read the full text of specific pages of a document, in reading order. Use to dig into a page another tool surfaced, or to keep reading nearby pages. Returns at most 2 pages per call."

    var parameters: GenerationSchema {
        GenerationSchema(
            type: GeneratedContent.self,
            properties: [
                .init(name: "doc", description: "Document id or book title from a tool result", type: String.self),
                .init(name: "from", description: "First page to read", type: Int.self),
                .init(name: "to", description: "Last page to read (at most from+1)", type: Int?.self),
            ])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        let doc = (try? arguments.value(String.self, forProperty: "doc")) ?? ""
        let from = (try? arguments.value(Int.self, forProperty: "from")) ?? 1
        let to = ((try? arguments.value(Int?.self, forProperty: "to")) ?? nil) ?? from
        recorder.record(name, ["doc": doc, "from": from, "to": to])
        emit.line([
            "e": "tool", "name": name, "status": "started",
            "args": ["doc": doc, "from": from, "to": to],
        ])
        let escaped = doc.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? doc
        let body = await callTool(
            name, ["doc": doc, "from": from, "to": to],
            path: "/api/text/\(escaped)", query: ["from": String(from), "to": String(to)])
        var summary = "pages \(from)-\(to)"
        var chips: [[String: Any]] = []
        if let obj = json(body) as? [String: Any] {
            if let err = obj["error"] as? String {
                summary = err
            } else if let d = obj["doc"] as? String {
                summary = "\(d) p.\(from)"
                chips = [["doc": d, "title": obj["title"] ?? NSNull(), "page": from]]
            }
        }
        emit.line(["e": "tool", "name": name, "status": "done", "summary": summary, "hits": chips])
        return body
    }
}

/// The overview fetch, shared by the model-visible tool and the plan
/// pre-pass (which injects it host-side rather than trusting the model to
/// make the call — see plannedPrompt).
func fetchOverview() async -> String {
    recorder.record("library_overview", [:])
    emit.line(["e": "tool", "name": "library_overview", "status": "started", "args": [String: String]()])
    let body = await callTool("library_overview", [:], path: "/api/overview", query: [:])
    var summary = "library overview"
    if let obj = json(body) as? [String: Any], let n = obj["books"] {
        summary = "\(n) books on the shelves"
    }
    emit.line(["e": "tool", "name": "library_overview", "status": "done", "summary": summary, "hits": [[String: Any]]()])
    return body
}

/// Compact plain-text rendering of the overview JSON for prompt injection.
func overviewText(_ body: String) -> String {
    guard let obj = json(body) as? [String: Any] else { return body }
    var parts: [String] = []
    if let n = obj["books"] { parts.append("\(n) books.") }
    for c in (obj["collections"] as? [[String: Any]]) ?? [] {
        let name = c["collection"] as? String ?? "?"
        let n = c["books"] ?? 0
        let ex = (c["examples"] as? [String] ?? []).joined(separator: ", ")
        parts.append("\(name): \(n) books, e.g. \(ex).")
    }
    if let loose = obj["uncollected_books"] {
        parts.append("\(loose) not in any collection.")
    }
    return parts.joined(separator: " ")
}

struct LibraryOverview: Tool {
    let name = "library_overview"
    let description =
        "See what the library holds: each collection with its size and a few example titles. Use to orient yourself before deciding where to look, or when the user asks what the library contains."

    var parameters: GenerationSchema {
        GenerationSchema(type: GeneratedContent.self, properties: [])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        await fetchOverview()
    }
}

// MARK: - Session

/// Process instructions, not use-case instructions: the model is told how a
/// librarian works — orient, find, dig, answer — and chooses tools itself.
/// The judgment-heavy choices (which tool, what query) are made in the plan
/// pre-pass (PLAN_SCHEMA) where decoding is schema-constrained; these prose
/// rules carry only the process and the honesty/grounding behaviors that
/// probes showed the model follows reliably.
let INSTRUCTIONS = """
    You are the librarian for a personal library of scanned books. You \
    answer only from the library's contents, using your tools — never from \
    your own knowledge. Work as a process: if you need to know what the \
    library holds, orient with library_overview; then find material — \
    search_library for anything specific, sample_page when the user leaves \
    the choice to you, search_figures only for pictures; then dig with \
    read_pages when the text you found is not enough; then answer from \
    what the tools returned, in your own words. Pass collection when the \
    user names one\(COLLECTIONS.isEmpty ? "" : " (\(COLLECTIONS))"). \
    Follow-ups like "more" or "another" are still library requests: call a \
    tool again; never decline them. Search results include a "confidence" \
    field — when it is "none" or "weak", say plainly that the library may \
    not cover the question. Results may include the top hit's full page \
    text; answer from it when it suffices. If a tool returns an error, \
    relay it honestly — never guess at a page's contents. Quote only short \
    clean phrases. Call books by their "title" from tool results — never \
    show raw document ids. Cite every claim as [Title p.N]. Be concise.
    """

/// Fresh tool instances per session/conversation. `seen` is the
/// conversation's sample-page avoid list — the caller holds it so the
/// plan pre-pass (which runs browse turns host-side) shares it with the
/// model-visible SamplePage tool.
func defaultTools(seen: SeenPages) -> [any Tool] {
    [SearchLibrary(), SearchFigures(), SamplePage(seen: seen), ReadPages(), LibraryOverview()]
}

func makeSession(tools: [any Tool]? = nil) -> LanguageModelSession {
    let tools = tools ?? defaultTools(seen: SeenPages())
    // Permissive guardrails are deliberate: benign questions about this
    // corpus (butchering a chicken, curing meat, sharpening a knife, home
    // brewing, lighting a wood stove) trip AFM's default guardrail. The
    // model still answers only from the library's own contents via its tools.
    return LanguageModelSession(
        model: SystemLanguageModel(guardrails: .permissiveContentTransformations),
        tools: tools,
        instructions: INSTRUCTIONS)
}

// MARK: - Plan pre-pass (structured reasoning via guided generation)

// The routing decision — what does the user want, which tool serves it,
// what are the actual search terms — used to live as trigger phrases in the
// instructions ("surprise me", "another one"), which generalized to nothing
// unscripted. It is now a decision the model makes itself, one guided-
// generation call before the tool loop, because schema-constrained decoding
// is where the 3B model is most reliable (chat-spike P4). This also attacks
// the spike's "query formulation is naive" finding: the plan's `query`
// field asks for reformulated content words, not the user's sentence.

let PLAN_INSTRUCTIONS = """
    You triage requests for a librarian agent over a personal library of \
    scanned books\(COLLECTIONS.isEmpty ? "" : " (collections: \(COLLECTIONS))"). \
    Decide how the librarian should handle the user's message. Do not \
    answer the message yourself. Examples: "what temperature for tandoori \
    chicken" → search. "do I have books about bread?" → search. "show me a \
    picture of a dome" → figures. "tell me an interesting fact" → browse. \
    "give me some wisdom from the books" → browse. "an interesting fact \
    from the cookbooks" → browse. "what kinds of books do I have?" → \
    overview. "hello!" → reply.
    """

let PLAN_SCHEMA: GenerationSchema = {
    let approach = DynamicGenerationSchema(
        name: "Approach",
        anyOf: ["search", "figures", "browse", "overview", "reply"])
    var props: [DynamicGenerationSchema.Property] = [
        .init(
            name: "intent",
            description: "What the user actually wants, in one short sentence.",
            schema: DynamicGenerationSchema(type: String.self)),
        .init(
            name: "approach",
            description: """
                figures: they want to SEE something — a picture, diagram, photograph, or map. \
                search: they want to know about something specific they name — a topic, \
                recipe, or book — answered from the books' text. \
                browse: they want an interesting fact, wisdom, inspiration, a surprise, \
                or another one — anything where the library picks the material. \
                overview: they ask what the library contains. \
                reply: no library material needed — a greeting or a question about you.
                """,
            schema: approach),
        .init(
            name: "query",
            description:
                "For search only: the terms to search with — plain content words, not the user's sentence. Otherwise empty.",
            schema: DynamicGenerationSchema(type: String.self)),
    ]
    // collection scoping is decode-constrained to the real shelf names
    if !COLLECTIONS.isEmpty {
        let choices = COLLECTIONS.split(separator: ",").map(String.init) + ["any"]
        props.append(
            .init(
                name: "collection",
                description: "The collection the user names, else \"any\".",
                schema: DynamicGenerationSchema(name: "Collection", anyOf: choices)))
    }
    let root = DynamicGenerationSchema(name: "Plan", properties: props)
    // hand-built (macro-free), same reason as ANSWER_SCHEMA
    return try! GenerationSchema(root: root, dependencies: [])
}()

struct Plan {
    let intent: String
    let approach: String
    let query: String
    let collection: String
}

/// One schema-constrained call that shapes the turn before the tool loop
/// runs. Returns nil on any failure — the turn then runs planless on the
/// raw prompt, which is exactly the pre-plan behavior.
func planTurn(_ context: String) async -> Plan? {
    let session = LanguageModelSession(
        model: SystemLanguageModel(guardrails: .permissiveContentTransformations),
        instructions: PLAN_INSTRUCTIONS)
    // near-greedy: routing is a classification, not prose — sampling
    // variance here flipped the same ask between approaches across runs
    let options = GenerationOptions(temperature: 0.1)
    guard let resp = try? await session.respond(to: context, schema: PLAN_SCHEMA, options: options)
    else {
        return nil
    }
    let col = (try? resp.content.value(String.self, forProperty: "collection")) ?? ""
    let plan = Plan(
        intent: (try? resp.content.value(String.self, forProperty: "intent")) ?? "",
        approach: (try? resp.content.value(String.self, forProperty: "approach")) ?? "",
        query: (try? resp.content.value(String.self, forProperty: "query")) ?? "",
        collection: col == "any" ? "" : col)
    emit.line([
        "e": "plan", "intent": plan.intent, "approach": plan.approach,
        "query": plan.query, "collection": plan.collection,
    ])
    recorder.record("plan", ["approach": plan.approach, "query": plan.query])
    return plan
}

/// Plain-text rendering of a search result for prompt injection (raw JSON
/// braces in the prompt broke the guided answer decode).
func searchResultText(_ body: String) -> String {
    guard let obj = json(body) as? [String: Any] else { return body }
    var parts: [String] = []
    if let conf = obj["confidence"] as? String { parts.append("Confidence: \(conf).") }
    if let note = obj["note"] as? String { parts.append(note) }
    for h in (obj["hits"] as? [[String: Any]]) ?? [] {
        let title = h["title"] as? String ?? h["doc"] as? String ?? "?"
        parts.append("[\(title) p.\(h["page"] ?? 0)] \(h["snippet"] as? String ?? "")")
    }
    if let thp = obj["top_hit_page"] as? [String: Any], let text = thp["text"] as? String {
        if let note = thp["note"] as? String { parts.append(note) }
        parts.append("Full text of the top hit's page: \(text)")
    }
    return parts.joined(separator: " ")
}

/// Plain-text rendering of a sampled page for prompt injection.
func sampleTextForPrompt(_ body: String) -> String {
    guard let obj = json(body) as? [String: Any] else { return body }
    if let err = obj["error"] as? String { return err }
    let title = obj["title"] as? String ?? obj["doc"] as? String ?? "?"
    return "[\(title) p.\(obj["page"] ?? 0)] \(obj["text"] as? String ?? "")"
}

/// Weave the plan into the turn prompt. "reply" (and a failed plan) adds
/// nothing — the raw prompt stands alone and the main session answers
/// directly. Every other approach executes its first hop host-side and
/// injects the result as context: the plan already made the decision, and
/// probe runs showed that delegating the call back to the model lets it
/// skip the tool and answer from imagination with fake citations
/// (p10-route-overview invented whole shelves; half the p1-search probes
/// never searched; browse turns wandered into library_overview). The model
/// still has every tool for follow-up hops (read_pages, another search);
/// only the first, planned hop is guaranteed.
func plannedPrompt(_ prompt: String, _ plan: Plan?, seen: SeenPages) async -> String {
    guard let plan else { return prompt }
    switch plan.approach {
    case "search":
        if plan.query.isEmpty {
            return "\(prompt)\n\nPlan: search the library first."
        }
        let body = await runSearch(
            toolName: "search_library", query: plan.query,
            collection: plan.collection, kind: "")
        return "\(prompt)\n\nLibrary search results for \"\(plan.query)\": \(searchResultText(body))"
    case "figures":
        if plan.query.isEmpty {
            return "\(prompt)\n\nPlan: find it with search_figures."
        }
        let body = await runSearch(
            toolName: "search_figures", query: plan.query,
            collection: plan.collection, kind: "images")
        return "\(prompt)\n\nFigure search results for \"\(plan.query)\": \(searchResultText(body))"
    case "browse":
        let body = await fetchSample(collection: plan.collection, seen: seen)
        return "\(prompt)\n\nA page opened at random — share the most interesting thing on it: \(sampleTextForPrompt(body))"
    case "overview":
        return "\(prompt)\n\nAnswer from this library overview: \(overviewText(await fetchOverview()))"
    default:
        return prompt
    }
}

/// Guide for the final answer's single `text` field. Constraining generation
/// to this schema is what replaces the old deterministic post-processing: the
/// format rules (no announcement opener; titles, never raw doc ids) are stated
/// at decode time, so we no longer strip filler openers with a regex or
/// substitute ids→titles after the fact. The 3B model followed the same rules
/// far less reliably as plain instructions — which is why that post-processing
/// existed at all.
let ANSWER_GUIDE = """
    Your answer to the user, in your own words. Begin directly with the \
    substance — never open with an announcement such as "Here is an \
    interesting fact". Refer to every book by its "title" from the tool \
    results, never by a raw document id, and cite each claim inline as \
    [Title p.N]. Be concise.
    """

/// The final answer's schema: a single guided `text` field. Built at runtime
/// (macro-free) so the tool stays buildable without the @Generable macro.
let ANSWER_SCHEMA = GenerationSchema(
    type: GeneratedContent.self,
    properties: [.init(name: "text", description: ANSWER_GUIDE, type: String.self)])

/// The `text` field of a (possibly partial) answer object, or nil if it has
/// not started streaming yet.
func answerText(_ content: GeneratedContent) -> String? {
    try? content.value(String.self, forProperty: "text")
}

func friendly(_ error: any Error) -> String {
    guard let e = error as? LanguageModelSession.GenerationError else {
        return error.localizedDescription
    }
    switch e {
    case .exceededContextWindowSize:
        return "context window exceeded"
    case .guardrailViolation:
        return "the on-device model declined this request (safety guardrail)"
    case .rateLimited:
        return "the on-device model is rate-limited right now"
    default:
        return e.localizedDescription
    }
}

/// Stream one prompt as a guided `Answer`; emit token deltas of its `text`
/// field; return the full text. Checks task cancellation between snapshots so a
/// `cancel` request stops output fast.
func stream(_ session: LanguageModelSession, _ prompt: String, options: GenerationOptions = GenerationOptions())
    async throws -> String
{
    var full = ""
    for try await snapshot in session.streamResponse(to: prompt, schema: ANSWER_SCHEMA, options: options) {
        try Task.checkCancellation()
        // partially-generated `text` is nil until the field starts streaming
        guard let text = answerText(snapshot.content) else { continue }
        // snapshots are cumulative; ship only the delta
        if text.count > full.count, text.hasPrefix(full) {
            let delta = String(text.dropFirst(full.count))
            emit.line(["e": "token", "text": delta])
        } else if text != full {
            emit.line(["e": "token", "text": text, "replace": true])
        }
        full = text
    }
    return full
}

// MARK: - turn mode

struct Msg {
    let role: String
    let content: String
}

func readMessages() -> [Msg] {
    let data = FileHandle.standardInput.readDataToEndOfFile()
    guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
        let raw = obj["messages"] as? [[String: Any]]
    else { return [] }
    return raw.compactMap { m in
        guard let role = m["role"] as? String, let content = m["content"] as? String else { return nil }
        return Msg(role: role, content: content)
    }
}

/// Fold prior turns into the prompt (spike-simple; AFM sessions are per-process).
func buildPrompt(_ messages: [Msg]) -> String {
    guard let last = messages.last, last.role == "user" else { return "" }
    let history = messages.dropLast().suffix(6)
    if history.isEmpty { return last.content }
    var block = "Conversation so far:\n"
    for m in history {
        var text = m.content
        if text.count > 300 { text = String(text.prefix(300)) + "…" }
        block += "\(m.role): \(text)\n"
    }
    block += "\nUser's new message: \(last.content)"
    return block
}

/// One turn against a session. Returns the replacement session if the
/// context overflowed and a fresh one was seeded (callers keep it for the
/// conversation), or nil to keep using the same session.
@discardableResult
func executeTurn(session: LanguageModelSession, prompt: String, fallback: String)
    async -> LanguageModelSession?
{
    let start = Date()
    do {
        let full = try await stream(session, prompt)
        emit.line([
            "e": "done", "content": full,
            "ms": Int(Date().timeIntervalSince(start) * 1000),
        ])
        return nil
    } catch is CancellationError {
        emit.line(["e": "cancelled"])
        return nil
    } catch let e as LanguageModelSession.GenerationError {
        if case .exceededContextWindowSize = e {
            // one retry: fresh session, bare question, no history
            let retry = makeSession()
            do {
                let full = try await stream(retry, fallback)
                emit.line(["e": "token", "text": full, "replace": true])
                emit.line(["e": "done", "content": full, "ms": Int(Date().timeIntervalSince(start) * 1000)])
            } catch {
                emit.line(["e": "error", "message": friendly(error)])
            }
            return retry
        }
        emit.line(["e": "error", "message": friendly(e)])
        return nil
    } catch {
        emit.line(["e": "error", "message": friendly(error)])
        return nil
    }
}

func runTurn() async {
    let messages = readMessages()
    let prompt = buildPrompt(messages)
    guard !prompt.isEmpty else {
        emit.line(["e": "error", "message": "no user message in request"])
        return
    }
    let seen = SeenPages()
    let session = makeSession(tools: defaultTools(seen: seen))
    session.prewarm()  // main session warms while the plan pre-pass runs
    let plan = await planTurn(prompt)
    await executeTurn(
        session: session,
        prompt: await plannedPrompt(prompt, plan, seen: seen),
        fallback: messages.last?.content ?? prompt)
}

// MARK: - serve mode (persistent: sessions per conversation, cancellation)

let SESSION_IDLE_EVICT: TimeInterval = 600

func parseMessages(_ raw: Any?) -> [Msg] {
    guard let arr = raw as? [[String: Any]] else { return [] }
    return arr.compactMap { m in
        guard let role = m["role"] as? String, let content = m["content"] as? String else { return nil }
        return Msg(role: role, content: content)
    }
}

/// Conversation sessions + the active turn, lock-guarded so the stdin loop
/// can keep reading (for `cancel`) while a turn task streams.
final class ServeState: @unchecked Sendable {
    private let lock = NSLock()
    private var sessions: [String: LanguageModelSession] = [:]
    private var seenPages: [String: SeenPages] = [:]
    private var lastUsed: [String: Date] = [:]
    private var active: Task<Void, Never>?

    /// Returns (session, seen, isNew), evicting idle conversations first.
    /// `seen` outlives session replacement (overflow retry) so "another
    /// one" keeps walking new shelves across the whole conversation.
    func checkout(_ conv: String) -> (LanguageModelSession, SeenPages, Bool) {
        lock.lock()
        defer { lock.unlock() }
        let now = Date()
        for (k, t) in lastUsed where now.timeIntervalSince(t) > SESSION_IDLE_EVICT {
            sessions.removeValue(forKey: k)
            seenPages.removeValue(forKey: k)
            lastUsed.removeValue(forKey: k)
        }
        lastUsed[conv] = now
        let seen = seenPages[conv] ?? SeenPages()
        seenPages[conv] = seen
        if let s = sessions[conv] { return (s, seen, false) }
        let s = makeSession(tools: defaultTools(seen: seen))
        sessions[conv] = s
        return (s, seen, true)
    }

    func replace(_ conv: String, with s: LanguageModelSession) {
        lock.lock()
        defer { lock.unlock() }
        sessions[conv] = s
    }

    func setActive(_ t: Task<Void, Never>?) {
        lock.lock()
        defer { lock.unlock() }
        active = t
    }

    func cancelActive() -> Task<Void, Never>? {
        lock.lock()
        defer { lock.unlock() }
        active?.cancel()
        return active
    }
}

func runServe() async {
    let state = ServeState()

    // prewarm once at spawn so the first turn is warm
    makeSession().prewarm()
    emit.line(["e": "ready"])

    do {
        for try await line in FileHandle.standardInput.bytes.lines {
            guard let obj = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any]
            else { continue }
            switch obj["e"] as? String {
            case "cancel":
                _ = state.cancelActive()
            case "tool_response":
                if let id = obj["id"] as? Int {
                    StdinToolBridge.shared.resolve(id: id, result: obj["result"] as? String ?? "{}")
                }
            case "turn":
                // one turn at a time: the host serializes, but never trust it
                if let prev = state.cancelActive() {
                    await prev.value
                }
                let conv = obj["conv"] as? String ?? "default"
                let messages = parseMessages(obj["messages"])
                guard let last = messages.last, last.role == "user" else {
                    emit.line(["e": "error", "message": "no user message in request"])
                    emit.line(["e": "turn_end"])
                    continue
                }
                // session transcripts carry history natively; only a brand-new
                // (or evicted) conversation needs the folded-history rebuild
                let (session, seen, isNew) = state.checkout(conv)
                let prompt = isNew ? buildPrompt(messages) : last.content
                let task = Task {
                    // plan over the folded history, not the bare message —
                    // "another one" only classifies with context
                    let plan = await planTurn(buildPrompt(messages))
                    if Task.isCancelled {
                        emit.line(["e": "cancelled"])
                        emit.line(["e": "turn_end"])
                        return
                    }
                    if let replacement = await executeTurn(
                        session: session, prompt: await plannedPrompt(prompt, plan, seen: seen),
                        fallback: last.content)
                    {
                        state.replace(conv, with: replacement)
                    }
                    emit.line(["e": "turn_end"])
                }
                state.setActive(task)
            default:
                continue
            }
        }
    } catch {
        // stdin closed or unreadable: the host is gone, exit quietly
    }
    _ = state.cancelActive()
}

// MARK: - probe mode

// Fixture: {"id": "...", "prompt": "..." | "turns": ["...", ...],
//           "instructions"?: "...", "tools"?: bool,
//           "temperature"?: 0.7, "schema"?: {"name": "...", "properties":
//           [{"name","type":"string"|"int"|"[string]","description"?}]}}
// `turns` runs a whole conversation on ONE session (transcript carries
// forward) — for probing follow-up behavior; `content` is the last turn's.
func runProbe(_ path: String) async {
    guard let data = FileManager.default.contents(atPath: path),
        let fx = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    else {
        emit.line(["e": "error", "message": "unreadable fixture \(path)"])
        exit(1)
    }
    let turns: [String] =
        (fx["turns"] as? [String]) ?? (fx["prompt"] as? String).map { [$0] } ?? []
    guard let prompt = turns.first else {
        emit.line(["e": "error", "message": "fixture has neither prompt nor turns: \(path)"])
        exit(1)
    }
    let id = fx["id"] as? String ?? (path as NSString).lastPathComponent
    let useTools = fx["tools"] as? Bool ?? true
    var options = GenerationOptions()
    if let t = fx["temperature"] as? Double { options = GenerationOptions(temperature: t) }

    let seen = SeenPages()
    let session: LanguageModelSession
    if let instructions = fx["instructions"] as? String {
        session = LanguageModelSession(
            model: SystemLanguageModel(guardrails: .permissiveContentTransformations),
            tools: useTools ? defaultTools(seen: seen) : [],
            instructions: instructions)
    } else {
        session = makeSession(tools: useTools ? defaultTools(seen: seen) : [])
    }

    let start = Date()
    do {
        var content: String
        var contents: [String] = []
        if let schemaSpec = fx["schema"] as? [String: Any] {
            let schema = try dynamicSchema(schemaSpec)
            let resp = try await session.respond(to: prompt, schema: schema, options: options)
            content = resp.content.jsonString
            contents = [content]
        } else {
            content = ""
            // probes run the same planned path as serve/turn mode, so
            // fixtures assert real routing; plan decisions land in
            // tool_calls (name "plan") via the recorder
            var transcript: [Msg] = []
            for (i, turn) in turns.enumerated() {
                transcript.append(Msg(role: "user", content: turn))
                var prompt = turn
                if useTools {
                    prompt = await plannedPrompt(
                        turn, await planTurn(buildPrompt(transcript)), seen: seen)
                }
                // guided generation, matching serve/turn mode
                let resp = try await session.respond(to: prompt, schema: ANSWER_SCHEMA, options: options)
                content = answerText(resp.content) ?? resp.content.jsonString
                contents.append(content)
                transcript.append(Msg(role: "assistant", content: content))
                if turns.count > 1 {
                    emit.line(["e": "turn_result", "id": id, "turn": i, "content": content])
                }
            }
        }
        emit.line([
            "e": "result", "id": id, "ok": true, "content": content, "contents": contents,
            "ms": Int(Date().timeIntervalSince(start) * 1000),
            "tool_calls": recorder.calls,
        ])
    } catch {
        emit.line([
            "e": "result", "id": id, "ok": false, "error": friendly(error),
            "ms": Int(Date().timeIntervalSince(start) * 1000),
            "tool_calls": recorder.calls,
        ])
        exit(2)
    }
}

func dynamicSchema(_ spec: [String: Any]) throws -> GenerationSchema {
    let name = spec["name"] as? String ?? "Result"
    let props = (spec["properties"] as? [[String: Any]] ?? []).map { p -> DynamicGenerationSchema.Property in
        let pname = p["name"] as? String ?? "field"
        let desc = p["description"] as? String
        let schema: DynamicGenerationSchema
        switch p["type"] as? String ?? "string" {
        case "int":
            schema = DynamicGenerationSchema(type: Int.self)
        case "[string]":
            schema = DynamicGenerationSchema(arrayOf: DynamicGenerationSchema(type: String.self))
        default:
            schema = DynamicGenerationSchema(type: String.self)
        }
        return DynamicGenerationSchema.Property(name: pname, description: desc, schema: schema)
    }
    let root = DynamicGenerationSchema(name: name, properties: props)
    return try GenerationSchema(root: root, dependencies: [])
}

// MARK: - entry

let args = CommandLine.arguments
switch args.count > 1 ? args[1] : "" {
case "serve":
    await runServe()
case "turn":
    await runTurn()
case "probe":
    guard args.count > 2 else {
        FileHandle.standardError.write(Data("usage: librarian probe <fixture.json>\n".utf8))
        exit(1)
    }
    await runProbe(args[2])
case "check":
    switch SystemLanguageModel.default.availability {
    case .available: print("AVAILABLE")
    case .unavailable(let reason): print("UNAVAILABLE: \(reason)")
    }
case "gen":
    // bare-generation diagnostic: default vs permissive guardrails, no tools
    for (label, model) in [
        ("default", SystemLanguageModel.default),
        ("permissive", SystemLanguageModel(guardrails: .permissiveContentTransformations)),
    ] {
        let s = LanguageModelSession(model: model)
        do {
            let r = try await s.respond(to: "Say the word hello.")
            print("\(label): OK \(r.content.prefix(40))")
        } catch {
            print("\(label): FAIL \(error)")
        }
    }
default:
    FileHandle.standardError.write(Data("usage: librarian serve|turn|probe <fixture.json>|check\n".utf8))
    exit(1)
}

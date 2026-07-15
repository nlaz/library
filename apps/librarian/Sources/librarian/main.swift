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
// Built macro-free (@Generable needs full Xcode): tool schemas are hand-built
// GenerationSchemas over GeneratedContent.

import Foundation
import FoundationModels

let BASE = ProcessInfo.processInfo.environment["LIBRARIAN_BASE"] ?? "http://127.0.0.1:8080"
/// --tools-stdin: tools go to the host process over stdio instead of HTTP
/// (the desktop app has no HTTP plane; it executes tools in-process).
let TOOLS_VIA_STDIN = CommandLine.arguments.contains("--tools-stdin")

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

struct SearchLibrary: Tool {
    let name = "search_library"
    let description =
        "Search the library of scanned books (full-text + semantic). Returns matching passages with doc id, page, and snippet. Plain keywords work best."

    var parameters: GenerationSchema {
        GenerationSchema(
            type: GeneratedContent.self,
            properties: [
                .init(name: "query", description: "Search terms", type: String.self),
                .init(
                    name: "kind",
                    description: "Set to \"images\" to search figures and photos instead of text",
                    type: String?.self),
            ])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        let query = (try? arguments.value(String.self, forProperty: "query")) ?? ""
        let kind = (try? arguments.value(String?.self, forProperty: "kind")) ?? nil
        recorder.record(name, ["query": query, "kind": kind ?? ""])
        emit.line(["e": "tool", "name": name, "status": "started", "args": ["query": query, "kind": kind ?? ""]])
        var params = ["q": query, "k": "6"]
        if let kind, !kind.isEmpty { params["kind"] = kind }
        let body = await callTool(
            name, ["query": query, "kind": kind ?? ""], path: "/api/search", query: params)
        // surface hit chips to the UI even when the model's citations are sloppy
        var chips: [[String: Any]] = []
        if let obj = json(body) as? [String: Any], let hits = obj["hits"] as? [[String: Any]] {
            chips = hits.map { h in
                ["doc": h["doc"] ?? "", "title": h["title"] ?? NSNull(), "page": h["page"] ?? 0]
            }
        }
        emit.line([
            "e": "tool", "name": name, "status": "done",
            "summary": "\(chips.count) hits", "hits": chips,
        ])
        return body
    }
}

struct ReadPages: Tool {
    let name = "read_pages"
    let description =
        "Read the full text of specific pages of a document, in reading order. Use after search_library to read the pages a hit points at. Returns at most 2 pages per call."

    var parameters: GenerationSchema {
        GenerationSchema(
            type: GeneratedContent.self,
            properties: [
                .init(name: "doc", description: "Document id from a search hit", type: String.self),
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
                chips = [["doc": d, "title": NSNull(), "page": from]]
            }
        }
        emit.line(["e": "tool", "name": name, "status": "done", "summary": summary, "hits": chips])
        return body
    }
}

struct ListCollections: Tool {
    let name = "list_collections"
    let description = "List the library's collections and the document ids in each."

    var parameters: GenerationSchema {
        GenerationSchema(type: GeneratedContent.self, properties: [])
    }

    func call(arguments: GeneratedContent) async throws -> String {
        recorder.record(name, [:])
        emit.line(["e": "tool", "name": name, "status": "started", "args": [String: String]()])
        let body = await callTool(name, [:], path: "/api/collections", query: [:])
        emit.line(["e": "tool", "name": name, "status": "done", "summary": "collections", "hits": [[String: Any]]()])
        return body
    }
}

// MARK: - Session

let INSTRUCTIONS = """
    You are the librarian for a personal library of scanned books: cookbooks, \
    Whole Earth Catalogs, and books about software and computing. You answer \
    only from the library's contents. Always call search_library before \
    answering a question about the books. Search results include a \
    "confidence" field and sometimes a "note" — when confidence is "none" or \
    "weak", tell the user plainly that the library may not cover their \
    question instead of stretching the hits. Results may include the top \
    hit's full page text; answer from it when it suffices, and use \
    read_pages only when you need a different page. If a tool returns an \
    error, relay it honestly — never guess at a page's contents. Cite every \
    claim as [doc-id p.N]. Be concise.
    """

func makeSession(tools: [any Tool] = [SearchLibrary(), ReadPages(), ListCollections()]) -> LanguageModelSession {
    LanguageModelSession(
        model: SystemLanguageModel(guardrails: .permissiveContentTransformations),
        tools: tools,
        instructions: INSTRUCTIONS)
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

/// Stream one prompt; emit token deltas; return the full text. Checks task
/// cancellation between snapshots so a `cancel` request stops output fast.
func stream(_ session: LanguageModelSession, _ prompt: String, options: GenerationOptions = GenerationOptions())
    async throws -> String
{
    var full = ""
    for try await snapshot in session.streamResponse(to: prompt, options: options) {
        try Task.checkCancellation()
        let text = snapshot.content
        // AFM's first snapshot before any tokens can be a literal "null"
        if full.isEmpty && text == "null" { continue }
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
        emit.line(["e": "done", "content": full, "ms": Int(Date().timeIntervalSince(start) * 1000)])
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
    let session = makeSession()
    session.prewarm()
    await executeTurn(session: session, prompt: prompt, fallback: messages.last?.content ?? prompt)
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
    private var lastUsed: [String: Date] = [:]
    private var active: Task<Void, Never>?

    /// Returns (session, isNew), evicting idle conversations first.
    func checkout(_ conv: String) -> (LanguageModelSession, Bool) {
        lock.lock()
        defer { lock.unlock() }
        let now = Date()
        for (k, t) in lastUsed where now.timeIntervalSince(t) > SESSION_IDLE_EVICT {
            sessions.removeValue(forKey: k)
            lastUsed.removeValue(forKey: k)
        }
        lastUsed[conv] = now
        if let s = sessions[conv] { return (s, false) }
        let s = makeSession()
        sessions[conv] = s
        return (s, true)
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
                let (session, isNew) = state.checkout(conv)
                let prompt = isNew ? buildPrompt(messages) : last.content
                let task = Task {
                    if let replacement = await executeTurn(
                        session: session, prompt: prompt, fallback: last.content)
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

// Fixture: {"id": "...", "prompt": "...", "instructions"?: "...", "tools"?: bool,
//           "temperature"?: 0.7, "schema"?: {"name": "...", "properties":
//           [{"name","type":"string"|"int"|"[string]","description"?}]}}
func runProbe(_ path: String) async {
    guard let data = FileManager.default.contents(atPath: path),
        let fx = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
        let prompt = fx["prompt"] as? String
    else {
        emit.line(["e": "error", "message": "unreadable fixture \(path)"])
        exit(1)
    }
    let id = fx["id"] as? String ?? (path as NSString).lastPathComponent
    let useTools = fx["tools"] as? Bool ?? true
    var options = GenerationOptions()
    if let t = fx["temperature"] as? Double { options = GenerationOptions(temperature: t) }

    let session: LanguageModelSession
    if let instructions = fx["instructions"] as? String {
        session = LanguageModelSession(
            model: SystemLanguageModel(guardrails: .permissiveContentTransformations),
            tools: useTools ? [SearchLibrary(), ReadPages(), ListCollections()] : [],
            instructions: instructions)
    } else {
        session = makeSession(tools: useTools ? [SearchLibrary(), ReadPages(), ListCollections()] : [])
    }

    let start = Date()
    do {
        var content: String
        if let schemaSpec = fx["schema"] as? [String: Any] {
            let schema = try dynamicSchema(schemaSpec)
            let resp = try await session.respond(to: prompt, schema: schema, options: options)
            content = resp.content.jsonString
        } else {
            let resp = try await session.respond(to: prompt, options: options)
            content = resp.content
        }
        emit.line([
            "e": "result", "id": id, "ok": true, "content": content,
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
default:
    FileHandle.standardError.write(Data("usage: librarian serve|turn|probe <fixture.json>|check\n".utf8))
    exit(1)
}

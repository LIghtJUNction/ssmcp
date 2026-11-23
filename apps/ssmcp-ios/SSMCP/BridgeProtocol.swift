import Foundation
import CoreFoundation

typealias JSONObject = [String: Any]

struct BridgeFailure: LocalizedError {
    let code: String
    let message: String
    let effect: String
    init(_ code: String, _ message: String, effect: String = "none") {
        self.code = code; self.message = message; self.effect = effect
    }
    var unknown: Bool { effect == "may_have_occurred" }
    var errorDescription: String? { message }
}

/// A canonical u64 decimal string; no floating point or signed conversion occurs.
struct Counter: Comparable, Equatable, CustomStringConvertible {
    let text: String
    init(_ value: Any?) throws {
        guard let text = value as? String, !text.isEmpty, text.utf8.count <= 20,
              (text.count == 1 || text.first != "0"), text.utf8.allSatisfy({ $0 >= 48 && $0 <= 57 }),
              text.count < 20 || text <= "18446744073709551615" else {
            throw BridgeFailure("invalid_counter", "Expected a canonical unsigned decimal string")
        }
        self.text = text
    }
    static func < (lhs: Counter, rhs: Counter) -> Bool {
        lhs.text.count == rhs.text.count ? lhs.text < rhs.text : lhs.text.count < rhs.text.count
    }
    var description: String { text }
}

func require(_ condition: Bool, _ message: String) throws {
    if !condition { throw BridgeFailure("invalid_response", message) }
}
func object(_ value: Any?) throws -> JSONObject {
    guard let value = value as? JSONObject else { throw BridgeFailure("invalid_response", "Expected an object") }
    return value
}
func string(_ value: Any?) throws -> String {
    guard let value = value as? String else { throw BridgeFailure("invalid_response", "Expected a string") }
    return value
}
func boolean(_ value: Any?) throws -> Bool {
    guard let value = value as? NSNumber, CFGetTypeID(value) == CFBooleanGetTypeID() else {
        throw BridgeFailure("invalid_response", "Expected a boolean")
    }
    return value.boolValue
}
func integer(_ value: Any?, _ range: ClosedRange<Int>) throws -> Int {
    guard let value = value as? NSNumber, CFGetTypeID(value) != CFBooleanGetTypeID(),
          "cCsSiIlLqQ".contains(String(cString: value.objCType)), range.contains(value.intValue) else {
        throw BridgeFailure("invalid_response", "Expected a bounded integer")
    }
    return value.intValue
}
func fields(_ value: JSONObject, _ required: Set<String>, optional: Set<String> = []) throws {
    try require(required.isSubset(of: Set(value.keys)) && Set(value.keys).isSubset(of: required.union(optional)), "Unexpected protocol fields")
}
func bridgeID(_ value: String) throws -> String {
    let allowed = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.:-"
    try require(!value.isEmpty && value.utf8.count <= 128 && value.allSatisfy { allowed.contains($0) }, "Invalid bridge identifier")
    return value
}
func jsonData(_ object: JSONObject) throws -> Data { try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) }

struct Capabilities {
    let generation: String
    let mode: String
    let resources: Set<String>
    let commands: Set<String>
    let events: Bool
    func can(_ command: String) -> Bool { mode == "control" && commands.contains(command) }
}
struct BridgeRequest {
    let id: String
    let operation: String
    let generation: String?
    let target: String?
    let json: JSONObject
    var isCommand: Bool { operation == "command" }
}
struct BridgeEvent {
    let sequence: Counter
    let topic: String
    let value: Any
}
struct EventPage {
    let events: [BridgeEvent]
    let next: Counter
    let gap: Bool
    let hasMore: Bool
}

enum CommandState: String { case confirmed, rejected, unknown, notSent }
struct UnconfirmedInput: Identifiable {
    let id: String
    let operation: String
    let connectionID: String
    let generation: String
    let target: String
    let text: String
}
struct CommandCompletion {
    let state: CommandState
    let connectionID: String
    let generation: String
    let requestID: String
    let target: String
    func mayClear(current: OpenConnection?, originalDraft: String, currentDraft: String, currentTarget: String) -> Bool {
        state == .confirmed && current?.id == connectionID && current?.capabilities.generation == generation &&
            originalDraft == currentDraft && target == currentTarget
    }
}

/// A received gap does not advance the committed cursor until reconciliation succeeds.
final class EventInbox {
    let generation: String
    private(set) var cursor: Counter?
    private(set) var pending: EventPage?
    init(generation: String, cursor: Counter? = nil) { self.generation = generation; self.cursor = cursor }
    @discardableResult func receive(_ page: EventPage, after issuedAfter: Counter? = nil) -> Bool {
        guard cursor == issuedAfter, pending == nil, cursor == nil || page.next >= cursor! else { return false }
        pending = page
        return true
    }
    @discardableResult func consume(reconciledGeneration: String? = nil, apply: (EventPage) throws -> Void) throws -> Bool {
        guard let page = pending else { return false }
        try require(!page.gap || reconciledGeneration == generation, "Refresh the gap before consuming it")
        try apply(page)
        cursor = page.next; pending = nil
        return true
    }
}

enum BridgeProtocol {
    static let maxFrame = 256 * 1024
    static let maxPage = 128 * 1024
    static let maxObservationMS = 55_000
    static let maxTimeline = 300

    static func request(_ op: String, generation: String? = nil, target: String? = nil,
                        arguments: JSONObject = [:]) throws -> BridgeRequest {
        try require(["capabilities", "state", "command"].contains(op), "Unsupported request")
        let id = UUID().uuidString
        var arguments = arguments
        if op == "command", target == "chat.send" {
            try require(arguments["request_id"] == nil, "The client assigns one command/admission ID")
            arguments["request_id"] = id
        }
        try require(try jsonData(arguments).count <= 32 * 1024, "Arguments too large")
        var operation: JSONObject = ["op": op]
        if let generation { operation["generation"] = try bridgeID(generation) }
        if let target {
            operation[op == "state" ? "resource" : "command"] = try bridgeID(target)
            operation[op == "state" ? "query" : "arguments"] = arguments
        }
        return BridgeRequest(id: id, operation: op, generation: generation, target: target,
            json: ["version": 1, "id": id, "operation": operation])
    }

    static func failure(_ value: JSONObject, wire: Bool = true) throws -> BridgeFailure {
        try fields(value, ["code", "message", "effect"])
        let effect = try string(value["effect"])
        let message = try string(value["message"])
        try require(["none", "may_have_occurred"].contains(effect) && message.utf8.count <= 1024, "Invalid error")
        let code = try string(value["code"])
        if wire {
            try require(["invalid_request", "unsupported_version", "denied", "closed", "unavailable", "stale_generation",
                "invalid_cursor", "too_large", "conflict", "internal"].contains(code), "Unknown zbridge error code")
        }
        return BridgeFailure(code, message, effect: effect)
    }

    private static func envelope(_ raw: JSONObject) throws -> JSONObject {
        try require(try jsonData(raw).count <= maxFrame, "Oversized frame")
        try fields(raw, ["version", "id", "generation", "body"])
        _ = try integer(raw["version"], 1...1)
        _ = try bridgeID(string(raw["id"]))
        let body = try object(raw["body"])
        try fields(body, ["kind", "value"])
        if try string(body["kind"]) != "error" || !(raw["generation"] is NSNull) {
            _ = try bridgeID(string(raw["generation"]))
        }
        return body
    }

    static func validate(_ raw: JSONObject, for request: BridgeRequest) throws -> JSONObject {
        let body = try envelope(raw)
        try require(try string(raw["id"]) == request.id, "Response belongs to another request")
        let kind = try string(body["kind"])
        let value = try object(body["value"])
        if kind == "error" {
            let error = try failure(value)
            try require(request.isCommand || !error.unknown, "Read response claimed command effects")
            throw error
        }
        if let generation = request.generation, try string(raw["generation"]) != generation {
            throw BridgeFailure("stale_generation", "Backend generation changed",
                effect: request.isCommand ? "may_have_occurred" : "none")
        }
        try require(kind == request.operation, "Response kind mismatch")
        switch kind {
        case "capabilities": _ = try capabilities(raw)
        case "state":
            try fields(value, ["resource", "schema_version", "value"], optional: ["revision"])
            _ = try integer(value["schema_version"], 1...1)
            try require(try string(value["resource"]) == request.target, "State resource mismatch")
            if let revision = value["revision"], !(revision is NSNull) { _ = try Counter(revision) }
        case "command":
            try fields(value, ["command", "value"])
            try require(try string(value["command"]) == request.target, "Command receipt mismatch")
            _ = try receiptState(request, payload: value["value"])
        default: throw BridgeFailure("invalid_response", "Unexpected response kind")
        }
        return value
    }

    /// Once dispatched, a locally invalid receipt cannot prove that a mutation had no effect.
    static func delivered(_ raw: JSONObject, for request: BridgeRequest) throws -> JSONObject {
        do { return try validate(raw, for: request) }
        catch {
            guard request.isCommand else { throw error }
            if let body = try? envelope(raw), raw["id"] as? String == request.id,
               body["kind"] as? String == "error", let value = body["value"] as? JSONObject,
               let authoritative = try? failure(value) { throw authoritative }
            throw BridgeFailure("invalid_response", "原命令回执无法验证，请核对原请求。", effect: "may_have_occurred")
        }
    }

    static func capabilities(_ raw: JSONObject) throws -> Capabilities {
        let body = try envelope(raw)
        if try string(body["kind"]) == "error" { throw try failure(object(body["value"])) }
        try require(try string(body["kind"]) == "capabilities", "Missing capabilities")
        let value = try object(body["value"])
        try fields(value, ["application", "mode", "resources", "commands", "events", "max_frame_bytes", "max_event_page_bytes"])
        try require(try string(value["application"]) == "ssmcp", "Unexpected application")
        let mode = try string(value["mode"])
        try require(["observe", "control"].contains(mode), "Invalid access mode")
        _ = try integer(value["max_frame_bytes"], 1...maxFrame)
        _ = try integer(value["max_event_page_bytes"], 1...maxPage)
        guard let resources = value["resources"] as? [JSONObject], let commands = value["commands"] as? [JSONObject] else {
            throw BridgeFailure("invalid_response", "Missing capability arrays")
        }
        try require(resources.count + commands.count <= 128, "Too many capabilities")
        var resourceIDs = Set<String>(); var allResourceIDs = Set<String>(); var commandIDs = Set<String>()
        for resource in resources {
            try fields(resource, ["id", "schema_version", "description"])
            let id = try bridgeID(string(resource["id"]))
            let schema = try integer(resource["schema_version"], 1...65535)
            try require(try string(resource["description"]).utf8.count <= 1024 && allResourceIDs.insert(id).inserted, "Invalid resource")
            if schema == 1 { resourceIDs.insert(id) }
        }
        for command in commands {
            try fields(command, ["id", "description", "input_schema"])
            let id = try bridgeID(string(command["id"]))
            try require(try string(command["description"]).utf8.count <= 1024 && commandIDs.insert(id).inserted, "Invalid command")
            try require(try jsonData(object(command["input_schema"])).count <= 32 * 1024, "Oversized command schema")
        }
        try require(mode != "observe" || commandIDs.isEmpty, "Observer advertised commands")
        return Capabilities(generation: try string(raw["generation"]), mode: mode,
            resources: resourceIDs, commands: commandIDs, events: try boolean(value["events"]))
    }

    static func receiptState(_ request: BridgeRequest, payload: Any?) throws -> CommandState {
        let value = payload as? JSONObject
        if request.target == "chat.send" {
            let receipt = try object(payload)
            let arguments = try object(object(request.json["operation"])["arguments"])
            try require(try string(receipt["request_id"]) == request.id && string(arguments["request_id"]) == request.id, "Chat request ID mismatch")
            try require(try string(receipt["target"]) == (arguments["target"] as? String ?? "next"), "Chat target mismatch")
            try require(try string(receipt["confirmation"]) == "local_queue_admission", "Unconfirmed queue admission")
            _ = try bridgeID(string(receipt["message_id"])); _ = try boolean(receipt["duplicate"])
            try require(try !string(receipt["status"]).isEmpty, "Missing admission status")
        }
        if ["unknown", "uncertain"].contains(value?["status"] as? String ?? "") || value?["uncertain"] as? Bool == true ||
            value?["effect"] as? String == "may_have_occurred" { return .unknown }
        return .confirmed
    }

    static func opened(_ raw: JSONObject, requestID: String, session: String, readOnly: Bool) throws -> OpenConnection {
        do {
            try require(try string(raw["status"]) == "connected" && string(raw["session"]) == session && boolean(raw["read_only"]) == readOnly, "Connection mismatch")
            try require(try string(object(raw["capabilities"])["id"]) == requestID, "Open response ID mismatch")
            let caps = try capabilities(object(raw["capabilities"]))
            try require((caps.mode == "observe") == readOnly, "Connection mode mismatch")
            return OpenConnection(id: try bridgeID(string(raw["connection_id"])), session: session, capabilities: caps)
        } catch { throw BridgeFailure("invalid_response", "租约可能已创建；请核对 open request \(requestID)", effect: "may_have_occurred") }
    }

    static func closed(_ raw: JSONObject) throws {
        do { try require(try boolean(raw["closed"]) && !boolean(raw["session_stopped"]), "Invalid detach acknowledgement") }
        catch { throw BridgeFailure("invalid_response", "租约释放结果待确认", effect: "may_have_occurred") }
    }

    static func eventEnvelope(_ raw: JSONObject, id: String, generation: String, cursor: Counter?, limit: Int) throws -> EventPage {
        try fields(raw, ["schema", "request_id", "generation", "page"])
        try require(try string(raw["schema"]) == "ssmcp-gui-events-v1" && string(raw["request_id"]) == id, "Event request mismatch")
        guard try string(raw["generation"]) == generation else { throw BridgeFailure("stale_generation", "Backend generation changed") }
        return try page(object(raw["page"]), cursor: cursor, limit: limit)
    }

    static func page(_ raw: JSONObject, cursor: Counter?, limit: Int) throws -> EventPage {
        try require((1...128).contains(limit) && (try jsonData(raw).count) <= maxPage, "Oversized event page")
        try fields(raw, ["events", "start_cursor", "next_cursor", "live_cursor", "retention_gap", "has_more"])
        let start = try Counter(raw["start_cursor"]), next = try Counter(raw["next_cursor"])
        let live = try Counter(raw["live_cursor"])
        let gap = try boolean(raw["retention_gap"]), more = try boolean(raw["has_more"])
        try require(start <= next && next <= live && more == (next < live), "Invalid event cursors")
        if let cursor { try require(next >= cursor && gap == (cursor < start), "Cursor mismatch") }
        guard let rows = raw["events"] as? [JSONObject], rows.count <= limit else { throw BridgeFailure("invalid_response", "Too many events") }
        var previous: Counter?; var events: [BridgeEvent] = []
        for row in rows {
            try fields(row, ["sequence", "topic", "value"])
            let sequence = try Counter(row["sequence"])
            try require(sequence >= start && sequence < next && (previous == nil || previous! < sequence), "Invalid event order")
            if let cursor { try require(sequence >= cursor, "Event precedes requested cursor") }
            events.append(BridgeEvent(sequence: sequence, topic: try bridgeID(string(row["topic"])), value: row["value"]!))
            previous = sequence
        }
        return EventPage(events: events, next: next, gap: gap, hasMore: more)
    }

    static func conversation(_ raw: JSONObject, cursor: Counter?) throws -> JSONObject {
        _ = try integer(raw["schema_version"], 1...1)
        try require(try string(raw["retention"]) == "live_process_memory", "Invalid history retention")
        let start = try Counter(raw["start_cursor"]), next = try Counter(raw["next_cursor"])
        let end = try Counter(raw["end_cursor"])
        try require(start <= next && next <= end, "Invalid conversation cursors")
        let gap = try boolean(raw["gap"])
        _ = try boolean(raw["has_more"])
        if let cursor { try require(next >= cursor && gap == (cursor < start), "Conversation cursor mismatch") }
        guard let events = raw["events"] as? [JSONObject], events.count <= 64 else { throw BridgeFailure("invalid_response", "Invalid history page") }
        try require(try jsonData(raw).count <= 64 * 1024, "Oversized history")
        var previous: Counter?
        for event in events {
            let sequence = try Counter(event["sequence"])
            try require(sequence >= start && sequence < next && (previous == nil || previous! < sequence), "Invalid history order")
            if let cursor { try require(sequence >= cursor, "History precedes cursor") }
            let source = try object(event["source"])
            _ = try string(source["kind"])
            if let author = source["agent_id"], !(author is NSNull) { _ = try string(author) }
            for key in ["connection", "request_id"] {
                if let value = source[key], !(value is NSNull) { _ = try Counter(value) }
            }
            _ = try string(event["text"]); _ = try string(event["kind"]); previous = sequence
        }
        return raw
    }
}

import Foundation

private final class CancellationBox {
    private let lock = NSLock()
    private var task: URLSessionTask?
    private var cancelled = false
    var isCancelled: Bool { lock.lock(); defer { lock.unlock() }; return cancelled }
    func attach(_ value: URLSessionTask) {
        lock.lock(); task = value; let shouldCancel = cancelled; lock.unlock()
        if shouldCancel { value.cancel() }
    }
    func cancel() { lock.lock(); cancelled = true; let value = task; lock.unlock(); value?.cancel() }
}

final class ReadBudget {
    private let cancellation = CancellationBox()
    private let clock: () -> TimeInterval
    private let deadline: TimeInterval
    init(window: TimeInterval = 55, clock: @escaping () -> TimeInterval = { ProcessInfo.processInfo.systemUptime }, deadline: TimeInterval? = nil) {
        precondition(window > 0 && window <= 55)
        self.clock = clock; self.deadline = deadline ?? (clock() + window)
    }
    var isCancelled: Bool { cancellation.isCancelled }
    func remaining() throws -> TimeInterval {
        if isCancelled { throw CancellationError() }
        let value = deadline - clock()
        guard value > 0 else { throw BridgeFailure("deadline", "观察截止，未消费的数据页仍保留。") }
        return min(value, 55)
    }
    func attach(_ task: URLSessionTask) { cancellation.attach(task) }
    func cancel() { cancellation.cancel() }
    func continuing() -> ReadBudget { ReadBudget(clock: clock, deadline: deadline) }
}

/// Streaming response cap applies before allocation can grow beyond one protocol frame.
private final class BoundedTransport {
    private struct Pending {
        var data = Data()
        let continuation: CheckedContinuation<(Data, HTTPURLResponse), Error>
        let deadline: DispatchWorkItem
    }
    private final class Delegate: NSObject, URLSessionDataDelegate {
        weak var owner: BoundedTransport?
        func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) { owner?.receive(dataTask, data) }
        func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) { owner?.complete(task, error) }
        func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                        newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
            completionHandler(nil) // Authorization must never move to another origin.
        }
    }
    private let lock = NSLock()
    private var pending: [Int: Pending] = [:]
    private var session: URLSession!
    init() {
        let delegate = Delegate(); delegate.owner = self
        let config = URLSessionConfiguration.ephemeral
        config.httpCookieStorage = nil; config.httpShouldSetCookies = false
        config.urlCache = nil; config.requestCachePolicy = .reloadIgnoringLocalCacheData
        config.timeoutIntervalForRequest = 55; config.timeoutIntervalForResource = 55
        config.waitsForConnectivity = false
        config.httpMaximumConnectionsPerHost = 4
        session = URLSession(configuration: config, delegate: delegate, delegateQueue: nil)
    }
    deinit { session.invalidateAndCancel() }
    func send(_ request: URLRequest, budget: ReadBudget? = nil) async throws -> (Data, HTTPURLResponse) {
        let cancellation = CancellationBox()
        let remaining = try budget?.remaining() ?? 55
        return try await withTaskCancellationHandler(operation: {
            try Task.checkCancellation()
            return try await withCheckedThrowingContinuation { continuation in
                let task = session.dataTask(with: request)
                lock.lock()
                guard pending.count < 4 else {
                    lock.unlock()
                    task.cancel()
                    continuation.resume(throwing: BridgeFailure("busy", "Too many requests"))
                    return
                }
                let deadline = DispatchWorkItem { [weak task] in task?.cancel() }
                pending[task.taskIdentifier] = Pending(continuation: continuation, deadline: deadline)
                lock.unlock()
                cancellation.attach(task); budget?.attach(task)
                DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + remaining, execute: deadline)
                task.resume()
            }
        }, onCancel: { cancellation.cancel() })
    }
    private func receive(_ task: URLSessionDataTask, _ data: Data) {
        lock.lock()
        guard var entry = pending[task.taskIdentifier] else { lock.unlock(); return }
        if entry.data.count + data.count > BridgeProtocol.maxFrame {
            pending.removeValue(forKey: task.taskIdentifier); lock.unlock(); task.cancel()
            entry.deadline.cancel()
            entry.continuation.resume(throwing: BridgeFailure("too_large", "Response exceeds the protocol limit"))
            return
        }
        entry.data.append(data); pending[task.taskIdentifier] = entry; lock.unlock()
    }
    private func complete(_ task: URLSessionTask, _ error: Error?) {
        lock.lock(); let entry = pending.removeValue(forKey: task.taskIdentifier); lock.unlock()
        guard let entry else { return }
        entry.deadline.cancel()
        if let error { entry.continuation.resume(throwing: error) }
        else if let response = task.response as? HTTPURLResponse { entry.continuation.resume(returning: (entry.data, response)) }
        else { entry.continuation.resume(throwing: BridgeFailure("transport", "Missing HTTP response")) }
    }
}

struct SessionSummary: Identifiable {
    let name: String
    let cwd: String
    let phase: String
    let managerState: String
    var id: String { name }
}
struct OpenConnection {
    let id: String
    let session: String
    let capabilities: Capabilities
}

protocol GatewayAPI: AnyObject {
    var origin: String { get }
    func sessions(offset: Int, budget: ReadBudget) async throws -> ([SessionSummary], Int?)
    func open(session: String, readOnly: Bool, openRequestID: String) async throws -> OpenConnection
    func capabilities(_ id: String, budget: ReadBudget) async throws -> Capabilities
    func bridge(_ id: String, _ request: BridgeRequest, budget: ReadBudget?) async throws -> JSONObject
    func events(_ id: String, generation: String, cursor: Counter?, budget: ReadBudget, waitMS: Int) async throws -> EventPage
    func close(_ id: String) async throws
}

final class Gateway: GatewayAPI {
    let origin: String
    private let token: String
    private let transport = BoundedTransport()
    init(origin: String, token: String) throws {
        guard let parts = URLComponents(string: origin.trimmingCharacters(in: .whitespacesAndNewlines)),
              parts.scheme == "https", parts.host?.isEmpty == false, parts.user == nil, parts.password == nil,
              parts.query == nil, parts.fragment == nil, parts.path == "" || parts.path == "/",
              let url = parts.url else {
            throw BridgeFailure("configuration", "输入 HTTPS 源地址，不包含路径、查询或凭据")
        }
        try require(!token.isEmpty && token.utf8.count <= 4096 && token.unicodeScalars.allSatisfy({ $0.value > 32 && $0.value != 127 }), "无效的配对令牌")
        self.origin = url.absoluteString.hasSuffix("/") ? String(url.absoluteString.dropLast()) : url.absoluteString
        self.token = token
    }

    private func http(_ method: String, _ path: String, body: JSONObject? = nil, command: Bool = false,
                      budget: ReadBudget? = nil) async throws -> JSONObject {
        try require(!command || budget == nil, "Mutations cannot use a read cancellation scope")
        let remaining = try budget?.remaining() ?? 55
        guard let url = URL(string: origin + "/api/gui/v1" + path) else { throw BridgeFailure("configuration", "Invalid gateway URL") }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: remaining)
        request.httpMethod = method
        request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            let data = try jsonData(body)
            try require(data.count <= BridgeProtocol.maxFrame, "Request exceeds frame limit")
            request.httpBody = data; request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let data: Data; let response: HTTPURLResponse
        do { (data, response) = try await transport.send(request, budget: budget) }
        catch {
            if !command && (Task.isCancelled || budget?.isCancelled == true) { throw CancellationError() }
            throw BridgeFailure("transport", "连接中断；请查询当前状态",
                effect: command ? "may_have_occurred" : "none")
        }
        _ = try budget?.remaining()
        let raw: JSONObject
        do { raw = try object(JSONSerialization.jsonObject(with: data)) }
        catch { throw BridgeFailure("invalid_response", "Invalid JSON response", effect: command ? "may_have_occurred" : "none") }
        if !(200...299).contains(response.statusCode) {
            let failure: BridgeFailure
            do { failure = try BridgeProtocol.failure(object(raw["error"]), wire: false) }
            catch { throw BridgeFailure("invalid_response", "Invalid gateway error", effect: command ? "may_have_occurred" : "none") }
            throw failure
        }
        return raw
    }

    func sessions(offset: Int = 0, budget: ReadBudget = ReadBudget()) async throws -> ([SessionSummary], Int?) {
        let raw = try await http("GET", "/sessions?offset=\(offset)&limit=16", budget: budget)
        guard let rows = raw["sessions"] as? [JSONObject], rows.count <= 16 else { throw BridgeFailure("invalid_response", "Invalid session page") }
        let sessions = try rows.map { row in
            SessionSummary(name: try string(row["name"]), cwd: try string(row["cwd"]),
                phase: row["phase"] as? String ?? "", managerState: row["manager_state"] as? String ?? "")
        }
        let next = raw["next_offset"] is NSNull ? nil : try integer(raw["next_offset"], 0...Int.max)
        try require(try boolean(raw["has_more"]) == (next != nil) && (next == nil || next! > offset), "Invalid session pagination")
        return (sessions, next)
    }
    func open(session: String, readOnly: Bool, openRequestID: String) async throws -> OpenConnection {
        _ = try bridgeID(openRequestID)
        let raw = try await http("POST", "/connections", body: ["request_id": openRequestID, "session": session, "read_only": readOnly], command: true)
        return try BridgeProtocol.opened(raw, requestID: openRequestID, session: session, readOnly: readOnly)
    }
    func capabilities(_ id: String, budget: ReadBudget = ReadBudget()) async throws -> Capabilities {
        let request = try BridgeProtocol.request("capabilities")
        let raw = try await http("POST", "/connections/\(bridgeID(id))/bridge", body: request.json, budget: budget)
        _ = try BridgeProtocol.validate(raw, for: request)
        return try BridgeProtocol.capabilities(raw)
    }
    func bridge(_ id: String, _ request: BridgeRequest, budget: ReadBudget? = nil) async throws -> JSONObject {
        let raw = try await http("POST", "/connections/\(bridgeID(id))/bridge", body: request.json, command: request.isCommand, budget: budget)
        return try BridgeProtocol.delivered(raw, for: request)
    }
    func events(_ id: String, generation: String, cursor: Counter?, budget: ReadBudget, waitMS: Int = 50_000) async throws -> EventPage {
        try require((0...55_000).contains(waitMS), "Invalid observation window")
        let requestID = UUID().uuidString
        var body: JSONObject = ["request_id": requestID, "generation": try bridgeID(generation), "limit": 32,
            "wait_ms": min(waitMS, max(1, Int(try budget.remaining() * 1000)))]
        if let cursor { body["cursor"] = cursor.text }
        let raw = try await http("POST", "/connections/\(bridgeID(id))/events", body: body, budget: budget)
        return try BridgeProtocol.eventEnvelope(raw, id: requestID, generation: generation, cursor: cursor, limit: 32)
    }
    func close(_ id: String) async throws {
        let raw = try await http("DELETE", "/connections/\(bridgeID(id))", command: true)
        try BridgeProtocol.closed(raw)
    }
}

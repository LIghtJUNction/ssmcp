import Foundation
import Combine

struct DisplayLine: Identifiable {
    let id = UUID()
    let text: String
}
struct ThreadSummary: Identifiable {
    let id: String
    let name: String
    let status: String
}
struct TerminalSummary: Identifiable {
    let id: String
    let name: String
    let cwd: String
}
struct WorkerSummary: Identifiable {
    let id: String
    let title: String
    let details: String
}

@MainActor final class ClientModel: ObservableObject {
    @Published var origin = ""
    @Published var token = ""
    @Published var readOnly = true
    @Published private(set) var ownedSession: String?
    var hasGateway: Bool { gateway != nil }
    @Published private(set) var status = "未连接"
    /// Only mutations occupy this lane; a close or explicit command can cancel reads immediately.
    @Published private(set) var busy = false
    @Published private(set) var connection: OpenConnection?
    @Published private(set) var sessions: [SessionSummary] = []
    @Published private(set) var nextSessionOffset: Int?
    @Published private(set) var timeline: [DisplayLine] = CommandJournal.summaries().map { DisplayLine(text: $0) }
    @Published private(set) var terminals: [TerminalSummary] = []
    @Published private(set) var workerRows: [WorkerSummary] = []
    @Published private(set) var workerSummary = ""
    @Published private(set) var timeSnapshot: JSONObject?
    @Published private(set) var updateResult: String?
    @Published private(set) var workerProductivity: [String] = []
    @Published private(set) var nextWorkerOffset: Int?
    @Published private(set) var poolGeneration: String?
    @Published private(set) var terminalOutput: [String: String] = [:]
    @Published private(set) var threads: [ThreadSummary] = []
    @Published private(set) var conversation: [DisplayLine] = []
    @Published private(set) var queue: [DisplayLine] = []
    @Published private(set) var chatCanSend = false
    @Published private(set) var chatHasMore = false
    @Published private(set) var chatTarget = "next"
    var cursor: Counter? { inbox?.cursor }
    var observationTask: Task<Void, Never>? { observer }
    private var gateway: GatewayAPI?
    private let makeBudget: () -> ReadBudget
    private var inbox: EventInbox?
    private var pendingHistory: HistoryPage?
    private var chatCursor: Counter?
    private var terminalBytes: [String: Data] = [:]
    private var epoch = 0
    private var active = false
    private var observer: Task<Void, Never>?
    private var observerBudget: ReadBudget?
    private var readBudget: ReadBudget?

    private struct HistoryPage {
        let generation: String
        let target: String
        let after: Counter?
        let value: JSONObject
    }
    private struct Snapshot {
        let terminals: [TerminalSummary]
        let workers: [WorkerSummary]
        let workerSummary: String
        let nextWorker: Int?
        let poolGeneration: String?
        let threads: [ThreadSummary]
        let canSend: Bool
        let queue: [DisplayLine]
        let history: [DisplayLine]
        let historyCursor: Counter?
        let historyMore: Bool
    }

    init(gateway: GatewayAPI? = nil, initialConnection: OpenConnection? = nil,
         initialInbox: EventInbox? = nil, loadPairing: Bool = true,
         makeBudget: @escaping () -> ReadBudget = { ReadBudget() }) {
        self.gateway = gateway; self.connection = initialConnection; self.inbox = initialInbox
        self.makeBudget = makeBudget
        guard loadPairing else { return }
        do {
            if let saved = try PairingStore.load() {
                let api = try Gateway(origin: saved.0, token: saved.1)
                self.gateway = api; origin = api.origin; token = saved.1
                ownedSession = try CommandJournal.lastOwnedSession(scope: PairingStore.receiptScope(origin: api.origin, token: saved.1))
            }
        }
        catch { status = "配对信息不可用，请删除后重新配对。" }
    }
    func setActive(_ value: Bool) {
        active = value
        if value, connection != nil, !busy { Task { await refresh() } }
        else if !value {
            stopReads()
            if !busy { status = "观察已暂停；未消费的数据页仍保留。" }
        }
    }
    private func stopReads() {
        observerBudget?.cancel(); observerBudget = nil
        observer?.cancel(); observer = nil
        readBudget?.cancel(); readBudget = nil
    }
    private func note(_ text: String) {
        timeline.append(DisplayLine(text: String(text.prefix(3072))))
        timeline = Array(timeline.suffix(BridgeProtocol.maxTimeline))
    }
    private func fail(_ error: Error, _ context: String) {
        if let failure = error as? BridgeFailure, failure.unknown {
            status = "\(context) · unknown：结果待确认，请核对原请求，勿自动重发。"
        } else { status = "\(context) · \(error.localizedDescription)" }
        note(status)
    }
    private func mutationFailure(_ error: Error, received: Bool, attempted: Bool) -> Error {
        if received || (attempted && !(error is BridgeFailure)) {
            return BridgeFailure("unconfirmed", "完整回执尚未确认或保存，请核对原请求。", effect: "may_have_occurred")
        }
        return error
    }
    func configure() {
        guard !busy, connection == nil else { return }
        do {
            let api = try Gateway(origin: origin, token: token)
            let previous = try CommandJournal.lastOwnedSession(scope: PairingStore.receiptScope(origin: api.origin, token: token))
            try PairingStore.save(origin: api.origin, token: token)
            stopReads(); gateway = api; origin = api.origin; epoch += 1
            ownedSession = previous; sessions = []; nextSessionOffset = nil; status = "已保存网关配对"
        } catch { fail(error, "配对失败") }
    }
    func deletePairing() {
        guard !busy, connection == nil else { return }
        do {
            try PairingStore.delete(); stopReads(); gateway = nil; origin = ""; token = ""
            ownedSession = nil; sessions = []; nextSessionOffset = nil; status = "已删除配对信息；未确定的请求记录仍保留。"; epoch += 1
        } catch { fail(error, "删除失败") }
    }
    func listSessions(more: Bool = false) async {
        guard let api = gateway, !busy else { return }
        let offset = more ? nextSessionOffset : 0
        guard let offset else { return }
        stopReads(); let budget = makeBudget(); readBudget = budget
        let mark = epoch; status = "读取会话"
        defer { if readBudget === budget { readBudget = nil; startObservation() } }
        do {
            let page = try await api.sessions(offset: offset, budget: budget)
            _ = try budget.remaining()
            guard mark == epoch, readBudget === budget else { return }
            sessions = more ? Array((sessions + page.0).suffix(128)) : page.0
            nextSessionOffset = page.1; status = "选择会话"
        } catch { if mark == epoch, readBudget === budget, !budget.isCancelled { fail(error, "读取失败") } }
    }
    func createSession(destination input: String, terms: Bool, privacy: Bool) async {
        guard let api = gateway, !busy, connection == nil else { return }
        let destination = input.trimmingCharacters(in: .whitespacesAndNewlines)
        let parts = destination.split(separator: "@", omittingEmptySubsequences: false)
        guard parts.count == 2, parts.allSatisfy({ !$0.isEmpty }), destination.utf8.count <= 512,
              !destination.contains(where: { $0.isWhitespace }) else {
            status = "请输入用户名@中继地址"; return
        }
        stopReads(); busy = true; epoch += 1
        let requestID = UUID().uuidString
        var reserved = false, received = false
        do {
            let scope = PairingStore.receiptScope(origin: api.origin, token: token)
            if let pending = try CommandJournal.pendingCreation(scope: scope, destination: destination) {
                busy = false; status = "已有待核对的创建请求 " + pending; return
            }
            try CommandJournal.reserve(session: destination, id: requestID, operation: "session.create",
                                       target: destination, gateway: scope)
            reserved = true; status = "正在网关上新建会话"
            let target = try await api.createSession(destination: destination, terms: terms, privacy: privacy, requestID: requestID)
            received = true
            try CommandJournal.settle(requestID, state: .confirmed, session: target)
            ownedSession = target; note("已新建会话 " + target + " · " + requestID)
            busy = false
            await open(target, readOnly: false)
        } catch {
            let failure = mutationFailure(error, received: received, attempted: reserved)
            if reserved, let known = failure as? BridgeFailure, !known.unknown { try? CommandJournal.settle(requestID, state: .rejected) }
            busy = false; fail(failure, "新建会话 " + requestID)
        }
    }
    func open(_ session: String, readOnly requestedMode: Bool? = nil) async {
        guard let api = gateway, !busy, connection == nil else { return }
        stopReads(); busy = true; epoch += 1
        let requestID = UUID().uuidString
        status = "连接中 · open request \(requestID)"; note(status)
        var reserved = false, received = false
        do {
            try CommandJournal.reserve(session: session, id: requestID, operation: "connection.open"); reserved = true
            let opened = try await api.open(session: session, readOnly: requestedMode ?? readOnly, openRequestID: requestID)
            received = true
            try CommandJournal.settle(requestID, state: .confirmed)
            connection = opened; inbox = nil; pendingHistory = nil; chatCursor = nil
            conversation = []; terminalBytes = [:]; terminalOutput = [:]
            note("已连接 \(session) · \(opened.capabilities.mode) · open request \(requestID)")
            busy = false
            if active { await refresh() } else { status = "已连接 · 观察已暂停" }
        } catch {
            let failure = mutationFailure(error, received: received, attempted: reserved)
            if reserved, let known = failure as? BridgeFailure, !known.unknown { try? CommandJournal.settle(requestID, state: .rejected) }
            busy = false; fail(failure, "Open request \(requestID)")
        }
    }
    private func read(_ name: String, api: GatewayAPI, connection: OpenConnection,
                      budget: ReadBudget, query: JSONObject = [:]) async throws -> JSONObject? {
        guard connection.capabilities.resources.contains(name) else { return nil }
        _ = try budget.remaining()
        let request = try BridgeProtocol.request("state", generation: connection.capabilities.generation, target: name, arguments: query)
        return try object(await api.bridge(connection.id, request, budget: budget)["value"])
    }
    /// Every read, including gap/history reconciliation, uses the same absolute deadline.
    func refresh(workerOffset: Int = 0, continuing: ReadBudget? = nil) async {
        guard let api = gateway, let old = connection, !busy else { return }
        stopReads(); let budget = continuing ?? makeBudget(); readBudget = budget
        let mark = epoch, target = chatTarget, after = chatCursor
        status = "刷新状态 · 未消费的数据页仍保留"
        defer { if readBudget === budget { readBudget = nil } }
        do {
            let caps = try await api.capabilities(old.id, budget: budget)
            let current = OpenConnection(id: old.id, session: old.session, capabilities: caps)
            var events = inbox?.generation == caps.generation ? inbox : nil
            if caps.events, events == nil {
                let received = EventInbox(generation: caps.generation)
                // Set the event boundary before history/state reads so their wakeups cannot be missed.
                let firstPage = try await api.events(old.id, generation: caps.generation, cursor: nil, budget: budget, waitMS: 0)
                received.receive(firstPage)
                guard mark == epoch, connection?.id == old.id, readBudget === budget else { return }
                inbox = received; events = received
            }
            let historyAfter = old.capabilities.generation == caps.generation ? after : nil
            var history = pendingHistory
            if history?.generation != caps.generation || history?.target != target || history?.after != historyAfter { history = nil }
            if caps.resources.contains("conversation"), history == nil {
                var query: JSONObject = ["limit": 32]
                if target != "next" { query["thread_id"] = target }
                if let historyAfter { query["cursor"] = historyAfter.text }
                if let raw = try await read("conversation", api: api, connection: current, budget: budget, query: query) {
                    let received = HistoryPage(generation: caps.generation, target: target, after: historyAfter,
                        value: try BridgeProtocol.conversation(raw, cursor: historyAfter))
                    guard mark == epoch, connection?.id == old.id, readBudget === budget else { return }
                    history = received
                    if target == chatTarget { pendingHistory = received }
                }
            }
            // Failures here retain both received pages and their old cursors, including across pause/resume.
            if let runtime = try await read("runtime", api: api, connection: current, budget: budget), let live = runtime["live_cursor"] {
                _ = try Counter(live)
            }
            let terminalState = try await read("terminals", api: api, connection: current, budget: budget)
            let workerState = try await read("workers", api: api, connection: current, budget: budget, query: ["offset": workerOffset, "limit": 16])
            let time = try await read("time", api: api, connection: current, budget: budget)
            let roster = try await read("threads", api: api, connection: current, budget: budget)
            let queued = try await read("chat.queue", api: api, connection: current, budget: budget)
            let snapshot = try prepare(terminalState, workers: workerState, roster: roster, queued: queued, history: history?.value)
            _ = try budget.remaining()
            guard mark == epoch, readBudget === budget, target == chatTarget, connection?.id == old.id else { return }
            if old.capabilities.generation != caps.generation {
                conversation = []; chatCursor = nil; terminalBytes = [:]; terminalOutput = [:]
                note("后端代次已变更；旧命令仍属于原代次，没有重放。")
            }
            connection = current
            timeSnapshot = time
            let productivity = workerState?["productivity"] as? JSONObject
            workerProductivity = Array((productivity?["plot"] as? [String] ?? []).prefix(5))
            terminals = snapshot.terminals; workerRows = snapshot.workers; workerSummary = snapshot.workerSummary
            nextWorkerOffset = snapshot.nextWorker; poolGeneration = snapshot.poolGeneration
            threads = snapshot.threads; chatCanSend = snapshot.canSend; queue = snapshot.queue
            inbox = events
            if events?.pending?.gap == true {
                terminalBytes = [:]; terminalOutput = [:]; note("事件保留窗口存在缺口；当前状态已刷新。")
            }
            try events?.consume(reconciledGeneration: caps.generation) { page in page.events.forEach { append($0) } }
            if history != nil {
                conversation = Array((conversation + snapshot.history).suffix(BridgeProtocol.maxTimeline))
                chatCursor = snapshot.historyCursor; chatHasMore = snapshot.historyMore; pendingHistory = nil
            }
            status = "已连接 · \(old.session) · \(caps.mode)"
            readBudget = nil; startObservation()
        } catch {
            if mark == epoch, readBudget === budget, !budget.isCancelled { fail(error, "状态读取失败；未消费的数据页仍保留") }
        }
    }
    private func prepare(_ terminalState: JSONObject?, workers: JSONObject?, roster: JSONObject?,
                         queued: JSONObject?, history: JSONObject?) throws -> Snapshot {
        let terminalRows = try ((terminalState?["terminals"] as? [JSONObject]) ?? []).prefix(16).map {
            TerminalSummary(id: try string($0["terminal_id"]), name: $0["name"] as? String ?? "", cwd: $0["cwd"] as? String ?? "")
        }
        var workerRows: [WorkerSummary] = [], summary = "当前连接没有 worker 池。"
        var pool: String?, nextWorker: Int?
        if let workers {
            pool = try string(workers["pool_generation"])
            if let value = workers["next_offset"], !(value is NSNull) { nextWorker = try integer(value, 0...Int.max) }
            summary = "Dreaming \(workers["dreaming"] ?? "—") · \(workers["working"] ?? 0) 工作 · \(workers["idle"] ?? 0) 待命 · \(workers["offline"] ?? 0) 离线\n\(workers["registered_workers"] ?? 0) 已登记 · 目标 \(workers["desired_total"] ?? 0) · \(workers["retiring"] ?? 0) 退出中"
            workerRows = try ((workers["slots"] as? [JSONObject]) ?? []).prefix(32).map { row in
                let epoch = try Counter(row["epoch"]), id = "\(row["slot"] ?? "")"
                let details = ["task_summary", "progress", "last_command", "reason"].compactMap { key -> String? in
                    guard let value = row[key], !(value is NSNull) else { return nil }
                    return String(String(describing: value).prefix(1024))
                }.joined(separator: "\n")
                let state = row["display_state"] as? String ?? row["state"] as? String ?? ""
                let label = state == "dreaming" ? "☾ Dreaming" : state
                return WorkerSummary(id: id, title: "站位 \(id) · \(label) · epoch \(epoch)", details: details)
            }
        }
        let threadRows = try ((roster?["agents"] as? [JSONObject]) ?? []).prefix(32).map {
            ThreadSummary(id: try string($0["agent_id"]), name: $0["name"] as? String ?? "", status: $0["status"] as? String ?? "")
        }
        let canSend = try roster.map { try boolean($0["can_send"]) } ?? false
        let queueRows = (queued?["items"] as? [JSONObject] ?? []).prefix(32).map { row -> DisplayLine in
            let state = row["uncertain"] as? Bool == true ? "unknown · 待确认，不自动重发" : (row["status"] as? String ?? "")
            let retarget = row["retarget_required"] as? Bool == true ? "\n目标失效，需要明确重新选择。" : ""
            return DisplayLine(text: "\(row["id"] ?? "") · \(row["target"] ?? "") · \(state)\n\(String((row["text"] as? String ?? "").prefix(1024)))\(retarget)")
        }
        var lines: [DisplayLine] = [], nextHistory: Counter?, more = false
        if let history {
            if try boolean(history["gap"]) { lines.append(DisplayLine(text: "保留窗口存在缺口；当前状态已刷新。")) }
            for event in history["events"] as? [JSONObject] ?? [] {
                let source = try object(event["source"])
                let author = source["agent_id"] as? String ?? source["kind"] as? String ?? "unattributed"
                lines.append(DisplayLine(text: "\(try string(event["sequence"])) · \(author) · \(try string(event["kind"]))\n\(try string(event["text"]))"))
            }
            nextHistory = try Counter(history["next_cursor"]); more = try boolean(history["has_more"])
        }
        return Snapshot(terminals: terminalRows, workers: workerRows, workerSummary: summary, nextWorker: nextWorker,
            poolGeneration: pool, threads: threadRows, canSend: canSend, queue: queueRows,
            history: lines, historyCursor: nextHistory, historyMore: more)
    }
    @discardableResult func command(_ name: String, arguments: JSONObject, expectedGeneration: String,
                                   expectedConnectionID: String? = nil) async -> CommandCompletion {
        let original = connection
        let target = name == "chat.send" ? (arguments["target"] as? String ?? "next") : (arguments["terminal_id"] as? String ?? "")
        func completion(_ state: CommandState, _ id: String = "") -> CommandCompletion {
            CommandCompletion(state: state, connectionID: original?.id ?? "", generation: expectedGeneration, requestID: id, target: target)
        }
        guard let api = gateway, let current = original, !busy, current.capabilities.can(name) else { return completion(.notSent) }
        guard current.capabilities.generation == expectedGeneration, expectedConnectionID == nil || expectedConnectionID == current.id else {
            fail(BridgeFailure("stale_generation", "后端代次已变更，请刷新后重新确认操作。"), "未发送命令")
            return completion(.notSent)
        }
        let request: BridgeRequest
        do { request = try BridgeProtocol.request("command", generation: expectedGeneration, target: name, arguments: arguments) }
        catch { fail(error, "无效命令"); return completion(.notSent) }
        stopReads(); busy = true
        if name == "client.update" { updateResult = nil }
        var reserved = false, dispatched = false, received = false
        var state: CommandState = .notSent
        do {
            try CommandJournal.reserve(session: current.session, id: request.id, operation: name,
                generation: expectedGeneration, connection: current.id, target: target,
                text: name == "chat.send" ? arguments["text"] as? String : (name == "terminal.write" ? arguments["data"] as? String : nil)); reserved = true
            note("发送 \(name) · \(request.id) · generation \(expectedGeneration)")
            dispatched = true
            let receipt = try await api.bridge(current.id, request, budget: nil)
            received = true
            state = try BridgeProtocol.receiptState(request, payload: receipt["value"])
            try CommandJournal.settle(request.id, state: state)
            status = state == .unknown ? "\(name) · unknown · 正文仍保留" : "\(name) · 已验证回执"
            note("\(current.session) · \(name) · \(request.id)\n\(status)\n回执：\(receipt["value"] ?? "")")
            if name == "client.update", let payload = receipt["value"] as? JSONObject {
                updateResult = [payload["summary"] as? String, payload["details"] as? String]
                    .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: "\n\n")
            }
        } catch {
            let failure = mutationFailure(error, received: received, attempted: dispatched)
            state = !dispatched ? .notSent : ((failure as? BridgeFailure)?.unknown == false ? .rejected : .unknown)
            if reserved { try? CommandJournal.settle(request.id, state: state == .notSent ? .rejected : state) }
            if state == .unknown, (failure as? BridgeFailure)?.unknown != true {
                fail(BridgeFailure("unconfirmed", "原命令的结果尚未确认", effect: "may_have_occurred"), "\(current.session) · \(name) · \(request.id)")
            } else { fail(failure, "\(current.session) · \(name) · \(request.id)") }
        }
        busy = false
        if active { Task { await refresh() } }
        return completion(state, request.id)
    }
    private func startObservation() {
        guard active, !busy, readBudget == nil, observer == nil, let api = gateway, let current = connection,
              let events = inbox, events.generation == current.capabilities.generation, current.capabilities.events else { return }
        if events.pending != nil { Task { await refresh() }; return }
        let mark = epoch
        observer = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, self.active, self.epoch == mark else { return }
                let budget = self.makeBudget(); self.observerBudget = budget
                do {
                    let issuedAfter = events.cursor
                    let page = try await api.events(current.id, generation: events.generation, cursor: issuedAfter, budget: budget, waitMS: 50_000)
                    guard self.epoch == mark, self.inbox === events else { return }
                    // Save the page before checking a pause that raced its arrival.
                    events.receive(page, after: issuedAfter)
                    guard !Task.isCancelled, self.active, self.observerBudget === budget else { return }
                    guard let pending = events.pending else { continue }
                    if pending.gap || (!pending.events.isEmpty && current.capabilities.resources.contains("conversation")) {
                        self.observer = nil; self.observerBudget = nil
                        await self.refresh(continuing: budget.continuing())
                        return
                    }
                    try events.consume { received in received.events.forEach { self.append($0) } }
                } catch {
                    if Task.isCancelled || budget.isCancelled { return }
                    guard self.epoch == mark, self.observerBudget === budget else { return }
                    self.observer = nil; self.observerBudget = nil
                    self.fail(error, "观察已停止；游标仍保留")
                    if let failure = error as? BridgeFailure, failure.code == "stale_generation", !self.busy { await self.refresh() }
                    return
                }
            }
        }
    }
    private func append(_ event: BridgeEvent) {
        if let value = event.value as? JSONObject, value["kind"] as? String == "terminal",
           let id = value["terminal_id"] as? String, let encoded = value["data_base64"] as? String {
            if terminalBytes[id] != nil || terminalBytes.count < 16, let bytes = Data(base64Encoded: encoded) {
                var output = terminalBytes[id] ?? Data(); output.append(bytes); output = Data(output.suffix(32768))
                terminalBytes[id] = output
                terminalOutput[id] = String(decoding: output, as: UTF8.self)
                    .replacingOccurrences(of: "\u{1B}\\[[0-?]*[ -/]*[@-~]", with: "", options: .regularExpression)
                    .replacingOccurrences(of: "[\\x00-\\x08\\x0B\\x0C\\x0E-\\x1F]", with: "", options: .regularExpression)
            }
            note("\(event.sequence) · \(event.topic) · \(id) · 输出已更新")
        } else { note("\(event.sequence) · \(event.topic)\n\(event.value)") }
    }
    func selectThread(_ target: String) {
        guard target != chatTarget else { return }
        chatTarget = target; chatCursor = nil; pendingHistory = nil; conversation = []; chatHasMore = false
        if !busy { Task { await refresh() } }
    }
    func refreshConversation() { if !busy { Task { await refresh() } } }
    func disconnect() async {
        guard !busy, let api = gateway, let current = connection else { return }
        stopReads(); busy = true
        let requestID = UUID().uuidString
        status = "释放租约 \(current.id) · request \(requestID)"; note(status)
        var reserved = false, received = false
        do {
            try CommandJournal.reserve(session: current.session, id: requestID, operation: "connection.close",
                generation: current.capabilities.generation, connection: current.id); reserved = true
            try await api.close(current.id); received = true
            try CommandJournal.settle(requestID, state: .confirmed)
            epoch += 1; connection = nil; inbox = nil; pendingHistory = nil
            terminals = []; terminalOutput = [:]; terminalBytes = [:]; workerRows = []; timeSnapshot = nil; workerProductivity = []; updateResult = nil
            threads = []; conversation = []; queue = []; chatCursor = nil; chatCanSend = false
            status = "已断开 · 后台会话继续运行"
        } catch {
            let failure = mutationFailure(error, received: received, attempted: reserved)
            if reserved, let known = failure as? BridgeFailure, !known.unknown { try? CommandJournal.settle(requestID, state: .rejected) }
            fail(failure, "Close request \(requestID) · connection \(current.id)")
        }
        busy = false
    }
}

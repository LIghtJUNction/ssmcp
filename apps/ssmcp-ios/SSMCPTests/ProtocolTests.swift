import XCTest
@testable import SSMCP

final class ProtocolTests: XCTestCase {
    func testCreateReceiptRequiresItsRequestAndExactLiveTarget() throws {
        let id = UUID().uuidString
        let target = "live:" + String(repeating: "a7", count: 16)
        XCTAssertEqual(try BridgeProtocol.createdSession(["request_id": id, "session": target], requestID: id), target)
        for invalid in [
            ["request_id": "another", "session": target], ["request_id": id, "session": "saved:first"],
            ["request_id": id, "session": target.uppercased()], ["request_id": id, "session": target + "0"]
        ] {
            XCTAssertThrowsError(try BridgeProtocol.createdSession(invalid, requestID: id)) { error in
                XCTAssertTrue((error as? BridgeFailure)?.unknown == true)
            }
        }
    }

    func testOwnedSessionReceiptStaysWithItsPairingWhenJournalFills() throws {
        let endpoint = "https://gateway.test"
        let scope = PairingStore.receiptScope(origin: endpoint, token: "one")
        XCTAssertNotEqual(scope, PairingStore.receiptScope(origin: endpoint, token: "two"))
        XCTAssertNotEqual(scope, PairingStore.receiptScope(origin: "https://other.test", token: "one"))
        let created = ["id": "create", "operation": "session.create", "state": "confirmed", "gateway": scope]
        let ordinary = ["id": "old", "state": "confirmed"]
        let result = try CommandJournal.insert([created, ordinary], value: ["id": "new", "state": "unknown"], capacity: 2)
        XCTAssertEqual(result.map { $0["id"] }, ["create", "new"])
    }

    func testLosslessCountersAndBounds() throws {
        XCTAssertGreaterThan(try Counter("9007199254740993"), try Counter("9007199254740992"))
        XCTAssertEqual(try Counter("18446744073709551615").text, "18446744073709551615")
        let invalid: [Any] = ["01", "-1", "", "18446744073709551616", 42, 1.0]
        for value in invalid {
            XCTAssertThrowsError(try Counter(value))
        }
    }
    func testPageOrderingGapAndAssociation() throws {
        let row: JSONObject = ["sequence": "9007199254740993", "topic": "ssmcp.event", "value": ["text": "真实事件"]]
        var page: JSONObject = ["events": [row], "start_cursor": "9007199254740993", "next_cursor": "9007199254740994",
            "live_cursor": "9007199254740994", "retention_gap": false, "has_more": false]
        XCTAssertEqual(try BridgeProtocol.page(page, cursor: Counter("9007199254740993"), limit: 16).events.count, 1)
        XCTAssertThrowsError(try BridgeProtocol.page(page, cursor: Counter("9007199254740992"), limit: 16))
        page["retention_gap"] = true
        XCTAssertTrue(try BridgeProtocol.page(page, cursor: Counter("9007199254740992"), limit: 16).gap)
        page["events"] = [row, row]
        XCTAssertThrowsError(try BridgeProtocol.page(page, cursor: Counter("9007199254740992"), limit: 16))
    }
    func testLateCommandResponseCannotBecomeCurrent() throws {
        let request = try BridgeProtocol.request("command", generation: "generation-1", target: "terminal.write", arguments: ["terminal_id": "main", "data": "hi"])
        var response: JSONObject = ["version": 1, "id": request.id, "generation": "generation-1",
            "body": ["kind": "command", "value": ["command": "terminal.write", "value": [:] as JSONObject]]]
        _ = try BridgeProtocol.validate(response, for: request)
        response["id"] = "different-request"
        XCTAssertThrowsError(try BridgeProtocol.validate(response, for: request))
        response["id"] = request.id; response["generation"] = "generation-2"
        XCTAssertThrowsError(try BridgeProtocol.validate(response, for: request)) { error in
            XCTAssertTrue((error as? BridgeFailure)?.unknown == true)
        }
    }
    func testNoCredentialsOrInsecureSchemeInURL() throws {
        XCTAssertThrowsError(try Gateway(origin: "http://example.test", token: "secret"))
        XCTAssertThrowsError(try Gateway(origin: "https://user:secret@example.test", token: "secret"))
        XCTAssertThrowsError(try Gateway(origin: "https://example.test?token=secret", token: "secret"))
        XCTAssertEqual(try Gateway(origin: "https://example.test/", token: "secret").origin, "https://example.test")
    }
    func testObserverCannotAdvertiseCommands() throws {
        let command: JSONObject = ["id": "terminal.write", "description": "write", "input_schema": [:] as JSONObject]
        let capabilities: JSONObject = ["application": "ssmcp", "mode": "observe", "resources": [] as [JSONObject],
            "commands": [command], "events": true, "max_frame_bytes": 261120, "max_event_page_bytes": 131072]
        let response: JSONObject = ["version": 1, "id": "open-capabilities", "generation": "generation-1",
            "body": ["kind": "capabilities", "value": capabilities]]
        XCTAssertThrowsError(try BridgeProtocol.capabilities(response))
    }
}

private func testCaps(_ resources: [String], events: Bool = false) -> Capabilities {
    Capabilities(generation: "generation-1", mode: "control", resources: Set(resources),
        commands: ["chat.send", "terminal.write"], events: events)
}
private func testCapsEnvelope(id: String = "open-id", schema: Int = 1) -> JSONObject {
    ["version": 1, "id": id, "generation": "generation-1", "body": ["kind": "capabilities", "value": [
        "application": "ssmcp", "mode": "observe", "events": true, "max_frame_bytes": BridgeProtocol.maxFrame,
        "max_event_page_bytes": BridgeProtocol.maxPage, "commands": [] as [JSONObject],
        "resources": [["id": "runtime", "schema_version": schema, "description": "state"]]]]]
}
private func testResponse(_ request: BridgeRequest, payload: JSONObject, schema: Int = 1) -> JSONObject {
    let body: JSONObject = request.isCommand ? ["command": request.target!, "value": payload] :
        ["resource": request.target!, "schema_version": schema, "value": payload]
    return ["version": 1, "id": request.id, "generation": request.generation!, "body": ["kind": request.operation, "value": body]]
}
private func testHistory(_ sequences: [String], next: String) -> JSONObject {
    ["schema_version": 1, "scope": [:] as JSONObject, "start_cursor": "0", "next_cursor": next, "end_cursor": next,
     "has_more": false, "gap": false, "retention": "live_process_memory", "events": sequences.map { sequence -> JSONObject in
        ["id": "history-" + sequence, "sequence": sequence, "kind": "human_input", "text": "history-" + sequence,
         "source": ["kind": "local_human", "agent_id": NSNull(), "request_id": NSNull(), "connection": "1"]]
     }]
}

final class ObservationAndReceiptTests: XCTestCase {
    func testGapPageSurvivesFailedApplicationAndConsumesOnce() throws {
        let old = try Counter("1")
        let inbox = EventInbox(generation: "generation-1", cursor: old)
        let page = EventPage(events: [BridgeEvent(sequence: try Counter("2"), topic: "ssmcp.event", value: "A")],
            next: try Counter("3"), gap: true, hasMore: false)
        XCTAssertTrue(inbox.receive(page, after: old))
        XCTAssertThrowsError(try inbox.consume { _ in XCTFail("Gap was consumed without reconciliation") })
        XCTAssertThrowsError(try inbox.consume(reconciledGeneration: "generation-1") { _ in throw BridgeFailure("unavailable", "snapshot failed") })
        XCTAssertEqual(inbox.cursor, old); XCTAssertNotNil(inbox.pending)
        var consumed = 0
        XCTAssertTrue(try inbox.consume(reconciledGeneration: "generation-1") { received in consumed += received.events.count })
        XCTAssertFalse(try inbox.consume(reconciledGeneration: "generation-1") { _ in XCTFail("Duplicate application") })
        XCTAssertEqual(consumed, 1); XCTAssertEqual(inbox.cursor?.text, "3")
        XCTAssertFalse(inbox.receive(page, after: old)); XCTAssertNil(inbox.pending)
    }
    func testAbsoluteReadDeadlineAndSeparateCancellationScopes() throws {
        var clock: TimeInterval = 0
        let budget = ReadBudget(clock: { clock })
        clock = 20; XCTAssertEqual(try budget.remaining(), 35)
        let continued = budget.continuing()
        clock = 54; XCTAssertEqual(try continued.remaining(), 1)
        clock = 55; XCTAssertThrowsError(try budget.remaining()); XCTAssertThrowsError(try continued.remaining())
        let read = ReadBudget(), independent = ReadBudget()
        read.cancel(); XCTAssertThrowsError(try read.remaining()); XCTAssertGreaterThan(try independent.remaining(), 0)
    }
    func testDraftClearsOnlyForSameConfirmedConnectionGenerationTargetAndText() {
        let original = OpenConnection(id: "connection-1", session: "test", capabilities: testCaps([]))
        let confirmed = CommandCompletion(state: .confirmed, connectionID: original.id,
            generation: original.capabilities.generation, requestID: "request-1", target: "agent-1")
        XCTAssertTrue(confirmed.mayClear(current: original, originalDraft: "body", currentDraft: "body", currentTarget: "agent-1"))
        for state in [CommandState.unknown, .rejected, .notSent] {
            let result = CommandCompletion(state: state, connectionID: original.id, generation: "generation-1", requestID: "request", target: "agent-1")
            XCTAssertFalse(result.mayClear(current: original, originalDraft: "body", currentDraft: "body", currentTarget: "agent-1"))
        }
        XCTAssertFalse(confirmed.mayClear(current: original, originalDraft: "body", currentDraft: "edited", currentTarget: "agent-1"))
        XCTAssertFalse(confirmed.mayClear(current: original, originalDraft: "body", currentDraft: "body", currentTarget: "agent-2"))
        XCTAssertFalse(confirmed.mayClear(current: nil, originalDraft: "body", currentDraft: "body", currentTarget: "agent-1"))
        let other = OpenConnection(id: "connection-2", session: "test", capabilities: testCaps([]))
        XCTAssertFalse(confirmed.mayClear(current: other, originalDraft: "body", currentDraft: "body", currentTarget: "agent-1"))
        let newGeneration = Capabilities(generation: "generation-2", mode: "control", resources: [], commands: [], events: false)
        XCTAssertFalse(confirmed.mayClear(current: OpenConnection(id: original.id, session: "test", capabilities: newGeneration),
            originalDraft: "body", currentDraft: "body", currentTarget: "agent-1"))
    }
    func testChatAdmissionCorrelationAndUnknownReceipt() throws {
        let request = try BridgeProtocol.request("command", generation: "generation-1", target: "chat.send", arguments: ["text": "你好", "target": "agent-1"])
        let arguments = try object(object(request.json["operation"])["arguments"])
        XCTAssertEqual(arguments["request_id"] as? String, request.id)
        let receipt: JSONObject = ["request_id": request.id, "message_id": "message-1", "target": "agent-1",
            "status": "sending", "duplicate": false, "confirmation": "local_queue_admission"]
        XCTAssertEqual(try BridgeProtocol.receiptState(request, payload: receipt), .confirmed)
        for (key, value) in [("request_id", "other"), ("target", "agent-2"), ("confirmation", "other"), ("message_id", "")] {
            var invalid = receipt; invalid[key] = value
            XCTAssertThrowsError(try BridgeProtocol.delivered(testResponse(request, payload: invalid), for: request)) {
                XCTAssertTrue(($0 as? BridgeFailure)?.unknown == true)
            }
        }
        for state in ["unknown", "uncertain"] {
            var uncertain = receipt; uncertain["status"] = state
            XCTAssertEqual(try BridgeProtocol.receiptState(request, payload: uncertain), .unknown)
        }
        var uncertain = receipt; uncertain["uncertain"] = true
        XCTAssertEqual(try BridgeProtocol.receiptState(request, payload: uncertain), .unknown)
        XCTAssertThrowsError(try BridgeProtocol.request("command", generation: "generation-1", target: "chat.send", arguments: ["request_id": "external", "text": "body"]))
    }
    func testLeaseValidationFailureRemainsUnknown() throws {
        let open: JSONObject = ["status": "connected", "session": "test", "read_only": true,
            "connection_id": "connection-1", "duplicate": false, "capabilities": testCapsEnvelope()]
        XCTAssertEqual(try BridgeProtocol.opened(open, requestID: "open-id", session: "test", readOnly: true).id, "connection-1")
        XCTAssertThrowsError(try BridgeProtocol.opened(open, requestID: "other-id", session: "test", readOnly: true)) {
            XCTAssertTrue(($0 as? BridgeFailure)?.unknown == true)
        }
        XCTAssertThrowsError(try BridgeProtocol.opened(open, requestID: "open-id", session: "test", readOnly: false)) {
            XCTAssertTrue(($0 as? BridgeFailure)?.unknown == true)
        }
        XCTAssertThrowsError(try BridgeProtocol.closed(["closed": false, "session_stopped": false])) {
            XCTAssertTrue(($0 as? BridgeFailure)?.unknown == true)
        }
        try BridgeProtocol.closed(["closed": true, "session_stopped": false])
    }
    func testUnsupportedResourceSchemaAndUntrustedWireError() throws {
        XCTAssertFalse(try BridgeProtocol.capabilities(testCapsEnvelope(schema: 2)).resources.contains("runtime"))
        let request = try BridgeProtocol.request("state", generation: "generation-1", target: "runtime")
        XCTAssertThrowsError(try BridgeProtocol.validate(testResponse(request, payload: ["live_cursor": "1"], schema: 2), for: request))
        let error: JSONObject = ["code": "invented_native_error", "message": "bad", "effect": "none"]
        XCTAssertThrowsError(try BridgeProtocol.failure(error))
        XCTAssertEqual(try BridgeProtocol.failure(error, wire: false).code, "invented_native_error")
    }
    func testJournalNeverEvictsUnknownAndPreservesOriginalInput() throws {
        let unknown = ["id": "unknown", "state": "unknown", "text_base64": Data("原文".utf8).base64EncodedString()]
        let confirmed = ["id": "confirmed", "state": "confirmed"]
        let pending = ["id": "new", "state": "unknown"]
        let entries = try CommandJournal.insert([unknown, confirmed], value: pending, capacity: 2)
        XCTAssertEqual(entries.map { $0["id"]! }, ["unknown", "new"])
        XCTAssertEqual(entries[0]["text_base64"], unknown["text_base64"])
        XCTAssertThrowsError(try CommandJournal.insert(entries, value: ["id": "another", "state": "unknown"], capacity: 2))
        let id = UUID().uuidString
        try CommandJournal.reserve(session: "protocol-test", id: id, operation: "chat.send", generation: "generation-1",
            connection: "connection-1", target: "agent-1", text: "完整原文\n不自动重发")
        defer { try? CommandJournal.settle(id, state: .confirmed) }
        XCTAssertEqual(CommandJournal.inputs().first(where: { $0.id == id })?.text, "完整原文\n不自动重发")
        try CommandJournal.settle(id, state: .confirmed)
        XCTAssertFalse(CommandJournal.inputs().contains(where: { $0.id == id }))
    }
}

private final class TestClock { var now: TimeInterval = 0 }
@MainActor private final class ScriptedGateway: GatewayAPI {
    nonisolated let origin = "https://test.invalid"
    var caps: Capabilities
    var page: EventPage
    var eventCalls = 0
    var listCalls = 0
    var creations: [String] = []
    var openedTargets: [String] = []
    var openedModes: [Bool] = []
    var createFailure: BridgeFailure?
    var openFailure: BridgeFailure?
    let owned = "live:" + String(repeating: "01", count: 16)
    var historyCursors: [String] = []
    var budgets: [ObjectIdentifier] = []
    var capabilitiesCost: TimeInterval = 0
    let clock: TestClock
    var handler: (@MainActor (BridgeRequest, ReadBudget?) async throws -> JSONObject)?
    var eventHandler: (@MainActor (Counter?, ReadBudget) async throws -> EventPage)?
    init(caps: Capabilities, clock: TestClock = TestClock()) throws {
        self.caps = caps; self.clock = clock
        page = EventPage(events: [BridgeEvent(sequence: try Counter("1"), topic: "ssmcp.event", value: "gap-page-A")],
            next: try Counter("2"), gap: true, hasMore: false)
    }
    func createSession(destination: String, terms: Bool, privacy: Bool, requestID: String) async throws -> String {
        creations.append(requestID)
        if let createFailure { throw createFailure }
        return owned
    }
    func sessions(offset: Int, budget: ReadBudget) async throws -> ([SessionSummary], Int?) {
        listCalls += 1; return ([], nil)
    }
    func open(session: String, readOnly: Bool, openRequestID: String) async throws -> OpenConnection {
        openedTargets.append(session); openedModes.append(readOnly)
        if let openFailure { throw openFailure }
        return OpenConnection(id: "connection-1", session: session, capabilities: caps)
    }
    func capabilities(_ id: String, budget: ReadBudget) async throws -> Capabilities {
        budgets.append(ObjectIdentifier(budget)); clock.now += capabilitiesCost; return caps
    }
    func bridge(_ id: String, _ request: BridgeRequest, budget: ReadBudget?) async throws -> JSONObject {
        if let budget { budgets.append(ObjectIdentifier(budget)) }
        if request.target == "conversation" {
            let query = try object(object(request.json["operation"])["query"])
            historyCursors.append(query["cursor"] as? String ?? "initial")
        }
        guard let handler else { throw BridgeFailure("test", "Missing test response") }
        return try await handler(request, budget)
    }
    func events(_ id: String, generation: String, cursor: Counter?, budget: ReadBudget, waitMS: Int) async throws -> EventPage {
        eventCalls += 1
        if let eventHandler { return try await eventHandler(cursor, budget) }
        return page
    }
    func close(_ id: String) async throws {}
}

final class ClientModelIntegrationTests: XCTestCase {
    @MainActor func testStartupAndOwnedCreationNeverDiscoverAnotherSession() async throws {
        let api = try ScriptedGateway(caps: testCaps([]))
        let model = ClientModel(gateway: api, loadPairing: false)
        model.setActive(true); model.setActive(false)
        XCTAssertTrue(api.creations.isEmpty); XCTAssertEqual(api.listCalls, 0); XCTAssertTrue(api.openedTargets.isEmpty)
        await model.createSession(destination: "ios@test.invalid", terms: false, privacy: false)
        XCTAssertEqual(model.connection?.session, api.owned); XCTAssertEqual(model.ownedSession, api.owned)
        XCTAssertEqual(api.creations.count, 1); XCTAssertEqual(api.openedTargets, [api.owned])
        XCTAssertEqual(api.openedModes, [false]); XCTAssertEqual(api.listCalls, 0)
    }

    @MainActor func testUnavailableAndUnconfirmedCreationNeverAttachOrDiscover() async throws {
        let api = try ScriptedGateway(caps: testCaps([]))
        let model = ClientModel(gateway: api, loadPairing: false)
        api.createFailure = BridgeFailure("unsupported", "此网关未提供独立会话创建功能。")
        await model.createSession(destination: "unsupported@test.invalid", terms: false, privacy: false)
        XCTAssertTrue(model.status.contains("未提供")); XCTAssertNil(model.connection)
        let destination = UUID().uuidString + "@test.invalid"
        api.createFailure = BridgeFailure("transport", "连接中断", effect: "may_have_occurred")
        await model.createSession(destination: destination, terms: false, privacy: false)
        let original = try XCTUnwrap(api.creations.last)
        defer { try? CommandJournal.settle(original, state: .rejected) }
        await model.createSession(destination: destination, terms: false, privacy: false)
        XCTAssertEqual(api.creations.count, 2); XCTAssertTrue(model.status.contains(original))
        XCTAssertEqual(api.listCalls, 0); XCTAssertTrue(api.openedTargets.isEmpty)
    }

    @MainActor func testCreateReceiptRemainsAvailableWhenControlIsDenied() async throws {
        let api = try ScriptedGateway(caps: testCaps([]))
        let model = ClientModel(gateway: api, loadPairing: false)
        api.openFailure = BridgeFailure("denied", "输入租约已被占用")
        await model.createSession(destination: "lease@test.invalid", terms: false, privacy: false)
        XCTAssertNil(model.connection); XCTAssertEqual(model.ownedSession, api.owned)
        api.openFailure = nil
        await model.open(try XCTUnwrap(model.ownedSession), readOnly: false)
        XCTAssertEqual(api.creations.count, 1); XCTAssertEqual(api.openedTargets, [api.owned, api.owned])
        XCTAssertEqual(api.listCalls, 0); XCTAssertEqual(model.connection?.session, api.owned)
    }

    @MainActor func testFailedGapRefreshRetainsPageAcrossPauseWithoutRefetching() async throws {
        let api = try ScriptedGateway(caps: testCaps(["runtime", "conversation"], events: true))
        var failRuntime = true
        api.handler = { request, _ in
            if request.target == "runtime", failRuntime { throw BridgeFailure("unavailable", "snapshot unavailable") }
            let payload = request.target == "conversation" ? testHistory(api.historyCursors.last == "initial" ? ["0"] : [], next: "1") : ["live_cursor": "2"]
            return try BridgeProtocol.validate(testResponse(request, payload: payload), for: request)
        }
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps), loadPairing: false)
        await model.refresh()
        XCTAssertNil(model.cursor); XCTAssertTrue(model.conversation.isEmpty); XCTAssertEqual(api.eventCalls, 1)
        model.setActive(false); failRuntime = false
        await model.refresh()
        XCTAssertEqual(api.eventCalls, 1); XCTAssertEqual(api.historyCursors.count, 1)
        XCTAssertEqual(model.cursor?.text, "2"); XCTAssertEqual(model.conversation.count, 1)
        XCTAssertEqual(model.timeline.filter { $0.text.contains("gap-page-A") }.count, 1)
        await model.refresh()
        XCTAssertEqual(api.eventCalls, 1); XCTAssertEqual(model.conversation.count, 1)
        XCTAssertEqual(model.timeline.filter { $0.text.contains("gap-page-A") }.count, 1)
    }
    @MainActor func testRefreshDeadlineRejectsLateSnapshotAcrossResources() async throws {
        let clock = TestClock(), api = try ScriptedGateway(caps: testCaps(["runtime", "terminals", "workers"]))
        var cost: TimeInterval = 0, name = "original"
        api.handler = { request, _ in
            clock.now += cost
            let payload: JSONObject
            switch request.target {
            case "terminals": payload = ["terminals": [["terminal_id": "main", "name": name, "cwd": "/"]]]
            case "workers": payload = ["pool_generation": "pool-1", "slots": [] as [JSONObject], "next_offset": NSNull()]
            default: payload = ["live_cursor": "1"]
            }
            return try BridgeProtocol.validate(testResponse(request, payload: payload), for: request)
        }
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps),
            loadPairing: false, makeBudget: { ReadBudget(clock: { clock.now }) })
        await model.refresh(); XCTAssertEqual(model.terminals.first?.name, "original")
        api.budgets = []; cost = 20; name = "late"
        // Three state reads cumulatively exceed one 55-second pass despite each being below 55 seconds.
        await model.refresh()
        XCTAssertEqual(Set(api.budgets).count, 1); XCTAssertEqual(clock.now, 60)
        XCTAssertEqual(model.terminals.first?.name, "original")
        XCTAssertTrue(model.status.contains("状态读取失败"))
    }
    @MainActor func testCancelledReadCannotPublishLateState() async throws {
        let api = try ScriptedGateway(caps: testCaps(["terminals"]))
        let started = expectation(description: "read started")
        var held: CheckedContinuation<JSONObject, Error>?, heldRequest: BridgeRequest?, readBudget: ReadBudget?
        api.handler = { request, budget in
            heldRequest = request; readBudget = budget
            return try await withCheckedThrowingContinuation { continuation in held = continuation; started.fulfill() }
        }
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps), loadPairing: false)
        let read = Task { await model.refresh() }
        await fulfillment(of: [started], timeout: 2)
        model.setActive(false); XCTAssertTrue(readBudget?.isCancelled == true)
        guard let held, let request = heldRequest else { XCTFail("Missing read continuation"); return }
        held.resume(returning: try BridgeProtocol.validate(testResponse(request, payload: ["terminals": [["terminal_id": "late"]]]), for: request))
        await read.value
        XCTAssertTrue(model.terminals.isEmpty); XCTAssertTrue(model.status.contains("观察已暂停"))
    }
    @MainActor func testSupersededHistoryCannotOverwriteNewCursorOrDuplicateRows() async throws {
        let api = try ScriptedGateway(caps: testCaps(["conversation"]))
        let started = expectation(description: "old history held")
        var held: CheckedContinuation<JSONObject, Error>?, heldRequest: BridgeRequest?
        api.handler = { request, _ in
            if api.historyCursors.count == 1 {
                heldRequest = request
                return try await withCheckedThrowingContinuation { continuation in held = continuation; started.fulfill() }
            }
            let sequences = api.historyCursors.last == "initial" ? ["0", "1"] : []
            return try BridgeProtocol.validate(testResponse(request, payload: testHistory(sequences, next: "2")), for: request)
        }
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps), loadPairing: false)
        let old = Task { await model.refresh() }
        await fulfillment(of: [started], timeout: 2)
        await model.refresh(); XCTAssertEqual(model.conversation.count, 2)
        guard let held, let request = heldRequest else { XCTFail("Missing old continuation"); return }
        held.resume(returning: try BridgeProtocol.validate(testResponse(request, payload: testHistory(["0"], next: "1")), for: request))
        await old.value
        await model.refresh()
        XCTAssertEqual(api.historyCursors, ["initial", "initial", "2"])
        XCTAssertEqual(model.conversation.count, 2)
    }
    @MainActor func testLateObserverCannotRestorePageBehindCommittedCursor() async throws {
        let api = try ScriptedGateway(caps: testCaps([], events: true))
        let firstStarted = expectation(description: "old observer waiting")
        let advanced = expectation(description: "new observer advanced")
        var oldReply: CheckedContinuation<EventPage, Error>?, nextReply: CheckedContinuation<EventPage, Error>?
        api.eventHandler = { cursor, _ in
            if api.eventCalls == 1 {
                XCTAssertEqual(cursor?.text, "10")
                return try await withCheckedThrowingContinuation { oldReply = $0; firstStarted.fulfill() }
            }
            if api.eventCalls == 2 {
                return EventPage(events: [BridgeEvent(sequence: try Counter("10"), topic: "ssmcp.event", value: "new-page")],
                    next: try Counter("20"), gap: false, hasMore: false)
            }
            XCTAssertEqual(cursor?.text, "20")
            return try await withCheckedThrowingContinuation { nextReply = $0; advanced.fulfill() }
        }
        let inbox = EventInbox(generation: "generation-1", cursor: try Counter("10"))
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps),
            initialInbox: inbox, loadPairing: false)
        model.setActive(true)
        await fulfillment(of: [firstStarted], timeout: 2)
        let oldTask = model.observationTask
        model.setActive(false); model.setActive(true)
        await fulfillment(of: [advanced], timeout: 2)
        XCTAssertEqual(model.cursor?.text, "20")
        oldReply?.resume(returning: EventPage(events: [BridgeEvent(sequence: try Counter("10"), topic: "ssmcp.event", value: "old-page")],
            next: try Counter("11"), gap: false, hasMore: false))
        await oldTask?.value
        XCTAssertNil(inbox.pending); XCTAssertEqual(model.cursor?.text, "20")
        let currentTask = model.observationTask
        model.setActive(false)
        nextReply?.resume(returning: EventPage(events: [], next: try Counter("20"), gap: false, hasMore: false))
        await currentTask?.value
        await model.refresh()
        XCTAssertEqual(model.cursor?.text, "20")
        XCTAssertEqual(model.timeline.filter { $0.text.contains("new-page") }.count, 1)
        XCTAssertFalse(model.timeline.contains { $0.text.contains("old-page") })
    }
    @MainActor func testPausedObserverRetainsValidatedPageAtOriginalCursor() async throws {
        let api = try ScriptedGateway(caps: testCaps([], events: true))
        let started = expectation(description: "observer waiting")
        var reply: CheckedContinuation<EventPage, Error>?
        api.eventHandler = { _, _ in
            try await withCheckedThrowingContinuation { reply = $0; started.fulfill() }
        }
        let inbox = EventInbox(generation: "generation-1", cursor: try Counter("10"))
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps),
            initialInbox: inbox, loadPairing: false)
        model.setActive(true); await fulfillment(of: [started], timeout: 2)
        let task = model.observationTask
        model.setActive(false)
        reply?.resume(returning: EventPage(events: [BridgeEvent(sequence: try Counter("10"), topic: "ssmcp.event", value: "paused-page")],
            next: try Counter("11"), gap: false, hasMore: false))
        await task?.value
        XCTAssertEqual(model.cursor?.text, "10"); XCTAssertNotNil(inbox.pending)
        await model.refresh()
        XCTAssertEqual(api.eventCalls, 1); XCTAssertEqual(model.cursor?.text, "11"); XCTAssertNil(inbox.pending)
        XCTAssertEqual(model.timeline.filter { $0.text.contains("paused-page") }.count, 1)
    }
    @MainActor func testEventGapRefreshUsesRemainingObservationDeadline() async throws {
        let clock = TestClock(), api = try ScriptedGateway(caps: testCaps(["runtime"], events: true))
        let started = expectation(description: "event window waiting")
        var reply: CheckedContinuation<EventPage, Error>?, stateReads = 0
        api.handler = { request, _ in
            stateReads += 1
            if stateReads > 1 { clock.now += 6 }
            return try BridgeProtocol.validate(testResponse(request, payload: ["live_cursor": "20"]), for: request)
        }
        api.eventHandler = { _, _ in
            try await withCheckedThrowingContinuation { reply = $0; started.fulfill() }
        }
        let inbox = EventInbox(generation: "generation-1", cursor: try Counter("10"))
        let model = ClientModel(gateway: api, initialConnection: OpenConnection(id: "connection-1", session: "test", capabilities: api.caps),
            initialInbox: inbox, loadPairing: false, makeBudget: { ReadBudget(clock: { clock.now }) })
        model.setActive(true); await fulfillment(of: [started], timeout: 2)
        let task = model.observationTask
        clock.now = 50
        reply?.resume(returning: EventPage(events: [BridgeEvent(sequence: try Counter("19"), topic: "ssmcp.event", value: "gap-after-wait")],
            next: try Counter("20"), gap: true, hasMore: false))
        await task?.value
        XCTAssertEqual(clock.now, 56); XCTAssertEqual(model.cursor?.text, "10"); XCTAssertNotNil(inbox.pending)
        XCTAssertTrue(model.status.contains("状态读取失败"))
        model.setActive(false); await model.refresh()
        XCTAssertEqual(api.eventCalls, 1); XCTAssertEqual(model.cursor?.text, "20"); XCTAssertNil(inbox.pending)
    }

}

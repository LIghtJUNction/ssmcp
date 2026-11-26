import Foundation
import XCTest
@testable import SSMCP

final class DirectRelayTests: XCTestCase {
    private let client = String(repeating: "a", count: 64)
    private let second = String(repeating: "b", count: 64)
    private let capabilities = ["session_access_v1", "control_attribution_v1"]
    private func make() throws -> DeviceWire {
        DeviceWire(try DeviceRelay(username: "Alice", server: "https://RELAY.example:443/"))
    }
    private func resolved(_ device: DeviceWire, subject: String = "123") -> JSONObject {
        ["type": "resolved_controller", "id": 0, "principal": device.relay.principal, "provider": "github", "subject": subject]
    }
    private func ready(_ id: String) -> JSONObject {
        ["type": "ready", "client_id": id, "url": "https://relay.example/mcp", "login_code": "",
         "login_expires_in": 0, "capabilities": capabilities]
    }
    private func admitted(_ id: String) throws -> DeviceWire {
        var device = try make()
        let reply = resolved(device)
        let hello = try XCTUnwrap(device.receive(reply, bind: { relay, subject in
            XCTAssertEqual(relay.principal, "alice@relay.example"); XCTAssertEqual(subject, "123")
        }))
        XCTAssertEqual(hello["version"] as? Int, 7)
        XCTAssertEqual((hello["metadata"] as? JSONObject)?["allow"] as? [String], ["alice@relay.example"])
        XCTAssertNil(try device.receive(ready(id), bind: { _,_ in XCTFail("unexpected second admission") }))
        return device
    }
    private func exchange(_ device: inout DeviceWire, _ value: JSONObject) throws -> JSONObject {
        let reply = try XCTUnwrap(device.receive(value, bind: { _,_ in XCTFail("unexpected admission") }))
        XCTAssertEqual(reply["type"] as? String, "reply")
        XCTAssertEqual((reply["id"] as? NSNumber)?.uint64Value, (value["id"] as? NSNumber)?.uint64Value)
        return try XCTUnwrap(reply["result"] as? JSONObject)
    }
    private func call(_ id: UInt64, _ operation: JSONObject, subject: String = "123") -> JSONObject {
        let agent = "agent_" + String(repeating: "a", count: 32)
        return ["type": "call", "id": id, "operation": operation,
                "attribution": ["account": ["provider":"github", "subject":subject, "login":"alice"], "agent": ["id":agent, "name":"fixture"]],
                "code_context": ["run_id": String(repeating:"0", count:32), "code":"", "timestamp_ms":1] as JSONObject]
    }
    func testTwoFieldRoutingAndFreshAdmission() throws {
        let device = try make()
        XCTAssertEqual(device.relay.socketRequest.url?.absoluteString, "wss://relay.example/api/client/connect")
        XCTAssertEqual(device.relay.socketRequest.value(forHTTPHeaderField:"ssmcp-capabilities"), capabilities.joined(separator:","))
        XCTAssertNil(device.relay.socketRequest.value(forHTTPHeaderField:"Authorization"))
        XCTAssertEqual(device.start["type"] as? String, "resolve_controller")
        XCTAssertEqual(device.start["id"] as? Int, 0)
        XCTAssertEqual(try admitted(client).clientID, client)
        XCTAssertEqual(try admitted(second).clientID, second)
        for url in ["http://remote.example", "https://name@host.test", "https://host.test/gateway", "https://host.test?token=x", "https://host.test/#x"] {
            XCTAssertThrowsError(try DeviceRelay(username:"alice",server:url))
        }
        XCTAssertThrowsError(try DeviceRelay(username:"a@b",server:"relay.example"))
    }
    func testUnverifiedReadyOrReassignedControllerCannotAdmit() throws {
        var device = try make()
        XCTAssertThrowsError(try device.receive(ready(client), bind: {_,_ in }))
        for subject in ["0","001","-2","18446744073709551616"] {
            let rejected = resolved(device, subject: subject)
            XCTAssertThrowsError(try device.receive(rejected, bind: {_,_ in XCTFail("invalid subject reached pin")}))
        }
        let admission = resolved(device)
        XCTAssertThrowsError(try device.receive(admission, bind: {_,_ in throw BridgeFailure("changed_identity","reassigned")}))
        XCTAssertEqual(device.phase, .resolving)
        _ = try device.receive(admission, bind: {_,_ in })
        var unbound = ready(client); unbound["login_code"] = "pairing-id"
        XCTAssertThrowsError(try device.receive(unbound, bind: {_,_ in }))
        var missingAttribution = ready(client); missingAttribution["capabilities"] = ["session_access_v1"]
        XCTAssertThrowsError(try device.receive(missingAttribution, bind: {_,_ in }))
        XCTAssertThrowsError(try device.receive(ready("live:"+String(client.prefix(32))), bind: {_,_ in }))
        XCTAssertEqual(device.phase, .awaitingReady)
    }
    func testAttributionPermissionsDuplicateCallAndConcurrentIDs() throws {
        var device = try admitted(client)
        let list = try exchange(&device, call(8,["op":"list_tools"]))
        let tools = (list["Ok"] as? JSONObject)?["tools"] as? [JSONObject]
        XCTAssertEqual(tools?.compactMap { $0["name"] as? String },["ssmcp.client.capabilities","ssmcp.ui.message"])
        XCTAssertNotNil(try exchange(&device,call(9,["op":"write","data":"x"]))["Err"])
        let operation: JSONObject = ["op":"call_tool","name":"ssmcp.ui.message","arguments":["text":"testing","kind":"answer","idempotency_key":"msg1"]]
        XCTAssertNotNil(try exchange(&device, call(4,operation,subject:"124"))["Err"])
        var unbound = call(5,operation); unbound.removeValue(forKey:"code_context")
        XCTAssertNotNil(try exchange(&device,unbound)["Err"])
        XCTAssertTrue(device.lines.isEmpty)
        let largeID: UInt64 = 9_007_199_254_740_993
        XCTAssertNotNil(try exchange(&device,call(largeID,operation))["Ok"])
        XCTAssertNotNil(try exchange(&device,call(largeID,operation))["Err"])
        let again = try exchange(&device,call(2,operation))
        let body = (again["Ok"] as? JSONObject)?["structuredContent"] as? JSONObject
        XCTAssertEqual(body?["duplicate"] as? Bool,true)
        XCTAssertEqual(device.lines.map(\.text), ["testing"])
        let maintenance: JSONObject = ["type":"relay_maintenance","id":10,"operation":operation]
        XCTAssertNotNil(try exchange(&device,maintenance)["Err"])
        let probe: JSONObject = ["op":"call_tool","name":"ssmcp.client.capabilities","arguments":[:] as JSONObject]
        let ok = try exchange(&device,["type":"relay_maintenance","id":11,"operation":probe])
        let caps = (ok["Ok"] as? JSONObject)?["structuredContent"] as? JSONObject
        XCTAssertEqual(caps?["schema"] as? String,"ssmcp.client.capabilities.v1")
        XCTAssertFalse((caps?["client_version"] as? String ?? "").isEmpty)
    }
    func testPublishReceiptsCorrelateAcrossSessionsAndRetainUnknown() throws {
        var original = try admitted(client)
        let issued = try original.publish("draft"), command = try XCTUnwrap(issued["command"] as? JSONObject)
        let message = try XCTUnwrap(command["message_id"] as? String)
        let valid: JSONObject = ["id":message,"text":"draft","revision":1,"target":"next","state":"queued"]
        func receipt(_ id: UInt64, _ cid: String, _ row: JSONObject) -> JSONObject {
            ["type":"steering_reply","id":id,"result":["Ok":["schema_version":1,"client_id":cid,"messages":[row]] as JSONObject]]
        }
        var wrongText = valid; wrongText["text"] = "different"
        for frame in [receipt(2,client,valid),receipt(1,second,valid),receipt(1,client,wrongText)] {
            var interrupted = original
            XCTAssertThrowsError(try interrupted.receive(frame,bind:{_,_ in }))
            interrupted.lostReceipt(); XCTAssertEqual(interrupted.lines.first?.state,"结果待确认")
        }
        XCTAssertThrowsError(try original.publish("duplicate before receipt"))
        XCTAssertNil(try original.receive(receipt(1,client,valid),bind:{_,_ in }))
        XCTAssertEqual(original.lines.first?.state,"已入队")
        XCTAssertFalse(original.awaitingReceipt)
    }
}

import Network

/// The local peer exercises URLSessionWebSocketTask on a real loopback transport.
/// A dead peer cancels the connection after eight seconds to bound every read.
private final class WebSocketFixture {
    let listener: NWListener
    let queue = DispatchQueue(label: "ssmcp.tests.device-wire")

    init() throws {
        let parameters = NWParameters.tcp
        parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: .any)
        let ws = NWProtocolWebSocket.Options()
        ws.autoReplyPing = true
        parameters.defaultProtocolStack.applicationProtocols.insert(ws, at: 0)
        listener = try NWListener(using: parameters)
    }
    func receive(_ peer: NWConnection) async throws -> JSONObject {
        let data: Data = try await withCheckedThrowingContinuation { resume in
            peer.receiveMessage { bytes, _, _, error in
                if let error { resume.resume(throwing: error) }
                else if let bytes { resume.resume(returning: bytes) }
                else { resume.resume(throwing: BridgeFailure("fixture", "socket ended before frame")) }
            }
        }
        return try object(JSONSerialization.jsonObject(with: data))
    }
    func send(_ frame: JSONObject, to peer: NWConnection) async throws {
        var data = try jsonData(frame); data.append(10)
        let context = NWConnection.ContentContext(identifier: UUID().uuidString, metadata: [NWProtocolWebSocket.Metadata(opcode:.binary)])
        try await withCheckedThrowingContinuation { (resume: CheckedContinuation<Void, Error>) in
            peer.send(content:data, contentContext:context, isComplete:true, completion:.contentProcessed { error in
                if let error { resume.resume(throwing:error) } else { resume.resume() }
            })
        }
    }
}

extension DirectRelayTests {
    @MainActor private func waitFor(_ predicate: () -> Bool, _ failure: String) async throws {
        let end = Date().addingTimeInterval(5)
        while !predicate() && Date() < end { try await Task.sleep(nanoseconds: 15_000_000) }
        guard predicate() else { throw BridgeFailure("fixture",failure) }
    }
    @MainActor func testSocketStartsNativeSessionAndSettlesExactPublication() async throws {
        let fixture = try WebSocketFixture()
        let readyListener = expectation(description: "loopback listener ready")
        let accepted = expectation(description: "device opened a WebSocket")
        let model = IOSRelaySession(bind: { _, subject in XCTAssertEqual(subject, "123") })
        var connected: NWConnection?
        fixture.listener.stateUpdateHandler = { state in
            if case .ready = state { readyListener.fulfill() }
        }
        fixture.listener.newConnectionHandler = { connection in
            connected = connection
            connection.start(queue: fixture.queue)
            fixture.queue.asyncAfter(deadline:.now()+8) { connection.cancel() }
            accepted.fulfill()
        }
        fixture.listener.start(queue:fixture.queue)
        defer { connected?.cancel(); fixture.listener.cancel(); model.end("test complete") }
        await fulfillment(of:[readyListener],timeout:4)
        let port = try XCTUnwrap(fixture.listener.port?.rawValue)
        let origin = "http://127.0.0.1:" + String(port)
        model.connect(username:"fixture",server:origin)
        await fulfillment(of:[accepted],timeout:4)
        let peer = try XCTUnwrap(connected)
        let resolve = try await fixture.receive(peer)
        XCTAssertEqual(resolve["principal"] as? String, "fixture@"+origin)
        try await fixture.send(["type":"resolved_controller","id":0,"principal":"fixture@"+origin,"provider":"github","subject":"123"],to:peer)
        let hello = try await fixture.receive(peer)
        XCTAssertEqual(hello["type"] as? String,"hello")
        XCTAssertEqual((hello["metadata"] as? JSONObject)?["allow"] as? [String],["fixture@"+origin])
        try await fixture.send(ready(client),to:peer)
        try await waitFor({ model.clientID == self.client },"device did not enter online state")
        model.draft = "real socket"; model.publish()
        let frame = try await fixture.receive(peer)
        XCTAssertEqual(frame["type"] as? String,"steering")
        let command = try XCTUnwrap(frame["command"] as? JSONObject)
        let id = try XCTUnwrap(command["message_id"] as? String)
        let message: JSONObject = ["id":id,"text":"real socket","target":"next","state":"queued","revision":1]
        let snapshot: JSONObject = ["client_id":client,"schema_version":1,"messages":[message]]
        let requestID = try XCTUnwrap(frame["id"] as? NSNumber)
        try await fixture.send(["type":"steering_reply","id":requestID,"result":["Ok":snapshot]],to:peer)
        try await waitFor({ !model.sending },"device did not settle matching receipt")
        XCTAssertEqual(model.lines.first?.state,"已入队"); XCTAssertEqual(model.draft,"")
    }
    /// Opt in by passing the origin of the repo's isolated Rust account-router fixture.
    @MainActor func testAgainstRealRustAccountRouterWhenProvided() async throws {
        guard let address = ProcessInfo.processInfo.environment["SSMCP_IOS_FIXTURE_ORIGIN"] else {
            throw XCTSkip("Set TEST_RUNNER_SSMCP_IOS_FIXTURE_ORIGIN to the Rust loopback fixture URL")
        }
        let endpoint = try DeviceRelay(username:"relayfixture",server:address)
        guard endpoint.origin.scheme == "http", endpoint.origin.host == "127.0.0.1", endpoint.origin.port != nil else {
            throw BridgeFailure("fixture","The opt-in test requires the repository's isolated loopback router")
        }
        let first = IOSRelaySession(bind:{ _, subject in XCTAssertEqual(subject,"123") })
        let next = IOSRelaySession(bind:{ _, subject in XCTAssertEqual(subject,"123") })
        defer { first.end("test complete"); next.end("test complete") }
        first.connect(username:"relayfixture",server:address)
        next.connect(username:"relayfixture",server:address)
        try await waitFor({ !first.clientID.isEmpty && !next.clientID.isEmpty },"Rust router did not admit two device sessions")
        XCTAssertNotEqual(first.clientID,next.clientID)
        first.draft = "Rust loopback receipt"; first.publish()
        try await waitFor({ !first.sending },"Rust router did not return a receipt")
        XCTAssertEqual(first.lines.first?.state,"已入队"); XCTAssertTrue(first.draft.isEmpty)
        XCTAssertTrue(next.lines.isEmpty)
    }
}

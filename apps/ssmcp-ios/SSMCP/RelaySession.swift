import Foundation
import Combine
import CoreFoundation
import Security

struct DeviceRelay {
    let origin: URL
    let principal: String
    let username: String
    var isHosted: Bool { origin.host?.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: ".")) == "ssmcp.lmm.best" }

    init(username input: String, server address: String) throws {
        let username = input.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        let ascii = Array(username.utf8)
        let letter: (UInt8) -> Bool = { (97...122).contains($0) }
        let digit: (UInt8) -> Bool = { (48...57).contains($0) }
        guard (3...32).contains(ascii.count), let first = ascii.first, let last = ascii.last, letter(first),
              letter(last) || digit(last), ascii.allSatisfy({ letter($0) || digit($0) || $0 == 45 || $0 == 95 }) else {
            throw BridgeFailure("username", "用户名须以英文字母开头，使用 3–32 个字母、数字、连字符或下划线。")
        }
        let raw = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !raw.isEmpty, !raw.contains(where: { $0.isWhitespace || "\\%".contains($0) }),
              var parts = URLComponents(string: raw.contains("://") ? raw : "https://" + raw),
              let host = parts.host?.lowercased(), !host.isEmpty, parts.user == nil, parts.password == nil,
              parts.query == nil, parts.fragment == nil, parts.path.isEmpty || parts.path == "/",
              parts.port.map({ (1...65535).contains($0) }) ?? true else {
            throw BridgeFailure("server", "请输入中继服务器的 HTTPS 地址或域名。")
        }
        let scheme = parts.scheme?.lowercased()
        guard scheme == "https" || (scheme == "http" && ["127.0.0.1", "::1", "[::1]"].contains(host)) else {
            throw BridgeFailure("server", "远程中继需要 HTTPS。")
        }
        parts.host = host; parts.scheme = scheme; parts.path = ""
        if (scheme == "https" && parts.port == 443) || (scheme == "http" && parts.port == 80) { parts.port = nil }
        guard let origin = parts.url else { throw BridgeFailure("server", "中继服务器地址无效。") }
        self.origin = origin
        self.username = username
        self.principal = username + "@" + (scheme == "https" ? String(origin.absoluteString.dropFirst(8)) : origin.absoluteString)
    }

    var socketRequest: URLRequest {
        var parts = URLComponents(url: origin, resolvingAgainstBaseURL: false)!
        parts.scheme = origin.scheme == "https" ? "wss" : "ws"; parts.path = "/api/client/connect"
        var request = URLRequest(url: parts.url!, timeoutInterval: 25)
        request.setValue("session_access_v1,control_attribution_v1", forHTTPHeaderField: "ssmcp-capabilities")
        return request
    }
}

/// Device registration submits no account credential. The approved account is identified by
/// relay origin and stable numeric GitHub subject. An alias cannot silently change its owner.
enum RelayControllerPin {
    private static func query(_ principal: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: "dev.ssmcp.ios.controller.v1",
         kSecAttrAccount as String: principal]
    }
    static func bind(_ relay: DeviceRelay, subject: String) throws {
        var item = query(relay.principal)
        item[kSecValueData as String] = Data(subject.utf8)
        item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        let created = SecItemAdd(item as CFDictionary, nil)
        if created == errSecSuccess { return }
        guard created == errSecDuplicateItem else { throw BridgeFailure("keychain", "无法保存本机授权。") }
        var lookup = query(relay.principal); lookup[kSecReturnData as String] = true
        var result: CFTypeRef?
        guard SecItemCopyMatching(lookup as CFDictionary, &result) == errSecSuccess, let data = result as? Data else {
            throw BridgeFailure("keychain", "无法读取本机授权。")
        }
        guard data == Data(subject.utf8) else { throw BridgeFailure("changed_identity", "这个用户名绑定的 GitHub 身份已改变。") }
    }
    static func forget(_ relay: DeviceRelay) throws {
        let result = SecItemDelete(query(relay.principal) as CFDictionary)
        guard result == errSecSuccess || result == errSecItemNotFound else { throw BridgeFailure("keychain", "无法清除原身份绑定。") }
    }
}

final class RelayTransport: NSObject, URLSessionTaskDelegate {
    lazy var session: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.httpCookieStorage = nil; config.urlCredentialStorage = nil; config.urlCache = nil
        config.timeoutIntervalForRequest = 25; config.httpShouldSetCookies = false
        return URLSession(configuration: config, delegate: self, delegateQueue: nil)
    }()
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}

private func wireUInt(_ value: Any?) -> UInt64? {
    guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID(),
          "cCsSiIlLqQ".contains(String(cString: n.objCType)) else { return nil }
    return UInt64(n.stringValue)
}
private func lowerHex(_ value: String, bytes: Int) -> Bool {
    value.utf8.count == bytes * 2 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) }
}
private func messageToken() -> String { UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased() }

struct DeviceChatLine: Identifiable {
    let id: String
    let author: String
    let text: String
    var state: String
}

struct DeviceWire {
    enum Phase: Equatable { case resolving, awaitingReady, online }
    private(set) var phase = Phase.resolving
    private(set) var subject = ""
    private(set) var clientID = ""
    private(set) var lines: [DeviceChatLine] = []
    let relay: DeviceRelay
    private var nextID: UInt64 = 1
    private var publication: (id: UInt64, message: String, text: String)?
    private var recentCalls: [UInt64] = []
    private var keys: [(String, String, String, String)] = []
    var awaitingReceipt: Bool { publication != nil }
    var start: JSONObject { ["type": "resolve_controller", "id": 0, "principal": relay.principal] }
    init(_ relay: DeviceRelay) { self.relay = relay }

    mutating func publish(_ text: String) throws -> JSONObject {
        guard phase == .online, publication == nil, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              text.utf8.count <= 16 * 1024, nextID < UInt64.max else { throw BridgeFailure("input", "请输入 1–16384 字节的消息并等待上一份回执。") }
        let id = nextID, message = messageToken(); nextID += 1
        publication = (id, message, text)
        append(DeviceChatLine(id: message, author: "我", text: text, state: "等待中继确认"))
        return ["type": "steering", "id": id, "command": ["op": "publish", "command_id": messageToken(), "message_id": message,
                 "expected_revision": 0, "text": text, "target": "next"] as JSONObject]
    }

    mutating func lostReceipt() {
        if let p = publication, let i = lines.firstIndex(where: { $0.id == p.message }) { lines[i].state = "结果待确认" }
        publication = nil
    }
    private mutating func append(_ line: DeviceChatLine) {
        if lines.count >= 128, let i = lines.firstIndex(where: { $0.id != publication?.message }) { lines.remove(at: i) }
        lines.append(line)
    }
    private func rpc(_ value: JSONObject) -> JSONObject { ["content": [["type":"text", "text": String(data: (try? jsonData(value)) ?? Data(), encoding: .utf8) ?? "{}"]], "structuredContent": value, "isError": false] }
    private var capabilities: JSONObject {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "1"
        return ["schema": "ssmcp.client.capabilities.v1", "client_version": version, "wire_version": 7,
                "platform": "ios", "chat_enabled": true]
    }

    mutating func receive(_ frame: JSONObject, bind: (DeviceRelay, String) throws -> Void) throws -> JSONObject? {
        let type = frame["type"] as? String
        if type == "reject" { throw BridgeFailure("relay_rejected", String((frame["message"] as? String ?? "中继拒绝连接").prefix(512))) }
        switch phase {
        case .resolving:
            guard type == "resolved_controller", wireUInt(frame["id"]) == 0, frame["principal"] as? String == relay.principal,
                  frame["provider"] as? String == "github", let id = frame["subject"] as? String,
                  let n = UInt64(id), n > 0, String(n) == id else { throw BridgeFailure("admission", "中继未确认所选账号的 GitHub 身份。") }
            try bind(relay, id); subject = id; phase = .awaitingReady
            return ["type":"hello", "version":7, "mode":"shell", "metadata":["name":"SSMCP iOS","hostname":"iOS","cwd":"/","allow":[relay.principal]] as JSONObject]
        case .awaitingReady:
            guard type == "ready", let client = frame["client_id"] as? String, lowerHex(client, bytes: 32),
                  frame["login_code"] as? String == "", wireUInt(frame["login_expires_in"]) == 0,
                  let caps = frame["capabilities"] as? [String], caps.contains("session_access_v1"), caps.contains("control_attribution_v1")
            else { throw BridgeFailure("admission", "中继未确认独立会话和所选账号的访问权限。") }
            clientID = client; phase = .online; return nil
        case .online:
            switch type {
            case "steering_reply":
                guard let p = publication, wireUInt(frame["id"]) == p.id, let result = frame["result"] as? JSONObject,
                      result.count == 1, let i = lines.firstIndex(where: { $0.id == p.message }) else { throw BridgeFailure("receipt", "消息回执无法关联原请求。") }
                if let reason = result["Err"] as? String { lines[i].state = "中继拒绝：" + String(reason.prefix(160)); publication = nil; return nil }
                guard let ok = result["Ok"] as? JSONObject, wireUInt(ok["schema_version"]) == 1, ok["client_id"] as? String == clientID,
                      let messages = ok["messages"] as? [JSONObject], let received = messages.first(where: { $0["id"] as? String == p.message }),
                      received["text"] as? String == p.text, received["target"] as? String == "next",
                      let revision = wireUInt(received["revision"]), revision > 0, let state = received["state"] as? String,
                      ["queued","delivered"].contains(state) else { throw BridgeFailure("receipt", "消息内容或会话与原请求不符。") }
                lines[i].state = state == "queued" ? "已入队" : "已投递"; publication = nil; return nil
            case "call", "relay_maintenance":
                guard let id = wireUInt(frame["id"]), id > 0, let operation = frame["operation"] as? JSONObject else { throw BridgeFailure("call", "无效的中继调用。") }
                guard !recentCalls.contains(id) else { return ["type":"reply","id":id,"result":["Err":"call ID was already received"]] }
                if recentCalls.count == 256 { recentCalls.removeFirst() }; recentCalls.append(id)
                let answer: JSONObject
                do { answer = ["Ok": try dispatch(operation, frame: frame, maintenance: type == "relay_maintenance")] }
                catch { answer = ["Err": error.localizedDescription] }
                return ["type":"reply","id":id,"result":answer]
            case "code_mode_finished": return nil
            default: throw BridgeFailure("wire", "中继发送了此客户端未协商的数据帧。")
            }
        }
    }

    private mutating func dispatch(_ operation: JSONObject, frame: JSONObject, maintenance: Bool) throws -> JSONObject {
        let op = operation["op"] as? String, name = operation["name"] as? String
        let args = operation["arguments"] as? JSONObject
        if maintenance {
            guard op == "call_tool", name == "ssmcp.client.capabilities", args?.isEmpty == true else { throw BridgeFailure("denied", "Unsupported relay maintenance operation") }
            return rpc(capabilities)
        }
        let attribution = frame["attribution"] as? JSONObject
        guard let account = attribution?["account"] as? JSONObject, account["provider"] as? String == "github",
              account["subject"] as? String == subject else { throw BridgeFailure("denied", "This call lacks the admitted controller identity") }
        switch op {
        case "status": return ["platform":"ios","client_id":clientID,"connected":true,"terminal":false]
        case "list_tools":
            guard operation["cursor"] == nil || operation["cursor"] is NSNull else { throw BridgeFailure("cursor", "Invalid tool cursor") }
            let noArguments: JSONObject = ["type": "object", "properties": [:] as JSONObject, "additionalProperties": false]
            let properties: JSONObject = [
                "text": ["type": "string"],
                "kind": ["enum": ["progress", "answer"]],
                "idempotency_key": ["type": "string"],
                "finish_task": ["const": false]
            ]
            let schema: JSONObject = ["type": "object", "required": ["text"], "properties": properties, "additionalProperties": false]
            let tools: [JSONObject] = [
                ["name": "ssmcp.client.capabilities", "description": "Read iOS session capabilities.", "inputSchema": noArguments],
                ["name": "ssmcp.ui.message", "description": "Display agent progress or an answer on this iPhone.", "inputSchema": schema]
            ]
            return ["tools": tools]
        case "call_tool" where name == "ssmcp.client.capabilities" && args?.isEmpty == true: return rpc(capabilities)
        case "call_tool" where name == "ssmcp.ui.message":
            guard let args, Set(args.keys).isSubset(of: ["text","kind","idempotency_key","finish_task"]),
                  let text = args["text"] as? String, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, text.utf8.count <= 16384,
                  let agent = attribution?["agent"] as? JSONObject, let owner = agent["id"] as? String,
                  owner.hasPrefix("agent_"), lowerHex(String(owner.dropFirst(6)), bytes:16),
                  let context = frame["code_context"] as? JSONObject, let run = context["run_id"] as? String, lowerHex(run,bytes:16),
                  args["finish_task"] == nil || (try? boolean(args["finish_task"])) == false else { throw BridgeFailure("denied","Expected an attributed agent message in a verified run") }
            let kind = args["kind"] as? String ?? "progress"
            guard ["progress","answer"].contains(kind), args["kind"] == nil || args["kind"] is String else { throw BridgeFailure("message","Unsupported message kind") }
            var id = messageToken(), duplicate = false
            if let value = args["idempotency_key"] {
                guard let key = value as? String, (1...96).contains(key.utf8.count), key.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 95 }) else { throw BridgeFailure("message","Invalid idempotency key") }
                let scoped = owner + "/" + key
                if let previous = keys.first(where: { $0.0 == scoped }) {
                    guard previous.1 == text, previous.2 == kind else { throw BridgeFailure("conflict","This key was used with different content") }
                    id = previous.3; duplicate = true
                } else { if keys.count == 64 { keys.removeFirst() }; keys.append((scoped,text,kind,id)) }
            }
            if !duplicate { append(DeviceChatLine(id:id,author:"Agent",text:text,state:kind == "answer" ? "答复" : "进度")) }
            return rpc(["shown":true,"accepted":true,"recorded":true,"storage":"memory","message_id":id,"kind":kind,"duplicate":duplicate])
        default: throw BridgeFailure("unsupported","This iOS session does not support that operation")
        }
    }
}

@MainActor final class IOSRelaySession: ObservableObject {
    @Published private(set) var clientID = ""
    @Published private(set) var status = ""
    @Published private(set) var connecting = false
    @Published private(set) var sending = false
    @Published private(set) var lines: [DeviceChatLine] = []
    @Published private(set) var mayRegister = false
    @Published private(set) var identityChanged = false
    @Published private(set) var uncertain = CommandJournal.inputs().filter { $0.operation == "relay.publish" }
    @Published var draft = ""
    private var wire: DeviceWire?
    private var changedRelay: DeviceRelay?
    private var transport: RelayTransport?
    private var socket: URLSessionWebSocketTask?
    private var loop: Task<Void, Never>?
    private var heartbeat: Task<Void, Never>?
    private var receiptDeadline: Task<Void, Never>?
    private var lastPong = Date()
    private var lastSubmission: (id:String, text:String)?
    private var generation = UUID()
    private let bind: (DeviceRelay, String) throws -> Void

    init(bind: @escaping (DeviceRelay, String) throws -> Void = RelayControllerPin.bind) { self.bind = bind }

    func connect(username: String, server: String) {
        guard socket == nil else { return }
        mayRegister = false; identityChanged = false; changedRelay = nil
        do {
            let relay = try DeviceRelay(username: username, server: server)
            let link = RelayTransport(), ws = link.session.webSocketTask(with: relay.socketRequest)
            ws.maximumMessageSize = 512 * 1024
            generation = UUID(); let epoch = generation
            transport = link; socket = ws; wire = DeviceWire(relay)
            connecting = true; status = "正在验证中继账号"; lines.removeAll(); lastPong = Date()
            ws.resume()
            loop = Task { [weak self] in await self?.run(ws, epoch: epoch) }
            heartbeat = Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(nanoseconds: 10_000_000_000)
                    guard !Task.isCancelled, let self, self.generation == epoch else { return }
                    if Date().timeIntervalSince(self.lastPong) > (self.connecting ? 25 : 65) {
                        self.end(self.connecting ? "中继未在期限内确认会话。" : "与中继的连接已中断。"); return
                    }
                    if !self.connecting {
                        ws.sendPing { [weak self] error in
                            Task { @MainActor in
                                guard let self, self.generation == epoch else { return }
                                if error == nil { self.lastPong = Date() }
                                else { self.end("中继心跳失败。") }
                            }
                        }
                    }
                }
            }
        } catch { status = error.localizedDescription }
    }

    private static func send(_ frame: JSONObject, on ws: URLSessionWebSocketTask) async throws {
        var bytes = try jsonData(frame)
        guard bytes.count < 512 * 1024 else { throw BridgeFailure("size","数据帧过大。") }
        bytes.append(10); try await ws.send(.data(bytes))
    }

    private func run(_ ws: URLSessionWebSocketTask, epoch: UUID) async {
        do {
            guard let start = wire?.start else { return }
            try await Self.send(start, on: ws)
            while !Task.isCancelled {
                let message = try await ws.receive()
                guard epoch == generation else { return }
                let data: Data
                switch message {
                case .data(let bytes): data = bytes
                case .string(let text): data = Data(text.utf8)
                @unknown default: throw BridgeFailure("wire","不支持的中继数据帧。")
                }
                let frame = try object(JSONSerialization.jsonObject(with: data))
                let reply = try wire?.receive(frame, bind: bind)
                guard let current = wire else { return }
                if current.phase == .online { clientID = current.clientID; connecting = false; status = "已连接" }
                sending = current.awaitingReceipt; lines = current.lines
                if !sending, let submission = lastSubmission,
                   let row = lines.first(where: { $0.id == submission.id }) {
                    receiptDeadline?.cancel(); receiptDeadline = nil
                    let confirmed = row.state == "已入队" || row.state == "已投递"
                    try CommandJournal.settle(submission.id, state: confirmed ? .confirmed : .rejected)
                    lastSubmission = nil
                    if confirmed && draft == submission.text { draft = "" }
                    uncertain = CommandJournal.inputs().filter { $0.operation == "relay.publish" }
                }
                if let reply { try await Self.send(reply, on: ws) }
            }
        } catch {
            guard epoch == generation else { return }
            if let failure = error as? BridgeFailure {
                identityChanged = failure.code == "changed_identity"
                changedRelay = identityChanged ? wire?.relay : nil
                mayRegister = failure.code == "relay_rejected" && wire?.phase == .resolving
            }
            end(error.localizedDescription)
        }
    }

    func publish() {
        guard let ws = socket, wire?.phase == .online, !sending else { return }
        do {
            // Validate and reserve the unknown receipt before sending the mutation.
            guard var next = wire else { return }
            let frame = try next.publish(draft)
            guard let command = frame["command"] as? JSONObject, let id = command["message_id"] as? String else { return }
            try CommandJournal.reserve(session:next.clientID,id:id,operation:"relay.publish",connection:next.clientID,target:"next",text:draft)
            wire = next; lastSubmission = (id,draft); sending = true; lines = next.lines
            let epoch = generation
            receiptDeadline = Task { [weak self] in
                try? await Task.sleep(nanoseconds: 25_000_000_000)
                guard !Task.isCancelled, let self, self.generation == epoch, self.lastSubmission?.id == id else { return }
                self.end("发送回执超时，结果待确认。")
            }
            Task { [weak self] in
                guard let self, self.generation == epoch else { return }
                do { try await Self.send(frame, on: ws) }
                catch { if self.generation == epoch { self.end("发送中断，结果待确认。") } }
            }
        } catch { status = error.localizedDescription }
    }

    func end(_ message: String) {
        generation = UUID(); loop?.cancel(); heartbeat?.cancel(); receiptDeadline?.cancel()
        socket?.cancel(with: .goingAway, reason: nil); transport?.session.invalidateAndCancel()
        if wire != nil { wire?.lostReceipt(); lines = wire?.lines ?? [] }
        wire = nil; socket = nil; transport = nil; loop = nil; heartbeat = nil; receiptDeadline = nil
        clientID = ""; connecting = false; sending = false; status = message; lastSubmission = nil
        uncertain = CommandJournal.inputs().filter { $0.operation == "relay.publish" }
    }

    func forgetChangedIdentity(username: String, server: String) {
        guard socket == nil, identityChanged else { return }
        do {
            let selected = try DeviceRelay(username: username, server: server)
            guard changedRelay?.principal == selected.principal else { throw BridgeFailure("changed_identity", "请重新连接当前选择的账号以核对身份。") }
            try RelayControllerPin.forget(selected)
            changedRelay = nil
            identityChanged = false; status = "旧身份绑定已清除。再次连接时将验证当前账号。"
        } catch { status = error.localizedDescription }
    }

    func registrationURL(username: String, server: String) async throws -> URL {
        let relay = try DeviceRelay(username:username, server:server)
        let transport = RelayTransport(); defer { transport.session.invalidateAndCancel() }
        var request = URLRequest(url:relay.origin.appendingPathComponent("api/client/registration/start"),timeoutInterval:25)
        request.httpMethod = "POST"; request.setValue("application/json",forHTTPHeaderField:"Content-Type")
        request.httpBody = try jsonData(["name":"ssmcp","username":relay.username])
        let (stream, response) = try await transport.session.bytes(for:request)
        guard let http = response as? HTTPURLResponse, (200...299).contains(http.statusCode) else {
            throw BridgeFailure("registration","中继未接受注册请求。")
        }
        var data = Data()
        for try await byte in stream {
            guard data.count < 16 * 1024 else { throw BridgeFailure("size","注册回执超过上限。") }
            data.append(byte)
        }
        let receipt = try object(JSONSerialization.jsonObject(with:data))
        guard let code = receipt["user_code"] as? String, (1...64).contains(code.utf8.count),
              code.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45 }) else {
            throw BridgeFailure("registration","无法验证中继返回的注册码。")
        }
        var url = URLComponents(url:relay.origin.appendingPathComponent("setup"),resolvingAgainstBaseURL:false)!
        url.queryItems = [URLQueryItem(name:"code",value:code)]; return url.url!
    }
}

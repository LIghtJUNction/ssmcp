import Foundation
import Security

enum PairingStore {
    private static let service = "dev.ssmcp.mobile.pairing.v1"
    static func save(origin: String, token: String) throws {
        let data = try jsonData(["origin": origin, "token": token])
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: "gateway"]
        let update: [String: Any] = [kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly]
        let status = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var item = query; update.forEach { item[$0.key] = $0.value }
            try require(SecItemAdd(item as CFDictionary, nil) == errSecSuccess, "无法保存配对信息")
        } else { try require(status == errSecSuccess, "无法更新配对信息") }
    }
    static func load() throws -> (String, String)? {
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: "gateway",
            kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitOne]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecItemNotFound { return nil }
        try require(status == errSecSuccess, "配对信息暂不可用")
        guard let data = item as? Data else { throw BridgeFailure("keychain", "配对数据损坏") }
        let raw = try object(JSONSerialization.jsonObject(with: data))
        return (try string(raw["origin"]), try string(raw["token"]))
    }
    static func delete() throws {
        let status = SecItemDelete([kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service, kSecAttrAccount as String: "gateway"] as CFDictionary)
        try require(status == errSecSuccess || status == errSecItemNotFound, "无法删除配对信息")
    }
}

enum CommandJournal {
    private static let key = "ssmcp.command.receipts.v1"
    private static func url() throws -> URL {
        let root = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true).appendingPathComponent("SSMCP", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root.appendingPathComponent("command-receipts.json")
    }
    static func state(_ entry: [String: String]) -> String {
        entry["state"] ?? (entry["outcome"] == "收到回执" ? "confirmed" : "unknown")
    }
    static func insert(_ entries: [[String: String]], value: [String: String], capacity: Int = 20) throws -> [[String: String]] {
        try require(!entries.contains(where: { $0["id"] == value["id"] }), "Request ID was already reserved")
        var next = entries
        while next.count >= capacity {
            guard let index = next.firstIndex(where: { ["confirmed", "rejected"].contains(state($0)) }) else {
                throw BridgeFailure("receipt_capacity", "未确定结果已达到存储上限；请核对这些请求后再提交。")
            }
            next.remove(at: index)
        }
        next.append(value)
        return next
    }
    private static func load() throws -> [[String: String]] {
        let file = try url()
        guard FileManager.default.fileExists(atPath: file.path) else {
            return UserDefaults.standard.array(forKey: key) as? [[String: String]] ?? []
        }
        let data = try Data(contentsOf: file)
        try require(data.count <= 512 * 1024, "Receipt store exceeds its size limit")
        guard let entries = try JSONSerialization.jsonObject(with: data) as? [[String: String]], entries.count <= 20,
              entries.allSatisfy({ $0["id"] != nil }) else { throw BridgeFailure("receipt_store", "请求记录无法读取；未发送新请求。") }
        return entries
    }
    private static func save(_ entries: [[String: String]]) throws {
        let data = try JSONSerialization.data(withJSONObject: entries, options: [.sortedKeys])
        try require(data.count <= 512 * 1024, "Receipt store exceeds its size limit")
        // Persist the unknown marker atomically before a side effect is dispatched.
        try data.write(to: url(), options: [.atomic, .completeFileProtection])
        UserDefaults.standard.removeObject(forKey: key)
    }
    static func reserve(session: String, id: String, operation: String, generation: String = "",
                        connection: String = "", target: String = "", text: String? = nil) throws {
        var value = ["id": id, "session": session, "operation": operation, "generation": generation,
            "connection_id": connection, "target": target, "state": "unknown"]
        if let text {
            try require(text.utf8.count <= 16 * 1024, "Input exceeds its byte limit")
            value["text_base64"] = Data(text.utf8).base64EncodedString()
        }
        try save(insert(load(), value: value))
    }
    static func settle(_ id: String, state: CommandState) throws {
        try require(state != .notSent, "An unsent request has no remote receipt")
        var entries = try load()
        guard let index = entries.firstIndex(where: { $0["id"] == id }) else { throw BridgeFailure("receipt_store", "原始请求记录缺失") }
        entries[index]["state"] = state.rawValue
        if state == .confirmed || state == .rejected { entries[index].removeValue(forKey: "text_base64") }
        try save(entries)
    }
    static func inputs() -> [UnconfirmedInput] {
        guard let entries = try? load() else { return [] }
        return entries.compactMap { entry in
            guard state(entry) == "unknown", let id = entry["id"], let encoded = entry["text_base64"],
                  let data = Data(base64Encoded: encoded), data.count <= 16 * 1024,
                  let text = String(data: data, encoding: .utf8) else { return nil }
            return UnconfirmedInput(id: id, operation: entry["operation"] ?? "", connectionID: entry["connection_id"] ?? "",
                generation: entry["generation"] ?? "", target: entry["target"] ?? "", text: text)
        }
    }
    static func summaries() -> [String] {
        do { return try load().map {
            "历史请求 · \($0["session"] ?? "") · \($0["operation"] ?? $0["command"] ?? "")\n\($0["id"] ?? "") · connection \($0["connection_id"] ?? "") · \(state($0))"
        } } catch { return ["请求记录无法读取；新操作将被阻止，避免丢失未确定结果。"] }
    }
}

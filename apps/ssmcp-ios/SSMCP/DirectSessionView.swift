import SwiftUI

@MainActor struct DirectSessionView: View {
    @ObservedObject var relay: IOSRelaySession
    @ObservedObject var advanced: ClientModel
    @AppStorage("ssmcp.ios.relay.username") private var username = ""
    @AppStorage("ssmcp.ios.relay.server") private var server = "ssmcp.lmm.best"
    @Environment(\.openURL) private var openURL
    @State private var advancedOpen = false
    @State private var confirmReset = false
    @State private var registrationError: String?
    @State private var registering = false

    var body: some View {
        NavigationStack {
            Form {
                if relay.clientID.isEmpty {
                    Section {
                        TextField("用户名", text:$username)
                            .textInputAutocapitalization(.never).autocorrectionDisabled().textContentType(.username)
                        TextField("服务器地址", text:$server)
                            .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                    }.disabled(relay.connecting || registering)
                    Section {
                        if relay.connecting {
                            HStack { ProgressView(); Text("正在连接") }
                            Button("取消") { relay.end("已取消连接。") }
                        } else {
                            Button("连接") { relay.connect(username:username, server:server) }
                                .disabled(username.isEmpty || server.isEmpty || registering)
                        }
                    } footer: {
                        if (try? DeviceRelay(username:"ios-client",server:server).isHosted) == true {
                            Text("点击连接即表示同意[用户协议](https://ssmcp.lmm.best/legal/terms)和[隐私政策](https://ssmcp.lmm.best/legal/privacy)。")
                        }
                    }
                    if relay.mayRegister {
                        Button("注册此用户名") {
                            let name = username, address = server
                            registering = true
                            Task {
                                defer { registering = false }
                                do { let url = try await relay.registrationURL(username:name, server:address); openURL(url) }
                                catch { registrationError = error.localizedDescription }
                            }
                        }.disabled(registering)
                    }
                    if relay.identityChanged {
                        Button("检查身份变更", role:.destructive) { confirmReset = true }
                    }
                } else {
                    Section("当前会话") {
                        Text(verbatim:relay.clientID).font(.caption.monospaced()).textSelection(.enabled)
                        Button("断开并结束此会话", role:.destructive) { relay.end("此会话已结束。") }
                    }
                    Section("发送消息") {
                        TextEditor(text:$relay.draft).frame(minHeight:70).accessibilityLabel("消息正文")
                        Button(relay.sending ? "等待中继确认" : "发送") { relay.publish() }
                            .disabled(relay.sending || relay.draft.trimmingCharacters(in:.whitespacesAndNewlines).isEmpty)
                    }
                }
                if !relay.status.isEmpty { Text(verbatim:relay.status).font(.caption).foregroundStyle(.secondary) }
                if !relay.lines.isEmpty {
                    Section("对话") {
                        ForEach(relay.lines.reversed()) { line in
                            VStack(alignment:.leading,spacing:5) {
                                Text(verbatim:line.author + " · " + line.state).font(.caption).foregroundStyle(.secondary)
                                Text(verbatim:line.text).textSelection(.enabled)
                            }
                        }
                    }
                }
                if !relay.uncertain.isEmpty {
                    DisclosureGroup("待核对的发送记录") {
                        ForEach(relay.uncertain) { input in
                            VStack(alignment:.leading,spacing:5) {
                                Text(verbatim:input.id).font(.caption.monospaced())
                                Text(verbatim:input.text)
                            }.textSelection(.enabled)
                        }
                    }
                }
            }
            .navigationTitle("SSMCP")
            .toolbar {
                if relay.clientID.isEmpty && !relay.connecting {
                    Button("高级") { advancedOpen = true }.disabled(registering)
                }
            }
            .sheet(isPresented:$advancedOpen) { ContentView(model:advanced) }
            .confirmationDialog("清除此用户名的旧 GitHub 身份绑定？", isPresented:$confirmReset) {
                Button("清除旧绑定", role:.destructive) { relay.forgetChangedIdentity(username:username,server:server) }
            } message: { Text("请先与中继管理员核对账号转移。下次连接将记住中继确认的新身份。") }
            .alert("注册失败", isPresented:Binding(get:{registrationError != nil},set:{if !$0 {registrationError=nil}})) {
                Button("好") { registrationError = nil }
            } message: { Text(registrationError ?? "") }
        }
    }
}

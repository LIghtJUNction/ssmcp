import SwiftUI

@MainActor struct ContentView: View {
    @ObservedObject var model: ClientModel
    @State private var controlSession: SessionSummary?
    @State private var selectedTerminal = ""
    @State private var showingTimeline = false
    @State private var terminalDraft = ""
    @State private var chatDraft = ""
    @State private var rows = "24"
    @State private var cols = "80"
    @State private var workerTotal = "3"
    @State private var inputError: String?
    @State private var unknownInputs: [UnconfirmedInput] = CommandJournal.inputs()
    @State private var inspectingInput: UnconfirmedInput?

    var body: some View {
        TabView {
            navigation("SSMCP") { sessions }.tabItem { Label("会话", systemImage: "square.stack") }
            navigation("对话") {
                chat.toolbar { Button { showingTimeline = true } label: { Label("活动", systemImage: "list.bullet") } }
            }.tabItem { Label("对话", systemImage: "bubble.left") }
            navigation("终端") { terminals }.tabItem { Label("终端", systemImage: "terminal") }
            navigation("Workers") { workers }.tabItem { Label("Workers", systemImage: "square.grid.2x2") }
            navigation("等待与计时") { waits }.tabItem { Label("等待", systemImage: "clock") }
        }
        .tint(Color(red: 0.20, green: 0.36, blue: 0.28))
        .alert("申请独占控制权限？", isPresented: Binding(get: { controlSession != nil }, set: { if !$0 { controlSession = nil } })) {
            Button("取消", role: .cancel) { controlSession = nil }
            Button("申请控制") {
                if let session = controlSession { Task { await model.open(session.name) } }
                controlSession = nil
            }
        } message: { Text("终端输入和调整容量仍需每次明确提交。后台会话不会因关闭界面而停止。") }
        .sheet(isPresented: $showingTimeline) {
            navigation("活动") {
                timeline.toolbar { Button("关闭") { showingTimeline = false } }
            }
        }
        .sheet(item: $inspectingInput) { input in
            NavigationStack {
                ScrollView {
                    VStack(alignment: .leading, spacing: 20) {
                        Text(verbatim: "\(input.id)\n\(input.connectionID) · \(input.generation)\n目标 \(input.target)").font(.caption)
                        Text(verbatim: input.text).font(.body)
                    }.textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).padding(24)
                }.navigationTitle("原始输入 · 待确认")
                    .toolbar { Button("关闭") { inspectingInput = nil } }
            }
        }
        .alert("检查输入", isPresented: Binding(get: { inputError != nil }, set: { if !$0 { inputError = nil } })) {
            Button("好") { inputError = nil }
        } message: { Text(inputError ?? "") }
    }

    private func navigation<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        NavigationStack {
            content()
                .navigationTitle(title)
                .safeAreaInset(edge: .top) {
                    HStack(alignment: .top, spacing: 12) {
                        if model.busy { ProgressView().controlSize(.small) }
                        Text(verbatim: model.status).font(.caption).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                    }.padding(.horizontal, 24).padding(.vertical, 12).background(.background)
                }
        }
    }

    private var sessions: some View {
        Form {
            if let current = model.connection {
                Section("当前会话") {
                    Text(verbatim: current.session).font(.headline)
                    Text(verbatim: "\(current.capabilities.mode) · \(current.capabilities.generation)").font(.caption).textSelection(.enabled)
                    Button("刷新状态") { Task { await model.refresh() } }.disabled(model.busy)
                    Button("断开连接 · 保持会话运行") { Task { await model.disconnect() } }.disabled(model.busy)
                }
            } else {
                Section("连接到 CLI core") {
                    TextField("https://your-gateway.example", text: $model.origin)
                        .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                    SecureField("配对令牌", text: $model.token).textInputAutocapitalization(.never).autocorrectionDisabled()
                    Toggle("只观察", isOn: $model.readOnly)
                    Button("保存配对并读取会话") { Task { await model.configureAndList() } }.disabled(model.busy)
                    Button("删除保存的配对", role: .destructive) { model.deletePairing() }.disabled(model.busy)
                }
            }
            if let current = model.connection, current.capabilities.can("client.update") {
                Section("SSMCP 更新") {
                    Button("检查更新") {
                        Task { await model.command("client.update", arguments: ["check": true], expectedGeneration: current.capabilities.generation, expectedConnectionID: current.id) }
                    }.disabled(model.busy)
                    Button("安装更新") {
                        Task { await model.command("client.update", arguments: ["check": false], expectedGeneration: current.capabilities.generation, expectedConnectionID: current.id) }
                    }.disabled(model.busy)
                    if let text = model.updateResult {
                        Text(verbatim: text).font(.caption).textSelection(.enabled)
                    }
                }
            }
            Section("会话") {
                if model.sessions.isEmpty { Text("连接后显示 core 中真实存在的会话。").foregroundStyle(.secondary) }
                ForEach(model.sessions) { session in
                    Button {
                        if model.readOnly { Task { await model.open(session.name) } }
                        else { controlSession = session }
                    } label: {
                        VStack(alignment: .leading, spacing: 8) {
                            Text(verbatim: session.name).font(.headline)
                            Text(verbatim: "\(session.phase) · \(session.managerState)").font(.caption).foregroundStyle(.secondary)
                            Text(verbatim: session.cwd).font(.caption).foregroundStyle(.secondary)
                        }.padding(.vertical, 8)
                    }.disabled(model.busy || model.connection != nil)
                }
                if model.nextSessionOffset != nil { Button("更多会话") { Task { await model.listSessions(more: true) } }.disabled(model.busy) }
            }
            Section {
                Text("使用 HTTPS 与系统证书校验。配对令牌保存在此设备的 Keychain。退到后台只取消观察，不取消任务。").font(.footnote).foregroundStyle(.secondary)
            }
        }
    }

    private var timeline: some View {
        List {
            if !unknownInputs.isEmpty {
                Section("原始输入 · 待确认") { unconfirmed("chat.send"); unconfirmed("terminal.write") }
            }
            if model.timeline.isEmpty { Text("连接后显示真实会话与 worker 事件。").foregroundStyle(.secondary) }
            ForEach(model.timeline.reversed()) { line in
                Text(verbatim: line.text).font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled).padding(.vertical, 12).listRowSeparator(.hidden)
            }
        }.listStyle(.plain)
    }

    private var chat: some View {
        Form {
            if model.connection?.capabilities.resources.contains("conversation") != true {
                Section { Text("当前 core 未提供对话资源。更新 core 后重新读取能力。").foregroundStyle(.secondary) }
            } else {
                Section("线程与目标") {
                    Picker("线程", selection: Binding(get: { model.chatTarget }, set: { model.selectThread($0) })) {
                        Text("共享会话 · 下一位接收者").tag("next")
                        ForEach(model.threads) { thread in Text(verbatim: "\(thread.name) · \(thread.status)").tag(thread.id) }
                        if model.chatTarget != "next" && !model.threads.contains(where: { $0.id == model.chatTarget }) {
                            Text("目标当前不可用，请明确重新选择").tag(model.chatTarget)
                        }
                    }
                    Button(model.chatHasMore ? "读取下一页" : "刷新对话") { model.refreshConversation() }.disabled(model.busy)
                }
                Section("保留的真实记录") {
                    if model.conversation.isEmpty { Text("当前范围暂无记录。").foregroundStyle(.secondary) }
                    ForEach(model.conversation) { line in Text(verbatim: line.text).font(.callout).textSelection(.enabled).padding(.vertical, 8) }
                }
                Section("消息队列") {
                    if model.queue.isEmpty { Text("暂无队列记录。").foregroundStyle(.secondary) }
                    ForEach(model.queue) { line in Text(verbatim: line.text).font(.caption).textSelection(.enabled) }
                }
                Section("写给这个会话") {
                    TextEditor(text: $chatDraft).frame(minHeight: 120).accessibilityLabel("消息正文")
                    Button("提交消息") {
                        guard !chatDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, chatDraft.utf8.count <= 16 * 1024 else {
                            inputError = "消息应为 1–16384 字节 UTF-8 文本。"; return
                        }
                        guard let original = model.connection else { return }
                        let generation = original.capabilities.generation
                        let text = chatDraft, target = model.chatTarget
                        Task {
                            let result = await model.command("chat.send", arguments: ["text": text, "target": target], expectedGeneration: generation, expectedConnectionID: original.id)
                            if result.mayClear(current: model.connection, originalDraft: text, currentDraft: chatDraft, currentTarget: model.chatTarget) { chatDraft = "" }
                            if result.state == .unknown { remember(result, operation: "chat.send", text: text) }
                        }
                    }.disabled(model.busy || !model.chatCanSend || model.connection?.capabilities.can("chat.send") != true ||
                        (model.chatTarget != "next" && !model.threads.contains(where: { $0.id == model.chatTarget })))
                    unconfirmed("chat.send")
                    Text("回执仅确认进入 CLI 本地队列，不表示模型已经读取或完成。不会自动启动模型或重发结果未知的消息。").font(.footnote).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var terminals: some View {
        Form {
            if model.connection?.capabilities.resources.contains("terminals") != true {
                Section { Text("当前连接没有终端资源。").foregroundStyle(.secondary) }
            } else {
                Section("真实终端") {
                    Button("刷新终端") { Task { await model.refresh() } }.disabled(model.busy)
                    Picker("终端", selection: $selectedTerminal) {
                        Text("选择终端").tag("")
                        ForEach(model.terminals) { terminal in Text(verbatim: "\(terminal.name) · \(terminal.id)").tag(terminal.id) }
                    }
                    ForEach(model.terminals) { terminal in
                        Text(verbatim: "\(terminal.id) · \(terminal.cwd)").font(.caption).foregroundStyle(.secondary)
                    }
                }
                Section("输出 · 最近 32 KiB") {
                    ScrollView([.horizontal, .vertical]) {
                        Text(verbatim: model.terminalOutput[selectedTerminal] ?? "等待所选终端的真实输出…")
                            .font(.system(.caption, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    }.frame(minHeight: 160, maxHeight: 320)
                    Text("显示 UTF-8 文本，不模拟终端控制序列。").font(.caption).foregroundStyle(.secondary)
                }
                Section("明确输入") {
                    TextEditor(text: $terminalDraft).frame(minHeight: 100).accessibilityLabel("终端输入，不自动添加换行")
                    Button("发送输入 · 不自动追加换行") {
                        guard !terminalDraft.isEmpty, terminalDraft.utf8.count <= 16 * 1024 else { inputError = "输入上限为 16 KiB。"; return }
                        guard let original = model.connection else { return }
                        let generation = original.capabilities.generation
                        let text = terminalDraft, terminal = selectedTerminal
                        Task {
                            let result = await model.command("terminal.write", arguments: ["terminal_id": terminal, "data": text], expectedGeneration: generation, expectedConnectionID: original.id)
                            if result.mayClear(current: model.connection, originalDraft: text, currentDraft: terminalDraft, currentTarget: selectedTerminal) { terminalDraft = "" }
                            if result.state == .unknown { remember(result, operation: "terminal.write", text: text) }
                        }
                    }.disabled(model.busy || !model.terminals.contains(where: { $0.id == selectedTerminal }) || model.connection?.capabilities.can("terminal.write") != true)
                }
                Section { unconfirmed("terminal.write") }
                Section("调整当前全部终端") {
                    TextField("行数", text: $rows).keyboardType(.numberPad)
                    TextField("列数", text: $cols).keyboardType(.numberPad)
                    Button("应用尺寸") {
                        guard let row = Int(rows), let col = Int(cols), (1...1000).contains(row), (1...1000).contains(col) else { inputError = "行和列都应在 1–1000 之间。"; return }
                        guard let generation = model.connection?.capabilities.generation else { return }
                        Task { await model.command("terminal.resize", arguments: ["rows": row, "cols": col], expectedGeneration: generation) }
                    }.disabled(model.busy || model.connection?.capabilities.can("terminal.resize") != true)
                }
            }
        }
    }

    private var waits: some View {
        Form {
            if let snapshot = model.timeSnapshot {
                Section {
                    Button("刷新状态") { Task { await model.refresh() } }.disabled(model.busy)
                }
                waitRows("进行中", snapshot["active"])
                waitRows("保留的结果", snapshot["recent"])
                waitRows("定时提示计划", snapshot["prompts"])
            } else {
                Text("当前连接尚未提供等待与计时快照。").foregroundStyle(.secondary)
            }
        }
    }

    private func waitRows(_ heading: String, _ value: Any?) -> some View {
        let entries = Array(((value as? [JSONObject]) ?? []).prefix(256))
        return Section(heading) {
            if entries.isEmpty {
                Text(value == nil ? "当前连接未提供此项。" : "当前快照没有记录。")
                    .foregroundStyle(.secondary)
            }
            ForEach(entries.indices, id: \.self) { index in
                let entry = entries[index]
                let durable = entry["durable"] as? JSONObject
                let title = entry["label"] as? String ?? entry["id"] as? String ?? "等待"
                VStack(alignment: .leading, spacing: 8) {
                    Text(verbatim: title).font(.headline)
                    if let status = entry["status"] as? String {
                        LabeledContent("状态", value: status)
                    }
                    if let result = durable?["result_status"] as? String {
                        LabeledContent("结果", value: result)
                    }
                    if let state = durable?["state"] as? String {
                        LabeledContent("读取状态", value: state)
                    }
                }
            }
        }
    }

    private var workers: some View {
        Form {
            if model.connection?.capabilities.resources.contains("workers") != true {
                Section { Text("当前连接没有 worker 池。").foregroundStyle(.secondary) }
            } else {
                Section("状态") {
                    Text(verbatim: model.workerSummary)
                    Button("刷新状态") { Task { await model.refresh() } }.disabled(model.busy)
                }
                if !model.workerProductivity.isEmpty {
                    Section("Worker 工具调用 / 分钟（对数）") {
                        Text(verbatim: model.workerProductivity.joined(separator: "\n"))
                            .font(.system(.caption, design: .monospaced))
                            .accessibilityLabel("最近十分钟 Worker 工具调用趋势")
                    }
                }
                Section("站位") {
                    ForEach(model.workerRows) { row in
                        VStack(alignment: .leading, spacing: 12) {
                            Text(verbatim: row.title).font(.headline)
                            Text(verbatim: row.details).font(.caption).textSelection(.enabled)
                        }.padding(.vertical, 8)
                    }
                    if let offset = model.nextWorkerOffset { Button("下一页站位") { Task { await model.refresh(workerOffset: offset) } }.disabled(model.busy) }
                }
                Section("目标容量") {
                    TextField("0–128", text: $workerTotal).keyboardType(.numberPad)
                    Button("提交目标容量") {
                        guard let count = Int(workerTotal), (0...128).contains(count), let pool = model.poolGeneration,
                              let generation = model.connection?.capabilities.generation else { inputError = "需要当前池代次及 0–128 的目标容量。"; return }
                        Task { await model.command("workers.resize", arguments: ["pool_generation": pool, "total": count], expectedGeneration: generation) }
                    }.disabled(model.busy || model.connection?.capabilities.can("workers.resize") != true)
                    Text("回执表示 core 收到请求。正在执行的 worker 通过原有握手完成退出；resize_pending 仍可能为真。").font(.footnote).foregroundStyle(.secondary)
                }
            }
        }
    }
    private func remember(_ completion: CommandCompletion, operation: String, text: String) {
        guard !unknownInputs.contains(where: { $0.id == completion.requestID }) else { return }
        unknownInputs.append(UnconfirmedInput(id: completion.requestID, operation: operation,
            connectionID: completion.connectionID, generation: completion.generation, target: completion.target, text: text))
    }
    @ViewBuilder private func unconfirmed(_ operation: String) -> some View {
        ForEach(unknownInputs.filter { $0.operation == operation }) { input in
            Button { inspectingInput = input } label: {
                VStack(alignment: .leading, spacing: 8) {
                    Text("结果待确认 · 查看原始输入")
                    Text(verbatim: input.id).font(.caption)
                }
            }
        }
    }

}

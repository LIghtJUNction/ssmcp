package dev.ssmcp.mobile

import android.app.Activity
import android.app.AlertDialog
import android.graphics.Color
import android.graphics.Typeface
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager
import android.widget.*
import org.json.JSONObject

/** Platform Views only. Host content is always plain text, never HTML or executable markup. */
class MainActivity : Activity() {
    private lateinit var controller: ClientController
    private lateinit var store: TokenStore
    private lateinit var status: TextView
    private lateinit var content: FrameLayout
    private var tab = "Sessions"
    private var originDraft = ""
    private var tokenDraft = ""
    private var readOnly = true
    private var terminalDraft = ""
    private var selectedTerminal = ""
    private var rowsDraft = "24"
    private var colsDraft = "80"
    private var workersDraft = "3"
    private var rendering = false
    private var lastRenderKey = ""
    private var chatDraft = ""
    private val unknownInputs = ArrayList<UnconfirmedInput>()
    private var chatLog: TextView? = null
    private var queueLog: TextView? = null
    private val controls = ArrayList<Pair<Button, Boolean>>()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.setFlags(WindowManager.LayoutParams.FLAG_SECURE, WindowManager.LayoutParams.FLAG_SECURE)
        store = TokenStore(this)
        val journal = CommandJournal(this)
        unknownInputs.addAll(journal.inputs())
        savedInstanceState?.let {
            chatDraft = it.getString("chatDraft", ""); terminalDraft = it.getString("terminalDraft", "")
            selectedTerminal = it.getString("selectedTerminal", ""); tab = it.getString("tab", "Sessions")
        }
        controller = ClientController(journal) {
            renderStatus()
            if (tab == "Activity" || !rendering || viewKey() != lastRenderKey) renderBody()
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(24), dp(24), dp(8))
            setBackgroundColor(Color.rgb(250, 250, 247))
            // Android 15 edge-to-edge: retain the large layout padding outside system insets.
            setOnApplyWindowInsetsListener { view, insets ->
                view.setPadding(dp(24) + insets.systemWindowInsetLeft, dp(16) + insets.systemWindowInsetTop,
                    dp(24) + insets.systemWindowInsetRight, dp(8) + insets.systemWindowInsetBottom)
                insets
            }
        }
        root.addView(label("SSMCP", 30, true))
        status = label("Disconnected", 13).apply { setPadding(0, dp(8), 0, dp(20)) }
        root.addView(status)
        val navigation = LinearLayout(this)
        listOf("Sessions", "对话", "Activity", "Terminals", "Workers").forEach { name ->
            navigation.addView(Button(this).apply {
                text = name; textSize = 11f; isAllCaps = false; minWidth = 0
                setPadding(dp(2), 0, dp(2), 0)
                setOnClickListener { tab = name; renderBody() }
            }, LinearLayout.LayoutParams(0, dp(48), 1f))
        }
        root.addView(navigation)
        content = FrameLayout(this)
        root.addView(content, LinearLayout.LayoutParams(-1, 0, 1f))
        setContentView(root)
        renderBody()
        try { store.load()?.let { (origin, token) ->
            originDraft = origin; tokenDraft = token
            controller.configure(Gateway(origin, token))
        } } catch (_: Exception) { showError("Stored pairing is unavailable. Delete it and pair again.") }
    }

    override fun onSaveInstanceState(outState: Bundle) {
        outState.putString("chatDraft", chatDraft); outState.putString("terminalDraft", terminalDraft)
        outState.putString("selectedTerminal", selectedTerminal); outState.putString("tab", tab)
        super.onSaveInstanceState(outState)
    }
    override fun onStart() { super.onStart(); controller.foreground(true) }
    override fun onStop() { controller.foreground(false); super.onStop() }
    override fun onDestroy() { controller.destroy(); tokenDraft = ""; super.onDestroy() }
    private fun dp(value: Int) = (resources.displayMetrics.density * value).toInt()
    private fun label(text: String, size: Int = 15, bold: Boolean = false) = TextView(this).apply {
        this.text = text; textSize = size.toFloat(); setTextColor(Color.rgb(33, 39, 35))
        if (bold) setTypeface(typeface, Typeface.BOLD)
        setTextIsSelectable(true)
        setPadding(0, dp(8), 0, dp(8))
    }
    private fun button(text: String, enabled: Boolean = true, action: () -> Unit) = Button(this).apply {
        this.text = text; isAllCaps = false; isEnabled = enabled && !controller.busy
        controls.add(this to enabled)
        setOnClickListener { if (!controller.busy) try { action() } catch (e: Exception) { showError(e.message ?: "Invalid input") } }
    }
    private fun field(hint: String, value: String, secret: Boolean = false, numeric: Boolean = false,
                      changed: (String) -> Unit): EditText = EditText(this).apply {
        this.hint = hint; setText(value); textSize = 15f; isSaveEnabled = !secret
        inputType = if (secret) InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
            else if (numeric) InputType.TYPE_CLASS_NUMBER
            else InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        setPadding(0, dp(10), 0, dp(10))
        addTextChangedListener(object : android.text.TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) { changed(s.toString()) }
            override fun afterTextChanged(s: android.text.Editable?) {}
        })
    }
    private fun renderStatus() {
        if (::status.isInitialized) status.text = controller.status
        controls.forEach { (button, granted) -> button.isEnabled = granted && !controller.busy }
        chatLog?.text = controller.conversation.joinToString("\n\n").ifEmpty { "当前范围暂无保留的对话记录。" }
        queueLog?.text = queueText()
    }
    private fun panel(): LinearLayout {
        val view = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(0, dp(16), 0, dp(24)) }
        content.addView(ScrollView(this).apply { isFillViewport = true; addView(view) })
        return view
    }
    private fun viewKey(): String {
        val current = controller.connection
        val state = when (tab) {
            "Terminals" -> System.identityHashCode(controller.terminals).toString()
            "Workers" -> System.identityHashCode(controller.workers).toString()
            "对话" -> "${controller.threads?.toString()}|${controller.chatTarget}"
            else -> ""
        }
        return "$tab|${current?.id}|${current?.capabilities?.generation}|${current?.capabilities?.mode}|$state"
    }
    private fun renderBody() {
        if (!::content.isInitialized) return
        // Avoid replacing editable controls during background event delivery.
        lastRenderKey = viewKey()
        controls.clear(); chatLog = null; queueLog = null
        content.removeAllViews()
        when (tab) {
            "Sessions" -> sessions(panel())
            "Terminals" -> terminals(panel())
            "Workers" -> workers(panel())
            "对话" -> chat(panel())
            else -> activity()
        }
    }
    private fun sessions(view: LinearLayout) {
        rendering = false
        view.addView(label("Connect to your CLI", 22, true))
        if (controller.connection == null) {
            view.addView(field("https://your-gateway.example", originDraft) { originDraft = it })
            view.addView(field("Pairing token", tokenDraft, secret = true) { tokenDraft = it })
            view.addView(Switch(this).apply {
                text = "Observe only"; isChecked = readOnly; setPadding(0, dp(16), 0, dp(16))
                setOnCheckedChangeListener { _, checked -> readOnly = checked }
            })
            view.addView(button("Save pairing · list sessions") {
                val api = Gateway(originDraft, tokenDraft)
                store.save(api.origin, tokenDraft); originDraft = api.origin
                controller.configure(api)
            })
            view.addView(button("Delete saved pairing") {
                store.delete(); tokenDraft = ""; originDraft = ""; controller.clearPairing()
            })
        } else {
            view.addView(label(controller.connection!!.session, 20, true))
            view.addView(label("${controller.connection!!.capabilities.mode} · ${controller.connection!!.capabilities.generation}", 12))
            view.addView(button("Disconnect · keep session running") { controller.disconnect() })
            view.addView(button("Refresh current state") { controller.refresh() })
        }
        view.addView(label("Sessions", 18, true))
        if (controller.sessions.isEmpty()) view.addView(label("No sessions loaded. Connect to a configured HTTPS gateway."))
        controller.sessions.forEach { session ->
            view.addView(button("${session.name}\n${session.phase} · ${session.managerState}", controller.connection == null) {
                if (readOnly) controller.open(session.name, true)
                else AlertDialog.Builder(this).setTitle("Control ${session.name}?")
                    .setMessage("Request the exclusive input lease. Terminal input and resize require an explicit action.")
                    .setNegativeButton("Cancel", null).setPositiveButton("Request control") { _, _ -> controller.open(session.name, false) }.show()
            })
            view.addView(label(session.cwd, 12))
        }
        if (controller.nextSessionOffset != null) view.addView(button("More sessions") { controller.listSessions(true) })
        view.addView(label("HTTPS with normal certificate verification. The pairing token is stored with Android Keystore. Backgrounding stops observation; jobs keep running.", 12))
    }
    private fun activity() {
        rendering = false
        val items = controller.timeline.asReversed().toList()
        if (items.isEmpty()) { panel().addView(label("Session and worker events appear here after connecting.")); return }
        content.addView(ListView(this).apply {
            divider = null
            if (unknownInputs.isNotEmpty()) {
                val original = LinearLayout(this@MainActivity).apply { orientation = LinearLayout.VERTICAL }
                showUnconfirmed(original, "chat.send"); showUnconfirmed(original, "terminal.write")
                addHeaderView(original, null, false)
            }
            adapter = object : BaseAdapter() {
                override fun getCount() = items.size
                override fun getItem(position: Int) = items[position]
                override fun getItemId(position: Int) = position.toLong()
                override fun getView(position: Int, recycled: View?, parent: ViewGroup?): View =
                    ((recycled as? TextView) ?: label("", 13).apply { typeface = Typeface.MONOSPACE }).apply {
                        text = items[position]; setPadding(0, dp(16), 0, dp(16))
                    }
            }
        })
    }
    private fun terminals(view: LinearLayout) {
        rendering = true
        view.addView(label("Terminals", 22, true))
        val caps = controller.connection?.capabilities
        if (caps == null || "terminals" !in caps.resources) {
            view.addView(label("This connection has no terminal resource.")); return
        }
        view.addView(button("Refresh terminals") { rendering = false; controller.refresh() })
        val array = controller.terminals?.optJSONArray("terminals")
        val ids = ArrayList<String>()
        if (array != null) for (i in 0 until minOf(array.length(), 16)) {
            val terminal = array.getJSONObject(i)
            val id = terminal.string("terminal_id")
            ids.add(id)
            view.addView(label("${terminal.optString("name", id)} · $id\n${terminal.optString("cwd")}", 13))
        }
        if (ids.isEmpty()) { view.addView(label("No terminals are currently available.")); return }
        if (selectedTerminal !in ids) selectedTerminal = ids.first()
        view.addView(Spinner(this).apply {
            adapter = ArrayAdapter(this@MainActivity, android.R.layout.simple_spinner_dropdown_item, ids)
            setSelection(ids.indexOf(selectedTerminal))
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                override fun onNothingSelected(parent: AdapterView<*>?) {}
                override fun onItemSelected(parent: AdapterView<*>?, v: View?, position: Int, id: Long) { selectedTerminal = ids[position] }
            }
        })
        view.addView(label(controller.terminalOutput(selectedTerminal), 12).apply {
            typeface = Typeface.MONOSPACE; maxLines = 120
        })
        view.addView(button("Refresh output view") { rendering = false; renderBody() })
        view.addView(label("Plain UTF-8 output · last 32 KiB per terminal", 12))
        view.addView(field("Explicit terminal input (no newline added)", terminalDraft) { terminalDraft = it }.apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            minLines = 2; maxLines = 6
        })
        showUnconfirmed(view, "terminal.write")
        view.addView(button("Send input", caps.can("terminal.write")) {
            require(byteSize(terminalDraft) <= 16 * 1024) { "Input must be at most 16 KiB" }
            require(terminalDraft.isNotEmpty()) { "Enter terminal input" }
            val data = terminalDraft
            controller.command("terminal.write", JSONObject().put("terminal_id", selectedTerminal).put("data", data), caps.generation) { receipt ->
                if (receipt.mayClear(controller.connection, data, terminalDraft, selectedTerminal)) {
                    terminalDraft = ""
                } else if (receipt.state == CommandState.UNKNOWN) remember(receipt, "terminal.write", data)
                renderBody()
            }
        })
        view.addView(field("Rows (1–1000)", rowsDraft, numeric = true) { rowsDraft = it })
        view.addView(field("Columns (1–1000)", colsDraft, numeric = true) { colsDraft = it })
        view.addView(button("Resize all current terminals", caps.can("terminal.resize")) {
            val rows = rowsDraft.toInt(); val cols = colsDraft.toInt()
            require(rows in 1..1000 && cols in 1..1000)
            controller.command("terminal.resize", JSONObject().put("rows", rows).put("cols", cols), caps.generation)
        })
        if (caps.mode != "control") view.addView(label("Observe mode. Reconnect with control to request input permission.", 12))
    }
    private fun workers(view: LinearLayout) {
        rendering = true
        view.addView(label("Worker pool", 22, true))
        val caps = controller.connection?.capabilities
        if (caps == null || "workers" !in caps.resources) { view.addView(label("This connection has no worker pool resource.")); return }
        view.addView(button("Refresh worker state") { rendering = false; controller.refresh() })
        val state = controller.workers ?: return
        view.addView(label("${state.optInt("working")} working · ${state.optInt("idle")} standby · ${state.optInt("offline")} offline\n${state.optInt("registered_workers")} registered · target ${state.optInt("desired_total")} · ${state.optInt("retiring")} retiring"))
        val slots = state.optJSONArray("slots")
        if (slots != null) for (i in 0 until minOf(32, slots.length())) {
            val slot = slots.getJSONObject(i)
            view.addView(label("Station ${slot.get("slot")} · ${slot.optString("state")} · epoch ${slot.get("epoch")}", 16, true))
            val details = listOf("task_summary", "progress", "last_command", "reason").mapNotNull {
                if (slot.isNull(it)) null else slot.opt(it)?.toString()?.take(1024)
            }.joinToString("\n")
            if (details.isNotEmpty()) view.addView(label(details, 13))
        }
        if (!state.isNull("next_offset")) view.addView(button("Next stations") { rendering = false; controller.refresh(state.getInt("next_offset")) })
        view.addView(field("Target stations (0–128)", workersDraft, numeric = true) { workersDraft = it })
        view.addView(button("Set target capacity", caps.can("workers.resize")) {
            val total = workersDraft.toInt(); require(total in 0..128)
            controller.command("workers.resize", JSONObject().put("pool_generation", state.string("pool_generation")).put("total", total), caps.generation)
        })
        view.addView(label("Resize receipts confirm the request. Active work retires through the core handshake; it may still be pending.", 12))
    }
    private fun queueText(): String {
        val items = controller.queue?.optJSONArray("items") ?: return "暂无队列快照。"
        return (0 until items.length()).joinToString("\n\n") {
            val item = items.getJSONObject(it)
            val state = if (item.optBoolean("uncertain")) "unknown · 待确认，禁止自动重发" else item.optString("status")
            "${item.optString("id")} · ${item.optString("target")} · $state\n${item.optString("text").take(1024)}" +
                if (item.optBoolean("retarget_required")) "\n目标失效，需要明确重新选择。" else ""
        }.ifEmpty { "队列为空。" }
    }
    private fun chat(view: LinearLayout) {
        rendering = true
        view.addView(label("对话", 22, true))
        val caps = controller.connection?.capabilities
        if (caps == null || "conversation" !in caps.resources) {
            view.addView(label("当前 core 未提供对话资源。升级 core 后重新读取能力。")); return
        }
        val ids = arrayListOf("next")
        val names = arrayListOf("共享会话 · 交给下一位接收者")
        val agents = controller.threads?.optJSONArray("agents")
        if (agents != null) for (i in 0 until agents.length()) {
            val agent = agents.getJSONObject(i)
            ids.add(agent.string("agent_id")); names.add("${agent.optString("name")} · ${agent.optString("status")}")
        }
        val targetAvailable = controller.chatTarget in ids
        if (!targetAvailable) {
            ids.add(controller.chatTarget); names.add("目标当前不可用 · ${controller.chatTarget}")
        }
        view.addView(Spinner(this).apply {
            adapter = ArrayAdapter(this@MainActivity, android.R.layout.simple_spinner_dropdown_item, names)
            setSelection(ids.indexOf(controller.chatTarget).coerceAtLeast(0))
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                override fun onNothingSelected(parent: AdapterView<*>?) {}
                override fun onItemSelected(parent: AdapterView<*>?, v: View?, position: Int, id: Long) {
                    controller.selectThread(ids[position])
                }
            }
        })
        view.addView(button("刷新对话 / 读取下一页") { controller.refreshChat() })
        chatLog = label(controller.conversation.joinToString("\n\n").ifEmpty { "当前范围暂无保留的对话记录。" }, 14)
        view.addView(chatLog)
        view.addView(label("消息队列", 16, true))
        queueLog = label(queueText(), 12); view.addView(queueLog)
        view.addView(field("写给这个会话…", chatDraft) { chatDraft = it }.apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            minLines = 3; maxLines = 8
        })
        showUnconfirmed(view, "chat.send")
        view.addView(button("提交消息", targetAvailable && caps.can("chat.send") && controller.threads?.optBoolean("can_send") == true) {
            require(chatDraft.isNotBlank() && byteSize(chatDraft) <= 16 * 1024) { "消息应为 1–16384 字节 UTF-8 文本。" }
            val text = chatDraft
            controller.command("chat.send", JSONObject().put("text", text).put("target", controller.chatTarget), caps.generation) { receipt ->
                if (receipt.mayClear(controller.connection, text, chatDraft, controller.chatTarget)) {
                    chatDraft = ""
                } else if (receipt.state == CommandState.UNKNOWN) remember(receipt, "chat.send", text)
                renderBody()
            }
        })
        view.addView(label("提交回执仅确认进入 CLI 本地队列。它不表示模型已读取或完成。不会自动启动模型，也不会自动重发结果未知的消息。", 12))
    }
    private fun remember(result: CommandCompletion, operation: String, text: String) {
        if (unknownInputs.none { it.id == result.requestId }) unknownInputs.add(UnconfirmedInput(result.requestId,
            operation, result.connectionId, result.generation, result.target, text))
    }
    private fun showUnconfirmed(view: LinearLayout, operation: String) {
        unknownInputs.filter { it.operation == operation }.forEach { input ->
            view.addView(label("结果待确认 · ${input.id}。原始正文已保留。", 12))
            view.addView(button("查看原始输入") { showOriginal(input.id,
                "${input.connectionId} · ${input.generation}\n目标 ${input.target}\n\n${input.text}") })
        }
    }
    private fun showError(message: String) { AlertDialog.Builder(this).setTitle("SSMCP").setMessage(message).setPositiveButton("OK", null).show() }
    private fun showOriginal(id: String, text: String) {
        val body = label(text, 14)
        body.setPadding(dp(24), dp(16), dp(24), dp(16))
        AlertDialog.Builder(this).setTitle(id).setView(ScrollView(this).apply { addView(body) }).setPositiveButton("关闭", null).show()
    }
}

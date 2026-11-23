package dev.ssmcp.mobile

import android.os.Handler
import android.os.Looper
import android.util.Base64
import org.json.JSONObject
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors

/** Mutations have one independent lane. Read cancellation cannot cancel or replay them. */
class ClientController(private val journal: CommandJournal, private val makeBudget: () -> ReadBudget = { ReadBudget() },
                       private val changed: () -> Unit) {
    private val main = Handler(Looper.getMainLooper())
    private val work = Executors.newSingleThreadExecutor()
    private val observations = Executors.newSingleThreadExecutor()
    private var observer: Observation? = null
    private var readBudget: ReadBudget? = null
    private var inbox: EventInbox? = null
    private var pendingHistory: HistoryPage? = null
    private var foreground = false
    private var disposed = false
    private var epoch = 0L
    var gateway: GatewayAPI? = null; private set
    var connection: OpenConnection? = null; private set
    val cursor: Counter? get() = inbox?.cursor
    var sessions = emptyList<SessionSummary>(); private set
    var nextSessionOffset: Int? = null; private set
    var runtime: JSONObject? = null; private set
    var terminals: JSONObject? = null; private set
    var workers: JSONObject? = null; private set
    var threads: JSONObject? = null; private set
    var queue: JSONObject? = null; private set
    var chatTarget = "next"; private set
    var chatHasMore = false; private set
    private var chatCursor: Counter? = null
    val conversation = ArrayList<String>()
    var status = "Disconnected"; private set
    /** True only for lease/command mutations; closing can interrupt a read pass immediately. */
    var busy = false; private set
    val timeline = ArrayList<String>(journal.summaries())
    private val terminalBytes = linkedMapOf<String, ByteArray>()
    private data class HistoryPage(val generation: String, val target: String, val after: Counter?, val value: JSONObject)
    private data class Snapshot(val caps: Capabilities, val inbox: EventInbox?, val history: HistoryPage?,
        val runtime: JSONObject?, val terminals: JSONObject?, val workers: JSONObject?, val threads: JSONObject?, val queue: JSONObject?)

    private fun note(text: String) {
        timeline.add(text.take(3072)); while (timeline.size > Protocol.MAX_TIMELINE) timeline.removeAt(0)
    }
    private fun failure(error: Exception, context: String) {
        status = if ((error as? BridgeFailure)?.unknown == true) "$context · unknown — inspect the original request; never resend automatically"
            else "$context · ${error.message ?: "Unavailable"}"
        note(status)
    }
    private fun stopObserver() { observer?.cancel(); observer = null }
    private fun stopReads() { stopObserver(); readBudget?.cancel(); readBudget = null }
    fun foreground(active: Boolean) {
        foreground = active
        if (!active) { stopReads(); if (!busy) { status = "Observation paused; pending pages retained"; changed() } }
        else if (connection != null && !busy) refresh()
    }
    /** Retain an already-received page even when cancellation races the main-thread delivery. */
    private fun retain(mark: Long, action: () -> Unit) {
        val delivered = CountDownLatch(1)
        var failure: Throwable? = null
        main.post {
            try { if (!disposed && mark == epoch) action() }
            catch (error: Throwable) { failure = error }
            finally { delivered.countDown() }
        }
        delivered.await(); failure?.let { throw it }
    }

    fun configure(value: GatewayAPI) {
        check(!busy && connection == null) { "Disconnect before changing gateways" }
        stopReads(); epoch++; gateway = value; sessions = emptyList(); nextSessionOffset = null
        listSessions()
    }
    fun listSessions(more: Boolean = false) {
        val api = gateway ?: return
        if (busy) return
        val offset = if (more) nextSessionOffset ?: return else 0
        stopReads(); val budget = makeBudget(); readBudget = budget
        val mark = epoch; status = "Loading sessions"; changed()
        work.execute {
            val result = runCatching { api.sessions(offset, budget).also { budget.remainingMillis() } }
            main.post {
                if (disposed || mark != epoch || readBudget !== budget) return@post
                readBudget = null
                result.onSuccess {
                    sessions = if (more) (sessions + it.sessions).takeLast(128) else it.sessions
                    nextSessionOffset = it.next; status = "Choose a session"
                }.onFailure { if (!budget.observation.cancelled) failure(it as Exception, "Sessions unavailable") }
                changed(); startObservation()
            }
        }
    }

    fun open(session: String, readOnly: Boolean) {
        val api = gateway ?: return
        if (busy || connection != null) return
        stopReads(); val mark = ++epoch; val id = requestId()
        busy = true; status = "Connecting · open request $id"; note(status); changed()
        work.execute {
            var reserved = false; var received = false
            val result = runCatching {
                journal.reserve(session, id, "connection.open"); reserved = true
                val opened = api.open(session, readOnly, id)
                received = true
                journal.settle(id, CommandState.CONFIRMED)
                opened
            }
            result.exceptionOrNull()?.let {
                if (reserved && !received && it is BridgeFailure && !it.unknown) runCatching { journal.settle(id, CommandState.REJECTED) }
            }
            main.post {
                if (disposed || mark != epoch) return@post
                busy = false
                result.onSuccess {
                    connection = it; inbox = null; pendingHistory = null; chatCursor = null
                    terminalBytes.clear(); conversation.clear()
                    note("Connected · ${it.session} · ${it.capabilities.mode} · open request $id")
                    if (foreground) refresh()
                    else { status = "Connected · observation paused"; changed() }
                }.onFailure { failure(if (received || (reserved && it !is BridgeFailure)) BridgeFailure("receipt_store", "Lease response was not durably confirmed", "may_have_occurred") else it as Exception, "Open request $id"); changed() }
            }
        }
    }

    /** One complete authoritative read pass, including history reconciliation, shares one deadline. */
    fun refresh(workerOffset: Int = 0, continuing: ReadBudget? = null) {
        val api = gateway ?: return
        val old = connection ?: return
        if (busy || disposed) return
        stopReads()
        val budget = continuing ?: makeBudget(); readBudget = budget
        val mark = epoch; val target = chatTarget
        val originalCursor = chatCursor; val previousInbox = inbox; val previousHistory = pendingHistory
        status = "Refreshing state · pending pages remain unconsumed"; changed()
        work.execute {
            val result = runCatching {
                val caps = api.capabilities(old.id, budget)
                fun read(name: String, query: JSONObject = JSONObject()): JSONObject? =
                    if (name in caps.resources) api.bridge(old.id, Protocol.state(caps.generation, name, query), budget).getJSONObject("value") else null
                var events = previousInbox?.takeIf { it.generation == caps.generation }
                if (caps.events && events == null) {
                    events = EventInbox(caps.generation)
                    // Establish a live event boundary before the state/history snapshots; no wakeup can fall between them.
                    events.receive(api.events(old.id, caps.generation, null, budget, waitMs = 0))
                    val received = events
                    retain(mark) { if (readBudget === budget) inbox = received }
                }
                val after = if (old.capabilities.generation == caps.generation) originalCursor else null
                var history = previousHistory?.takeIf { it.generation == caps.generation && it.target == target && it.after == after }
                if ("conversation" in caps.resources && history == null) {
                    val query = JSONObject().put("limit", 32)
                    if (target != "next") query.put("thread_id", target)
                    after?.let { query.put("cursor", it.text) }
                    val page = Protocol.conversation(read("conversation", query)!!, after)
                    history = HistoryPage(caps.generation, target, after, page)
                    val received = history
                    retain(mark) { if (readBudget === budget && chatTarget == target) pendingHistory = received }
                }
                // These reads follow the potentially gapped page. A failure leaves both pages/cursors untouched.
                val rt = read("runtime")
                if (rt != null && !rt.isNull("live_cursor")) Counter.parse(rt.get("live_cursor"))
                val terminalState = read("terminals")
                val workerState = read("workers", JSONObject().put("offset", workerOffset).put("limit", 16))
                val roster = read("threads")
                val queued = read("chat.queue")
                budget.remainingMillis()
                Snapshot(caps, events, history, rt, terminalState, workerState, roster, queued)
            }
            main.post {
                if (disposed || mark != epoch || readBudget !== budget || chatTarget != target) return@post
                readBudget = null
                result.onSuccess { snapshot ->
                    try {
                        budget.remainingMillis()
                        if (old.capabilities.generation != snapshot.caps.generation) {
                            terminalBytes.clear(); conversation.clear(); chatCursor = null
                            note("Backend generation changed. Old command outcomes retain their original generation.")
                        }
                        connection = old.copy(capabilities = snapshot.caps)
                        runtime = snapshot.runtime; terminals = snapshot.terminals; workers = snapshot.workers
                        threads = snapshot.threads; queue = snapshot.queue; inbox = snapshot.inbox
                        if (inbox?.pending?.gap == true) { terminalBytes.clear(); note("Retention gap reconciled from current state.") }
                        inbox?.consume(snapshot.caps.generation) { page -> page.events.forEach { appendEvent(it) } }
                        snapshot.history?.let { history ->
                            val page = history.value
                            if (page.bool("gap")) conversation.add("保留窗口存在缺口；已刷新当前状态。")
                            val rows = page.getJSONArray("events")
                            for (i in 0 until rows.length()) {
                                val event = rows.getJSONObject(i); val source = event.getJSONObject("source")
                                val who = if (source.isNull("agent_id")) source.optString("kind", "unattributed") else source.string("agent_id")
                                conversation.add("${event.string("sequence")} · $who · ${event.string("kind")}\n${event.string("text").take(8192)}")
                            }
                            while (conversation.size > Protocol.MAX_TIMELINE) conversation.removeAt(0)
                            chatCursor = Counter.parse(page.get("next_cursor")); chatHasMore = page.bool("has_more")
                            if (pendingHistory === history) pendingHistory = null
                        }
                        status = "Connected · ${old.session} · ${snapshot.caps.mode}"; changed(); startObservation()
                    } catch (error: Exception) { failure(error, "State not reconciled"); changed() }
                }.onFailure { if (!budget.observation.cancelled) failure(it as Exception, "State unavailable; pending pages retained"); changed() }
            }
        }
    }

    fun command(name: String, args: JSONObject, expectedGeneration: String,
                completed: (CommandCompletion) -> Unit = {}): Boolean {
        val api = gateway ?: return false
        val current = connection ?: return false
        if (busy || !current.capabilities.can(name)) return false
        if (current.capabilities.generation != expectedGeneration) {
            failure(BridgeFailure("stale_generation", "Backend changed; refresh before submitting"), "Command not sent"); changed(); return false
        }
        val request = try { Protocol.command(expectedGeneration, name, args) }
            catch (error: Exception) { failure(error, "Invalid command"); changed(); return false }
        val destination = if (name == "chat.send") args.optString("target", "next") else args.optString("terminal_id")
        stopReads(); busy = true; val mark = epoch
        status = "Sending $name · ${request.id}"; note(status); changed()
        work.execute {
            var dispatched = false; var reserved = false; var received = false
            var state = CommandState.NOT_SENT
            val result = runCatching {
                journal.reserve(current.session, request.id, name, expectedGeneration, current.id, destination,
                    if (name == "chat.send") args.string("text") else if (name == "terminal.write") args.string("data") else null); reserved = true
                dispatched = true
                val receipt = api.bridge(current.id, request)
                received = true
                state = Protocol.receiptState(request, receipt.get("value"))
                journal.settle(request.id, state)
                receipt
            }
            result.exceptionOrNull()?.let { error ->
                state = if (!dispatched) CommandState.NOT_SENT
                    else if (received) CommandState.UNKNOWN
                    else if (error is BridgeFailure && !error.unknown) CommandState.REJECTED else CommandState.UNKNOWN
                if (reserved) runCatching { journal.settle(request.id, if (state == CommandState.NOT_SENT) CommandState.REJECTED else state) }
            }
            main.post {
                if (disposed) return@post
                if (mark == epoch) busy = false
                result.onSuccess {
                    status = if (state == CommandState.UNKNOWN) "$name · unknown · draft retained" else "$name · verified receipt received"
                    note("${current.session} · $name · ${request.id}\n$status\n${it.get("value")}")
                }.onFailure { failure(if (state == CommandState.UNKNOWN) BridgeFailure("unconfirmed", "Original command result remains unknown", "may_have_occurred") else it as Exception,
                    "${current.session} · $name · ${request.id}") }
                completed(CommandCompletion(state, current.id, expectedGeneration, request.id, destination))
                changed()
                if (mark == epoch && foreground) refresh()
            }
        }
        return true
    }

    private fun startObservation() {
        val api = gateway ?: return
        val current = connection ?: return
        val events = inbox ?: return
        if (!foreground || disposed || busy || readBudget != null || observer != null || !current.capabilities.events || events.generation != current.capabilities.generation) return
        if (events.pending != null) { refresh(); return }
        val observation = Observation(); observer = observation; val mark = epoch
        observations.execute {
            try {
                while (!observation.cancelled) {
                    val budget = ReadBudget(observation)
                    val issuedAfter = events.cursor
                    val page = api.events(current.id, events.generation, issuedAfter, budget)
                    // Retain the validated page even if pause races its arrival; it is not yet consumed.
                    retain(mark) {
                        if (inbox === events) events.receive(page, issuedAfter)
                        if (observer === observation && foreground && !observation.cancelled) {
                            val pending = events.pending
                            if (pending != null && (pending.gap || (pending.events.isNotEmpty() && "conversation" in current.capabilities.resources))) {
                                observer = null
                                refresh(continuing = budget.continuing())
                                observation.cancel()
                            } else {
                                events.consume { received -> received.events.forEach { appendEvent(it) } }
                                changed()
                            }
                        }
                    }
                }
            } catch (error: Exception) {
                if (!observation.cancelled) main.post {
                    if (!disposed && mark == epoch && observer === observation) {
                        observer = null; failure(error, "Observation stopped; cursor retained"); changed()
                        if (error is BridgeFailure && error.code == "stale_generation" && !busy) refresh()
                    }
                }
            }
        }
    }
    private fun appendEvent(event: BridgeEvent) {
        val value = event.value as? JSONObject
        if (value?.optString("kind") == "terminal") {
            val id = value.optString("terminal_id")
            if (id.isNotEmpty() && (id in terminalBytes || terminalBytes.size < 16)) {
                val bytes = runCatching { Base64.decode(value.string("data_base64"), Base64.NO_WRAP) }.getOrNull()
                if (bytes != null) terminalBytes[id] = ((terminalBytes[id] ?: byteArrayOf()) + bytes).takeLast(32768).toByteArray()
            }
            note("${event.sequence.text} · ${event.topic} · $id · output available in Terminals")
        } else note("${event.sequence.text} · ${event.topic}\n${event.value}")
    }
    fun terminalOutput(id: String): String = (terminalBytes[id]?.toString(Charsets.UTF_8) ?: "Waiting for terminal output…")
        .replace(Regex("\\u001B\\[[0-?]*[ -/]*[@-~]"), "")
        .replace(Regex("[\\u0000-\\u0008\\u000B\\u000C\\u000E-\\u001F]"), "")
    fun selectThread(target: String) {
        if (target == chatTarget) return
        chatTarget = target; conversation.clear(); chatCursor = null; pendingHistory = null; chatHasMore = false
        changed(); if (!busy) refresh()
    }
    fun refreshChat() { if (!busy && readBudget == null) refresh() }
    fun disconnect(onDone: (() -> Unit)? = null) {
        val api = gateway; val current = connection
        if (busy) return
        stopReads()
        if (api == null || current == null) { onDone?.invoke(); return }
        busy = true; val mark = epoch; val id = requestId()
        status = "Releasing lease ${current.id} · request $id"; note(status); changed()
        work.execute {
            var reserved = false; var received = false
            val result = runCatching {
                journal.reserve(current.session, id, "connection.close", current.capabilities.generation, current.id); reserved = true
                api.close(current.id); received = true; journal.settle(id, CommandState.CONFIRMED)
            }
            result.exceptionOrNull()?.let {
                if (reserved && !received && it is BridgeFailure && !it.unknown) runCatching { journal.settle(id, CommandState.REJECTED) }
            }
            main.post {
                if (disposed || mark != epoch) return@post
                busy = false
                result.onSuccess {
                    epoch++; connection = null; inbox = null; pendingHistory = null; runtime = null; terminals = null; workers = null
                    terminalBytes.clear(); threads = null; queue = null; conversation.clear(); chatCursor = null
                    status = "Disconnected · session remains running"; onDone?.invoke()
                }.onFailure { failure(if (received || (reserved && it !is BridgeFailure)) BridgeFailure("receipt_store", "Lease release was not durably confirmed", "may_have_occurred") else it as Exception,
                    "Close request $id · connection ${current.id}") }
                changed()
            }
        }
    }
    fun clearPairing() {
        check(!busy && connection == null)
        stopReads(); epoch++; gateway = null; sessions = emptyList(); nextSessionOffset = null
        status = "Pairing deleted; unresolved request metadata retained"; changed()
    }
    fun destroy() {
        disposed = true; foreground = false; stopReads()
        work.shutdown(); observations.shutdown()
    }
}

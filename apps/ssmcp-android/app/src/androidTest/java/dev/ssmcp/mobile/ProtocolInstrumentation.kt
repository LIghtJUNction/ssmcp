package dev.ssmcp.mobile

import android.app.Activity
import android.app.Instrumentation
import android.os.Bundle
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/** Runs against platform JSON, preferences, and the real controller executors without network dependencies. */
class ProtocolInstrumentation : Instrumentation() {
    override fun onCreate(arguments: Bundle?) { super.onCreate(arguments); start() }
    override fun onStart() {
        val result = Bundle()
        try {
            countersAndPages()
            receiptsSchemasAndLeases()
            draftContextAndBudget()
            journalPreservesUnknownInput()
            gapRefreshResumesWithoutRefetch()
            refreshSharesOneDeadline()
            result.putString("stream", "SSMCP protocol and controller checks passed: counters, event order/gap, schema gates, lease/command correlation, unknown admission, draft context, durable input, shared deadline, failed-gap pause/resume.\n")
            finish(Activity.RESULT_OK, result)
        } catch (error: Throwable) {
            result.putString("stream", "SSMCP check failed: ${error.javaClass.simpleName}: ${error.message}\n")
            finish(Activity.RESULT_CANCELED, result)
        }
    }
    private fun rejects(action: () -> Unit) { check(runCatching(action).isFailure) }
    private fun unknown(action: () -> Unit) { check((runCatching(action).exceptionOrNull() as? BridgeFailure)?.unknown == true) }
    private fun onMain(action: () -> Unit) {
        var failure: Throwable? = null
        runOnMainSync { try { action() } catch (error: Throwable) { failure = error } }
        failure?.let { throw it }
    }
    private fun CountDownLatch.ready() { check(await(5, TimeUnit.SECONDS)) { "Controller transition timed out" } }
    private fun countersAndPages() {
        check(Counter.parse("9007199254740993") > Counter.parse("9007199254740992"))
        check(Counter.parse("18446744073709551615").text == "18446744073709551615")
        listOf<Any>("01", "-1", "", "18446744073709551616", 42, 1.0).forEach { value -> rejects { Counter.parse(value) } }
        val page = JSONObject("""{"events":[{"sequence":"9007199254740993","topic":"ssmcp.event","value":{"text":"真实事件"}}],"start_cursor":"9007199254740993","next_cursor":"9007199254740994","live_cursor":"9007199254740994","retention_gap":false,"has_more":false}""")
        check(Protocol.page(page, Counter.parse("9007199254740993"), 16).events.size == 1)
        rejects { Protocol.page(page, Counter.parse("9007199254740992"), 16) }
        page.put("retention_gap", true)
        val gap = Protocol.page(page, Counter.parse("9007199254740992"), 16)
        val issued = Counter.parse("9007199254740992")
        val inbox = EventInbox("generation-1", issued); check(inbox.receive(gap, issued))
        rejects { inbox.consume { error("Gap must not be consumed without reconciliation") } }
        rejects { inbox.consume("generation-1") { throw BridgeFailure("unavailable", "snapshot failed") } }
        check(inbox.cursor?.text == "9007199254740992" && inbox.pending != null)
        var rows = 0
        check(inbox.consume("generation-1") { rows += it.events.size }); check(rows == 1)
        check(!inbox.consume("generation-1") { error("Page consumed twice") })
        check(inbox.cursor?.text == "9007199254740994")
        check(!inbox.receive(gap, issued) && inbox.pending == null)
        val event = page.getJSONArray("events").getJSONObject(0)
        page.put("events", JSONArray().put(event).put(event))
        rejects { Protocol.page(page, Counter.parse("9007199254740992"), 16) }
    }
    private fun envelope(request: BridgeRequest, payload: JSONObject, schema: Int = 1): JSONObject {
        val value = if (request.isCommand) JSONObject().put("command", request.target).put("value", payload)
            else JSONObject().put("resource", request.target).put("schema_version", schema).put("value", payload)
        return JSONObject().put("version", 1).put("id", request.id).put("generation", request.generation)
            .put("body", JSONObject().put("kind", request.op).put("value", value))
    }
    private fun capsEnvelope(schema: Int = 1): JSONObject = JSONObject().put("version", 1).put("id", "open-id").put("generation", "generation-1")
        .put("body", JSONObject().put("kind", "capabilities").put("value", JSONObject().put("application", "ssmcp").put("mode", "observe")
            .put("events", true).put("max_frame_bytes", Protocol.MAX_FRAME).put("max_event_page_bytes", Protocol.MAX_PAGE)
            .put("commands", JSONArray()).put("resources", JSONArray().put(JSONObject().put("id", "runtime").put("schema_version", schema).put("description", "state")))))
    private fun receiptsSchemasAndLeases() {
        val req = Protocol.command("generation-1", "terminal.write", JSONObject().put("terminal_id", "main").put("data", "hi"))
        val response = envelope(req, JSONObject())
        Protocol.delivered(response, req)
        response.put("id", "another-request"); unknown { Protocol.delivered(response, req) }
        response.put("id", req.id).put("generation", "generation-2"); unknown { Protocol.delivered(response, req) }
        val chat = Protocol.command("generation-1", "chat.send", JSONObject().put("text", "你好").put("target", "agent-1"))
        check(chat.json.getJSONObject("operation").getJSONObject("arguments").string("request_id") == chat.id)
        val receipt = JSONObject().put("request_id", chat.id).put("message_id", "message-1").put("target", "agent-1")
            .put("status", "sending").put("duplicate", false).put("confirmation", "local_queue_admission")
        check(Protocol.receiptState(chat, receipt) == CommandState.CONFIRMED)
        for ((key, value) in listOf("request_id" to "other", "target" to "agent-2", "confirmation" to "other", "message_id" to "")) {
            val invalid = JSONObject(receipt.toString()).put(key, value)
            unknown { Protocol.delivered(envelope(chat, invalid), chat) }
        }
        for (state in listOf("unknown", "uncertain")) check(Protocol.receiptState(chat, JSONObject(receipt.toString()).put("status", state)) == CommandState.UNKNOWN)
        check(Protocol.receiptState(chat, JSONObject(receipt.toString()).put("uncertain", true)) == CommandState.UNKNOWN)
        rejects { Protocol.command("generation-1", "chat.send", JSONObject().put("request_id", "external").put("text", "body")) }
        val open = JSONObject().put("status", "connected").put("session", "test").put("read_only", true)
            .put("connection_id", "connection-1").put("duplicate", false).put("capabilities", capsEnvelope())
        check(Protocol.opened(open, "open-id", "test", true).id == "connection-1")
        unknown { Protocol.opened(open, "different-id", "test", true) }; unknown { Protocol.opened(open, "open-id", "test", false) }
        unknown { Protocol.closed(JSONObject().put("closed", false).put("session_stopped", false)) }
        Protocol.closed(JSONObject().put("closed", true).put("session_stopped", false))
        check("runtime" !in Protocol.capabilitiesResponse(capsEnvelope(2)).resources)
        val read = Protocol.state("generation-1", "runtime")
        rejects { Protocol.response(envelope(read, JSONObject().put("live_cursor", "1"), 2), read) }
        val fakeError = JSONObject().put("code", "invented_error").put("message", "bad").put("effect", "none")
        rejects { Protocol.error(fakeError) }; check(Protocol.error(fakeError, wire = false).code == "invented_error")
        rejects { Gateway("http://example.test", "secret") }; rejects { Gateway("https://user:secret@example.test", "secret") }
        rejects { Gateway("https://example.test?token=secret", "secret") }
        check(Gateway("https://example.test/", "secret").origin == "https://example.test")
    }
    private fun draftContextAndBudget() {
        val caps = Capabilities("generation-1", "control", emptySet(), setOf("chat.send"), false)
        val connection = OpenConnection("connection-1", "test", caps)
        val confirmed = CommandCompletion(CommandState.CONFIRMED, connection.id, caps.generation, "request-1", "agent-1")
        check(confirmed.mayClear(connection, "body", "body", "agent-1"))
        for (state in listOf(CommandState.UNKNOWN, CommandState.REJECTED, CommandState.NOT_SENT)) {
            check(!confirmed.copy(state = state).mayClear(connection, "body", "body", "agent-1"))
        }
        check(!confirmed.mayClear(connection, "body", "edited", "agent-1"))
        check(!confirmed.mayClear(connection, "body", "body", "agent-2"))
        check(!confirmed.mayClear(null, "body", "body", "agent-1"))
        check(!confirmed.mayClear(connection.copy(id = "connection-2"), "body", "body", "agent-1"))
        check(!confirmed.mayClear(connection.copy(capabilities = caps.copy(generation = "generation-2")), "body", "body", "agent-1"))
        var nanos = 0L
        val budget = ReadBudget(clock = { nanos })
        nanos = 20_000_000_000L; check(budget.remainingMillis() == 35_000L)
        val continued = budget.continuing()
        nanos = 54_000_000_000L; check(continued.remainingMillis() == 1000L)
        nanos = 55_000_000_000L; rejects { budget.remainingMillis() }; rejects { continued.remainingMillis() }
        val read = ReadBudget(); val independent = ReadBudget()
        read.cancel(); rejects { read.remainingMillis() }; check(independent.remainingMillis() > 0)
    }
    private fun journalPreservesUnknownInput() {
        val uncertain = JSONObject().put("id", "unknown").put("state", "unknown")
        val settled = JSONObject().put("id", "confirmed").put("state", "confirmed")
        val incoming = JSONObject().put("id", "new").put("state", "unknown")
        val entries = JournalPolicy.insert(listOf(uncertain, settled), incoming, 2)
        check(entries.map { it.string("id") } == listOf("unknown", "new"))
        rejects { JournalPolicy.insert(entries, JSONObject().put("id", "another").put("state", "unknown"), 2) }
        val name = "receipt-test-${requestId()}"
        try {
            val journal = CommandJournal(targetContext, name)
            journal.reserve("test", "original-id", "chat.send", "generation-1", "connection-1", "agent-1", "完整原文\n不自动重发")
            check(CommandJournal(targetContext, name).inputs().single().text == "完整原文\n不自动重发")
            journal.settle("original-id", CommandState.CONFIRMED)
            check(journal.inputs().isEmpty())
        } finally { targetContext.deleteSharedPreferences(name) }
    }
    private fun history(cursor: String?): JSONObject {
        val events = JSONArray()
        if (cursor == null) events.put(JSONObject().put("sequence", "0").put("kind", "human_input").put("text", "history-A")
            .put("source", JSONObject().put("kind", "local_human").put("agent_id", JSONObject.NULL).put("request_id", JSONObject.NULL).put("connection", "1")))
        return JSONObject().put("schema_version", 1).put("retention", "live_process_memory").put("start_cursor", "0")
            .put("next_cursor", "1").put("end_cursor", "1").put("gap", false).put("has_more", false).put("events", events)
    }
    private inner class ScriptedGateway(var caps: Capabilities, val clock: AtomicLong = AtomicLong()) : GatewayAPI {
        override val origin = "https://test.invalid"
        val eventCalls = AtomicInteger(); val historyCalls = AtomicInteger()
        @Volatile var failRuntime = false
        @Volatile var readCostNanos = 0L
        @Volatile var terminalName = "original"
        val budgets = java.util.Collections.synchronizedSet(HashSet<ReadBudget>())
        override fun sessions(offset: Int, budget: ReadBudget) = SessionPage(emptyList(), null)
        override fun open(session: String, readOnly: Boolean, openRequestId: String) = OpenConnection("connection-1", session, caps)
        override fun capabilities(id: String, budget: ReadBudget): Capabilities { budgets.add(budget); return caps }
        override fun close(id: String) {}
        override fun events(id: String, generation: String, cursor: Counter?, budget: ReadBudget, waitMs: Long): EventPage {
            eventCalls.incrementAndGet()
            return Protocol.page(JSONObject("""{"events":[{"sequence":"1","topic":"ssmcp.event","value":{"text":"gap-page-A"}}],"start_cursor":"1","next_cursor":"2","live_cursor":"2","retention_gap":true,"has_more":false}"""), cursor, 32)
        }
        override fun bridge(id: String, request: BridgeRequest, budget: ReadBudget?): JSONObject {
            check(!request.isCommand) { "Integration read fixture must never mutate" }
            check(budget != null); budget.remainingMillis(); budgets.add(budget)
            clock.addAndGet(readCostNanos)
            val payload = when (request.target) {
                "runtime" -> { if (failRuntime) throw BridgeFailure("unavailable", "snapshot failed"); JSONObject().put("live_cursor", "2") }
                "conversation" -> {
                    historyCalls.incrementAndGet()
                    val query = request.json.getJSONObject("operation").getJSONObject("query")
                    history(if (query.has("cursor")) query.string("cursor") else null)
                }
                "terminals" -> JSONObject().put("terminals", JSONArray().put(JSONObject().put("terminal_id", "main").put("name", terminalName).put("cwd", "/")))
                "workers" -> JSONObject().put("pool_generation", "pool-1").put("slots", JSONArray()).put("next_offset", JSONObject.NULL)
                else -> error("Unexpected state resource")
            }
            return Protocol.response(envelope(request, payload), request)
        }
    }
    private fun gapRefreshResumesWithoutRefetch() {
        val name = "gap-test-${requestId()}"
        val api = ScriptedGateway(Capabilities("generation-1", "control", setOf("runtime", "conversation"), emptySet(), true)).apply { failRuntime = true }
        val listed = CountDownLatch(1); val opened = CountDownLatch(1); val failed = CountDownLatch(1); val complete = AtomicReference(CountDownLatch(1))
        var controller: ClientController? = null
        try {
            onMain {
                controller = ClientController(CommandJournal(targetContext, name)) {
                    val status = controller!!.status
                    if (status == "Choose a session") listed.countDown()
                    if (status == "Connected · observation paused") opened.countDown()
                    if (status.startsWith("State unavailable")) failed.countDown()
                    if (status.startsWith("Connected · fixture")) complete.get().countDown()
                }
                controller!!.configure(api)
            }
            listed.ready(); onMain { controller!!.open("fixture", false) }; opened.ready()
            check(api.eventCalls.get() == 0) // A lease receipt arriving in the background must not start reads.
            onMain { controller!!.refresh() }; failed.ready()
            onMain { check(controller!!.cursor == null); check(controller!!.conversation.isEmpty()); controller!!.foreground(false) }
            check(api.eventCalls.get() == 1); check(api.historyCalls.get() == 1)
            api.failRuntime = false; onMain { controller!!.refresh() }; complete.get().ready()
            onMain {
                check(controller!!.cursor?.text == "2"); check(controller!!.conversation.size == 1)
                check(controller!!.timeline.count { it.contains("gap-page-A") } == 1)
            }
            check(api.eventCalls.get() == 1); check(api.historyCalls.get() == 1)
            complete.set(CountDownLatch(1)); onMain { controller!!.refresh() }; complete.get().ready()
            onMain { check(controller!!.conversation.size == 1); check(controller!!.timeline.count { it.contains("gap-page-A") } == 1) }
            check(api.eventCalls.get() == 1)
        } finally { onMain { controller?.destroy() }; targetContext.deleteSharedPreferences(name) }
    }
    private fun refreshSharesOneDeadline() {
        val name = "deadline-test-${requestId()}"
        val api = ScriptedGateway(Capabilities("generation-1", "control", setOf("runtime", "terminals", "workers"), emptySet(), false))
        val listed = CountDownLatch(1); val opened = CountDownLatch(1); val complete = CountDownLatch(1); val failed = CountDownLatch(1)
        var controller: ClientController? = null
        try {
            onMain {
                controller = ClientController(CommandJournal(targetContext, name), makeBudget = { ReadBudget(clock = { api.clock.get() }) }) {
                    val status = controller!!.status
                    if (status == "Choose a session") listed.countDown()
                    if (status == "Connected · observation paused") opened.countDown()
                    if (status.startsWith("Connected · fixture")) complete.countDown()
                    if (status.startsWith("State unavailable")) failed.countDown()
                }
                controller!!.configure(api)
            }
            listed.ready(); onMain { controller!!.open("fixture", false) }; opened.ready()
            onMain { controller!!.refresh() }; complete.ready()
            onMain { check(controller!!.terminals!!.getJSONArray("terminals").getJSONObject(0).string("name") == "original") }
            api.budgets.clear(); api.readCostNanos = 20_000_000_000L; api.terminalName = "late"
            onMain { controller!!.refresh() }; failed.ready()
            check(api.clock.get() == 60_000_000_000L && api.budgets.size == 1)
            onMain { check(controller!!.terminals!!.getJSONArray("terminals").getJSONObject(0).string("name") == "original") }
        } finally { onMain { controller?.destroy() }; targetContext.deleteSharedPreferences(name) }
    }
}

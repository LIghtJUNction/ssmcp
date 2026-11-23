package dev.ssmcp.mobile

import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/** zbridge v1. Never convert wire counters through Double or signed Long. */
class Counter private constructor(val text: String) : Comparable<Counter> {
    override fun equals(other: Any?): Boolean = other is Counter && text == other.text
    override fun hashCode(): Int = text.hashCode()
    override fun toString(): String = "Counter(text=$text)"

    override fun compareTo(other: Counter): Int =
        if (text.length != other.text.length) text.length.compareTo(other.text.length)
        else text.compareTo(other.text)
    companion object {
        fun parse(value: Any?): Counter {
            require(value is String && value.matches(Regex("0|[1-9][0-9]{0,19}"))) { "Non-canonical counter" }
            require(value.length < 20 || value <= "18446744073709551615") { "Counter overflow" }
            return Counter(value)
        }
    }
}

class BridgeFailure(val code: String, message: String, val effect: String = "none") : Exception(message) {
    val unknown: Boolean get() = effect == "may_have_occurred"
}

internal fun JSONObject.string(key: String): String =
    (get(key) as? String) ?: throw IllegalArgumentException("Expected string: $key")
internal fun JSONObject.bool(key: String): Boolean =
    (get(key) as? Boolean) ?: throw IllegalArgumentException("Expected boolean: $key")
internal fun JSONObject.integer(key: String, range: IntRange): Int {
    val n = get(key)
    require(n is Int || n is Long) { "Expected integer: $key" }
    val long = (n as Number).toLong()
    require(long in range.first.toLong()..range.last.toLong()) { "Out of range: $key" }
    return long.toInt()
}
internal fun JSONObject.keysExactly(required: Set<String>, optional: Set<String> = emptySet()) {
    val keys = keys().asSequence().toSet()
    require(keys.containsAll(required) && keys.all { it in required || it in optional }) { "Unexpected protocol fields" }
}
internal fun bridgeId(value: String): String {
    require(value.matches(Regex("[A-Za-z0-9_.:-]{1,128}"))) { "Invalid bridge identifier" }
    return value
}
internal fun requestId(): String = UUID.randomUUID().toString()
internal fun byteSize(value: String): Int = value.toByteArray(Charsets.UTF_8).size

data class Capabilities(val generation: String, val mode: String, val resources: Set<String>,
                        val commands: Set<String>, val events: Boolean) {
    fun can(command: String): Boolean = mode == "control" && command in commands
}
data class BridgeEvent(val sequence: Counter, val topic: String, val value: Any?)
data class EventPage(val events: List<BridgeEvent>, val start: Counter, val next: Counter,
                     val live: Counter, val gap: Boolean, val hasMore: Boolean)
data class BridgeRequest(val json: JSONObject, val id: String, val op: String,
                         val generation: String? = null, val target: String? = null) {
    val isCommand: Boolean get() = op == "command"
}

enum class CommandState { CONFIRMED, REJECTED, UNKNOWN, NOT_SENT }
data class UnconfirmedInput(val id: String, val operation: String, val connectionId: String,
                            val generation: String, val target: String, val text: String)
data class CommandCompletion(val state: CommandState, val connectionId: String,
                             val generation: String, val requestId: String, val target: String) {
    fun mayClear(current: OpenConnection?, originalDraft: String, currentDraft: String, currentTarget: String): Boolean =
        state == CommandState.CONFIRMED && current != null && current.id == connectionId &&
            current.capabilities.generation == generation && originalDraft == currentDraft && target == currentTarget
}

/** A gap page remains pending until a successful authoritative refresh of this generation. */
class EventInbox(val generation: String, initialCursor: Counter? = null) {
    var cursor = initialCursor; private set
    var pending: EventPage? = null; private set
    fun receive(page: EventPage, issuedAfter: Counter? = null): Boolean {
        if (cursor != issuedAfter || pending != null || (cursor != null && page.next < cursor!!)) return false
        pending = page; return true
    }
    fun consume(reconciledGeneration: String? = null, apply: (EventPage) -> Unit): Boolean {
        val page = pending ?: return false
        check(!page.gap || reconciledGeneration == generation) { "Refresh the gap before consuming it" }
        apply(page)
        cursor = page.next; pending = null
        return true
    }
}

object Protocol {
    const val MAX_FRAME = 256 * 1024
    const val MAX_PAGE = 128 * 1024
    const val MAX_WAIT_MS = 55_000
    const val MAX_TIMELINE = 300

    fun capabilities(): BridgeRequest = request("capabilities")
    fun state(generation: String, resource: String, query: JSONObject = JSONObject()): BridgeRequest =
        request("state", generation, resource, query)
    fun command(generation: String, command: String, args: JSONObject): BridgeRequest =
        request("command", generation, command, args)

    private fun request(op: String, generation: String? = null, target: String? = null,
                        args: JSONObject = JSONObject()): BridgeRequest {
        val id = requestId()
        val arguments = JSONObject(args.toString())
        if (op == "command" && target == "chat.send") {
            require(!arguments.has("request_id")) { "The client assigns one ID to the command and chat admission" }
            arguments.put("request_id", id)
        }
        require(byteSize(arguments.toString()) <= 32 * 1024) { "Arguments too large" }
        val operation = JSONObject().put("op", op)
        generation?.let { operation.put("generation", bridgeId(it)) }
        target?.let {
            operation.put(if (op == "state") "resource" else "command", bridgeId(it))
            operation.put(if (op == "state") "query" else "arguments", arguments)
        }
        return BridgeRequest(JSONObject().put("version", 1).put("id", id).put("operation", operation),
            id, op, generation, target)
    }

    fun error(value: JSONObject, wire: Boolean = true): BridgeFailure {
        value.keysExactly(setOf("code", "message", "effect"))
        val effect = value.string("effect")
        require(effect == "none" || effect == "may_have_occurred") { "Invalid effect" }
        val message = value.string("message")
        require(byteSize(message) <= 1024) { "Oversized error" }
        if (wire) require(value.string("code") in setOf("invalid_request", "unsupported_version", "denied", "closed",
            "unavailable", "stale_generation", "invalid_cursor", "too_large", "conflict", "internal")) { "Unknown zbridge error code" }
        return BridgeFailure(value.string("code"), message, effect)
    }

    /** Validates association before callers can update a visible model. */
    fun response(raw: JSONObject, request: BridgeRequest): JSONObject {
        val body = envelope(raw)
        require(raw.string("id") == request.id) { "Response request mismatch" }
        if (body.string("kind") == "error") {
            val failure = error(body.getJSONObject("value"))
            require(request.isCommand || !failure.unknown) { "Read response claimed effects" }
            throw failure
        }
        if (request.generation != null && raw.string("generation") != request.generation)
            throw BridgeFailure("stale_generation", "Backend generation changed",
                if (request.isCommand) "may_have_occurred" else "none")
        require(body.string("kind") == request.op) { "Response kind mismatch" }
        val value = body.getJSONObject("value")
        when (request.op) {
            "capabilities" -> capabilitiesResponse(raw)
            "state" -> {
                value.keysExactly(setOf("resource", "schema_version", "value"), setOf("revision"))
                require(bridgeId(value.string("resource")) == request.target) { "State resource mismatch" }
                value.integer("schema_version", 1..1)
                if (!value.isNull("revision")) Counter.parse(value.get("revision"))
            }
            "command" -> {
                value.keysExactly(setOf("command", "value"))
                require(bridgeId(value.string("command")) == request.target) { "Command receipt mismatch" }
                receiptState(request, value.get("value"))
            }
        }
        return value
    }

    fun delivered(raw: JSONObject, request: BridgeRequest): JSONObject = try { response(raw, request) }
        catch (error: BridgeFailure) { throw error }
        catch (_: Exception) { throw BridgeFailure("invalid_response", "Original command receipt could not be validated",
            if (request.isCommand) "may_have_occurred" else "none") }

    private fun envelope(raw: JSONObject): JSONObject {
        require(byteSize(raw.toString()) <= MAX_FRAME) { "Frame too large" }
        raw.keysExactly(setOf("version", "id", "generation", "body"))
        raw.integer("version", 1..1)
        bridgeId(raw.string("id"))
        val body = raw.getJSONObject("body")
        body.keysExactly(setOf("kind", "value"))
        if (body.string("kind") != "error") bridgeId(raw.string("generation"))
        else if (!raw.isNull("generation")) bridgeId(raw.string("generation"))
        return body
    }

    /** Open returns an embedded capabilities response, with a gateway-owned request id. */
    fun capabilitiesResponse(raw: JSONObject): Capabilities {
        val body = envelope(raw)
        if (body.string("kind") == "error") throw error(body.getJSONObject("value"))
        require(body.string("kind") == "capabilities") { "Missing capabilities" }
        val v = body.getJSONObject("value")
        v.keysExactly(setOf("application", "mode", "resources", "commands", "events",
            "max_frame_bytes", "max_event_page_bytes"))
        require(bridgeId(v.string("application")) == "ssmcp") { "Unexpected application" }
        val mode = v.string("mode")
        require(mode == "observe" || mode == "control") { "Invalid access mode" }
        v.integer("max_frame_bytes", 1..MAX_FRAME)
        v.integer("max_event_page_bytes", 1..MAX_PAGE)
        val resources = v.getJSONArray("resources")
        val commands = v.getJSONArray("commands")
        require(resources.length() + commands.length() <= 128) { "Too many capabilities" }
        val resourceIds = mutableSetOf<String>()
        val allResourceIds = mutableSetOf<String>()
        val commandIds = mutableSetOf<String>()
        for (i in 0 until resources.length()) {
            val r = resources.getJSONObject(i)
            r.keysExactly(setOf("id", "schema_version", "description"))
            val schema = r.integer("schema_version", 1..65535)
            val resource = bridgeId(r.string("id"))
            require(byteSize(r.string("description")) <= 1024 && allResourceIds.add(resource))
            if (schema == 1) resourceIds.add(resource)
        }
        for (i in 0 until commands.length()) {
            val c = commands.getJSONObject(i)
            c.keysExactly(setOf("id", "description", "input_schema"))
            require(byteSize(c.string("description")) <= 1024 && commandIds.add(bridgeId(c.string("id"))))
            require(byteSize(c.getJSONObject("input_schema").toString()) <= 32 * 1024)
        }
        require(mode != "observe" || commandIds.isEmpty()) { "Observer advertised write commands" }
        return Capabilities(raw.string("generation"), mode, resourceIds, commandIds, v.bool("events"))
    }

    fun receiptState(request: BridgeRequest, payload: Any?): CommandState {
        val value = payload as? JSONObject
        if (request.target == "chat.send") {
            require(value != null) { "Missing chat admission receipt" }
            val args = request.json.getJSONObject("operation").getJSONObject("arguments")
            require(value.string("request_id") == request.id && args.string("request_id") == request.id) { "Chat request ID mismatch" }
            require(value.string("target") == args.optString("target", "next")) { "Chat target mismatch" }
            require(value.string("confirmation") == "local_queue_admission") { "Unconfirmed queue admission" }
            bridgeId(value.string("message_id")); value.bool("duplicate")
            require(value.string("status").isNotEmpty()) { "Missing admission status" }
        }
        return if (value?.optString("status") in setOf("unknown", "uncertain") ||
            value?.optBoolean("uncertain") == true || value?.optString("effect") == "may_have_occurred") CommandState.UNKNOWN
        else CommandState.CONFIRMED
    }

    fun opened(raw: JSONObject, requestId: String, session: String, readOnly: Boolean): OpenConnection {
        try {
            require(raw.string("status") == "connected" && raw.string("session") == session && raw.bool("read_only") == readOnly)
            require(raw.getJSONObject("capabilities").string("id") == requestId) { "Open response ID mismatch" }
            val caps = capabilitiesResponse(raw.getJSONObject("capabilities"))
            require((caps.mode == "observe") == readOnly) { "Connection mode mismatch" }
            return OpenConnection(bridgeId(raw.string("connection_id")), session, caps)
        } catch (error: Exception) {
            throw BridgeFailure("invalid_response", "Lease may exist; inspect open request $requestId", "may_have_occurred")
        }
    }

    fun closed(raw: JSONObject) {
        try { require(raw.bool("closed") && !raw.bool("session_stopped")) }
        catch (_: Exception) { throw BridgeFailure("invalid_response", "Lease release is unconfirmed", "may_have_occurred") }
    }

    fun eventEnvelope(raw: JSONObject, id: String, generation: String, cursor: Counter?, limit: Int): EventPage {
        raw.keysExactly(setOf("schema", "request_id", "generation", "page"))
        require(raw.string("schema") == "ssmcp-gui-events-v1" && raw.string("request_id") == id) {
            "Event response association mismatch"
        }
        if (raw.string("generation") != generation) throw BridgeFailure("stale_generation", "Backend generation changed")
        return page(raw.getJSONObject("page"), cursor, limit)
    }

    fun page(raw: JSONObject, cursor: Counter?, limit: Int): EventPage {
        require(limit in 1..128 && byteSize(raw.toString()) <= MAX_PAGE) { "Event page too large" }
        raw.keysExactly(setOf("events", "start_cursor", "next_cursor", "live_cursor", "retention_gap", "has_more"))
        val start = Counter.parse(raw.get("start_cursor"))
        val next = Counter.parse(raw.get("next_cursor"))
        val live = Counter.parse(raw.get("live_cursor"))
        val gap = raw.bool("retention_gap")
        val more = raw.bool("has_more")
        require(start <= next && next <= live && more == (next < live)) { "Invalid page cursors" }
        if (cursor != null) require(next >= cursor && gap == (cursor < start)) { "Cursor mismatch" }
        val array = raw.getJSONArray("events")
        require(array.length() <= limit) { "Too many events" }
        val events = ArrayList<BridgeEvent>()
        var previous: Counter? = null
        for (i in 0 until array.length()) {
            val e = array.getJSONObject(i)
            e.keysExactly(setOf("sequence", "topic", "value"))
            val seq = Counter.parse(e.get("sequence"))
            require(seq >= start && seq < next && (previous == null || seq > previous)) { "Invalid event order" }
            if (cursor != null) require(seq >= cursor) { "Event before requested cursor" }
            events.add(BridgeEvent(seq, bridgeId(e.string("topic")), e.get("value")))
            previous = seq
        }
        return EventPage(events, start, next, live, gap, more)
    }

    fun conversation(raw: JSONObject, cursor: Counter?): JSONObject {
        raw.integer("schema_version", 1..1)
        require(raw.string("retention") == "live_process_memory")
        val start = Counter.parse(raw.get("start_cursor"))
        val next = Counter.parse(raw.get("next_cursor"))
        val end = Counter.parse(raw.get("end_cursor"))
        require(start <= next && next <= end)
        val gap = raw.bool("gap")
        if (cursor != null) require(next >= cursor && gap == (cursor < start))
        raw.bool("has_more")
        val events = raw.getJSONArray("events")
        require(events.length() <= 64 && byteSize(raw.toString()) <= 64 * 1024)
        var previous: Counter? = null
        for (i in 0 until events.length()) {
            val event = events.getJSONObject(i)
            val seq = Counter.parse(event.get("sequence"))
            require(seq >= start && seq < next && (previous == null || seq > previous))
            if (cursor != null) require(seq >= cursor)
            val source = event.getJSONObject("source")
            source.string("kind")
            if (!source.isNull("agent_id")) source.string("agent_id")
            for (key in listOf("connection", "request_id")) if (!source.isNull(key)) Counter.parse(source.get(key))
            event.string("text"); event.string("kind"); previous = seq
        }
        return raw
    }
}

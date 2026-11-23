package dev.ssmcp.mobile

import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URI
import java.util.concurrent.CancellationException
import java.util.concurrent.ScheduledThreadPoolExecutor
import java.util.concurrent.TimeUnit
import javax.net.ssl.HttpsURLConnection

/** Each observation owns a separate handle; cancellation never targets a command. */
class Observation {
    @Volatile var cancelled = false
        private set
    @Volatile private var connection: HttpURLConnection? = null
    fun attach(value: HttpURLConnection) {
        connection = value
        if (cancelled) { value.disconnect(); throw CancellationException() }
    }
    fun cancel() { cancelled = true; connection?.disconnect() }
    fun release() { connection = null }
}

/** One absolute deadline is shared by every read in a reconciliation pass. */
class ReadBudget(val observation: Observation = Observation(), windowMs: Long = 55_000,
                 private val clock: () -> Long = System::nanoTime,
                 private val deadlineNanos: Long = clock() + windowMs * 1_000_000) {
    init { require(windowMs in 1..55_000) }
    fun remainingMillis(): Long {
        if (observation.cancelled) throw CancellationException()
        val nanos = deadlineNanos - clock()
        if (nanos <= 0) throw BridgeFailure("deadline", "Observation deadline elapsed; unconsumed pages are retained")
        return ((nanos + 999_999) / 1_000_000).coerceAtMost(55_000)
    }
    fun cancel() = observation.cancel()
    fun continuing(): ReadBudget = ReadBudget(clock = clock, deadlineNanos = deadlineNanos)
}

data class SessionSummary(val name: String, val cwd: String, val phase: String,
                          val managerState: String, val connectionState: String)
data class SessionPage(val sessions: List<SessionSummary>, val next: Int?)
data class OpenConnection(val id: String, val session: String, val capabilities: Capabilities)

interface GatewayAPI {
    val origin: String
    fun sessions(offset: Int = 0, budget: ReadBudget = ReadBudget()): SessionPage
    fun open(session: String, readOnly: Boolean, openRequestId: String): OpenConnection
    fun bridge(id: String, request: BridgeRequest, budget: ReadBudget? = null): JSONObject
    fun capabilities(id: String, budget: ReadBudget = ReadBudget()): Capabilities
    fun events(id: String, generation: String, cursor: Counter?, budget: ReadBudget, waitMs: Long = 50_000): EventPage
    fun close(id: String)
}

class Gateway(origin: String, private val token: String) : GatewayAPI {
    companion object {
        private val deadlines = ScheduledThreadPoolExecutor(1) { task ->
            Thread(task, "ssmcp-http-deadline").apply { isDaemon = true }
        }.apply { removeOnCancelPolicy = true }
    }
    override val origin: String
    init {
        val uri = URI(origin.trim())
        require(uri.scheme == "https" && !uri.host.isNullOrEmpty() && uri.userInfo == null &&
            uri.query == null && uri.fragment == null && (uri.path.isNullOrEmpty() || uri.path == "/")) {
            "Enter an HTTPS origin, without path, query or credentials"
        }
        require(token.isNotEmpty() && token.length <= 4096 && token.none { it <= ' ' || it == '\u007f' }) {
            "Enter a valid pairing token"
        }
        this.origin = uri.toString().trimEnd('/')
    }

    /** No redirects, cookies, automatic retries, URL credentials, or payload logging. */
    private fun http(method: String, path: String, body: JSONObject? = null,
                     observation: Observation? = null, command: Boolean = false, budget: ReadBudget? = null): JSONObject {
        check(!command || (observation == null && budget == null)) { "A mutation cannot use a read cancellation scope" }
        val cancelledBy = budget?.observation ?: observation
        val remaining = budget?.remainingMillis() ?: 55_000L
        var conn: HttpsURLConnection? = null
        var sent = false
        var deadline: java.util.concurrent.ScheduledFuture<*>? = null
        try {
            if (cancelledBy?.cancelled == true) throw CancellationException()
            conn = (URI(origin + "/api/gui/v1" + path).toURL().openConnection() as HttpsURLConnection)
            conn.instanceFollowRedirects = false
            conn.connectTimeout = minOf(5000, remaining.toInt())
            conn.readTimeout = remaining.toInt()
            conn.useCaches = false
            conn.requestMethod = method
            conn.setRequestProperty("Authorization", "Bearer $token")
            conn.setRequestProperty("Accept", "application/json")
            cancelledBy?.attach(conn)
            val boundedConnection = conn
            deadline = deadlines.schedule({ boundedConnection.disconnect() }, remaining, TimeUnit.MILLISECONDS)
            if (body != null) {
                val bytes = body.toString().toByteArray(Charsets.UTF_8)
                require(bytes.size <= Protocol.MAX_FRAME) { "Request too large" }
                conn.doOutput = true
                conn.setRequestProperty("Content-Type", "application/json")
                conn.setFixedLengthStreamingMode(bytes.size)
                sent = true
                conn.outputStream.use { it.write(bytes) }
            }
            sent = true // DELETE also dispatches a real lease mutation without a body.
            val status = conn.responseCode
            val input = if (status in 200..299) conn.inputStream else conn.errorStream
            val bytes = ByteArrayOutputStream()
            input?.use { stream ->
                val buffer = ByteArray(4096)
                while (true) {
                    val n = stream.read(buffer)
                    if (n < 0) break
                    require(bytes.size() + n <= Protocol.MAX_FRAME) { "Response too large" }
                    bytes.write(buffer, 0, n)
                }
            }
            if (cancelledBy?.cancelled == true) throw CancellationException()
            budget?.remainingMillis()
            val raw = JSONObject(bytes.toString("UTF-8"))
            if (status !in 200..299) {
                val failure = Protocol.error(raw.getJSONObject("error"), wire = false)
                // Trust only an explicit gateway effect-free error, never an unparsed transport error.
                throw failure
            }
            return raw
        } catch (error: BridgeFailure) {
            throw error
        } catch (error: Exception) {
            if (cancelledBy?.cancelled == true) throw CancellationException()
            throw BridgeFailure("transport", "Connection or protocol failure; reconnect or refresh to inspect current state",
                if (command && sent) "may_have_occurred" else "none")
        } finally { deadline?.cancel(false); cancelledBy?.release(); conn?.disconnect() }
    }

    override fun sessions(offset: Int, budget: ReadBudget): SessionPage {
        require(offset >= 0)
        val raw = http("GET", "/sessions?offset=$offset&limit=16", budget = budget)
        val array = raw.getJSONArray("sessions")
        require(array.length() <= 16) { "Too many sessions" }
        val list = (0 until array.length()).map {
            val s = array.getJSONObject(it)
            SessionSummary(s.string("name"), s.string("cwd"), s.optString("phase"),
                s.optString("manager_state"), s.optString("connection_state"))
        }
        val next = if (raw.isNull("next_offset")) null else raw.integer("next_offset", 0..Int.MAX_VALUE)
        require(raw.bool("has_more") == (next != null) && (next == null || next > offset))
        return SessionPage(list, next)
    }

    override fun open(session: String, readOnly: Boolean, openRequestId: String): OpenConnection {
        bridgeId(openRequestId)
        val raw = http("POST", "/connections", JSONObject().put("request_id", openRequestId)
            .put("session", session).put("read_only", readOnly), command = true)
        return Protocol.opened(raw, openRequestId, session, readOnly)
    }

    override fun bridge(id: String, request: BridgeRequest, budget: ReadBudget?): JSONObject {
        try {
            return Protocol.delivered(http("POST", "/connections/${bridgeId(id)}/bridge", request.json,
                command = request.isCommand, budget = budget), request)
        } catch (failure: BridgeFailure) { throw failure }
        catch (error: Exception) {
            throw BridgeFailure("invalid_response", "Invalid bridge response",
                if (request.isCommand) "may_have_occurred" else "none")
        }
    }

    override fun capabilities(id: String, budget: ReadBudget): Capabilities {
        val req = Protocol.capabilities()
        val raw = http("POST", "/connections/${bridgeId(id)}/bridge", req.json, budget = budget)
        Protocol.response(raw, req)
        return Protocol.capabilitiesResponse(raw)
    }

    override fun events(id: String, generation: String, cursor: Counter?, budget: ReadBudget, waitMs: Long): EventPage {
        require(waitMs in 0..55_000)
        val rid = requestId()
        val body = JSONObject().put("request_id", rid).put("generation", bridgeId(generation))
            .put("limit", 32).put("wait_ms", minOf(waitMs, budget.remainingMillis()))
        cursor?.let { body.put("cursor", it.text) }
        return Protocol.eventEnvelope(http("POST", "/connections/${bridgeId(id)}/events", body, budget = budget),
            rid, generation, cursor, 32)
    }

    override fun close(id: String) {
        val raw = http("DELETE", "/connections/${bridgeId(id)}", command = true)
        Protocol.closed(raw)
    }
}

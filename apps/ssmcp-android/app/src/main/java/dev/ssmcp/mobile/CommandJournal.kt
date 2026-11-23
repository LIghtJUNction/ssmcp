package dev.ssmcp.mobile

import android.content.Context
import android.util.Base64
import org.json.JSONArray
import org.json.JSONObject

/** Unknown entries are never evicted to make room for a new side effect. */
object JournalPolicy {
    fun state(entry: JSONObject): String = entry.optString("state").ifEmpty {
        if (entry.optString("outcome").contains("receipt received")) "confirmed" else "unknown"
    }
    fun insert(entries: List<JSONObject>, value: JSONObject, capacity: Int = 20): List<JSONObject> {
        require(entries.none { it.string("id") == value.string("id") }) { "Request ID was already reserved" }
        val next = entries.toMutableList()
        while (next.size >= capacity) {
            val settled = next.indexOfFirst { state(it) in setOf("confirmed", "rejected") }
            check(settled >= 0) { "Receipt storage is full of unknown outcomes; resolve them before submitting more work" }
            next.removeAt(settled)
        }
        next.add(value)
        return next
    }
}

/** Unknown input is kept in private app storage until a verified receipt settles it. */
class CommandJournal(context: Context, name: String = "command_receipts") {
    private val prefs = context.getSharedPreferences(name, Context.MODE_PRIVATE)
    private fun load(): List<JSONObject> {
        val stored = prefs.getString("entries", "[]") ?: "[]"
        check(byteSize(stored) <= 512 * 1024) { "Receipt store exceeds its byte limit" }
        val array = JSONArray(stored)
        check(array.length() <= 20) { "Receipt store exceeds its limit" }
        return (0 until array.length()).map { array.getJSONObject(it).also { entry -> entry.string("id") } }
    }
    private fun save(entries: List<JSONObject>) {
        val array = JSONArray(); entries.forEach { array.put(it) }
        check(byteSize(array.toString()) <= 512 * 1024) { "Receipt store exceeds its byte limit" }
        val previous = prefs.getString("entries", "[]")
        if (!prefs.edit().putString("entries", array.toString()).commit()) {
            // SharedPreferences can change its memory cache even when disk persistence fails.
            prefs.edit().putString("entries", previous).commit()
            error("Cannot preserve request outcome")
        }
    }
    @Synchronized fun reserve(session: String, id: String, operation: String, generation: String = "",
                              connection: String = "", target: String = "", text: String? = null) {
        val value = JSONObject().put("id", id).put("session", session).put("generation", generation)
            .put("operation", operation).put("connection_id", connection).put("target", target).put("state", "unknown")
        if (text != null) {
            require(byteSize(text) <= 16 * 1024) { "Input exceeds its byte limit" }
            value.put("text_base64", Base64.encodeToString(text.toByteArray(Charsets.UTF_8), Base64.NO_WRAP))
        }
        save(JournalPolicy.insert(load(), value))
    }
    @Synchronized fun settle(id: String, state: CommandState) {
        require(state != CommandState.NOT_SENT)
        val values = load()
        val entry = values.singleOrNull { it.string("id") == id } ?: error("Original receipt metadata is missing")
        entry.put("state", state.name.lowercase())
        if (state in setOf(CommandState.CONFIRMED, CommandState.REJECTED)) entry.remove("text_base64")
        save(values)
    }
    @Synchronized fun inputs(): List<UnconfirmedInput> = try {
        load().filter { JournalPolicy.state(it) == "unknown" && it.has("text_base64") }.map { entry ->
            val bytes = Base64.decode(entry.string("text_base64"), Base64.NO_WRAP)
            check(bytes.size <= 16 * 1024)
            UnconfirmedInput(entry.string("id"), entry.optString("operation"), entry.optString("connection_id"),
                entry.optString("generation"), entry.optString("target"), bytes.toString(Charsets.UTF_8))
        }
    } catch (_: Exception) { emptyList() }
    fun summaries(): List<String> = try {
        load().map { entry ->
            "Request · ${entry.optString("session")} · ${entry.optString("operation", entry.optString("command"))}\n" +
                "${entry.string("id")} · connection ${entry.optString("connection_id")} · generation ${entry.optString("generation")} · ${JournalPolicy.state(entry)}"
        }
    } catch (_: Exception) { listOf("Receipt metadata is unreadable. New mutations are blocked to preserve unknown outcomes.") }
}

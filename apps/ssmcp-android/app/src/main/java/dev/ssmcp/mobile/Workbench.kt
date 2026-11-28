package dev.ssmcp.mobile

import org.json.JSONObject
import java.io.InputStream
import java.net.URI
import java.util.Locale

internal class WorkbenchError(val messageResource: Int) : IllegalArgumentException()

/** A gateway origin chosen by the user, or returned by our own CLI. */
class Workbench(val origin: String, val token: String = "") {
    fun owns(url: String): Boolean = runCatching {
        val uri = URI(url)
        uri.rawUserInfo == null && originOf(uri) == origin
    }.getOrDefault(false)

    companion object {
        fun readPairing(input: InputStream): Workbench {
            val bytes = ByteArray(8193)
            var size = 0
            while (size < bytes.size) {
                val read = input.read(bytes, size, bytes.size - size)
                if (read < 0) break
                require(read > 0) { throw WorkbenchError(R.string.pairing_file_read_failed) }
                size += read
            }
            require(size in 1..8192) { throw WorkbenchError(R.string.pairing_file_bad_size) }
            val receipt = JSONObject(String(bytes, 0, size, Charsets.UTF_8))
            val token = receipt.opt("token") as? String
            val address = receipt.opt("url") as? String
            require(receipt.opt("schema") == "ssmcp-gui-pairing-v1" && address != null
                && token != null && Regex("[0-9a-f]{64}").matches(token)) { throw WorkbenchError(R.string.pairing_file_invalid) }
            return at(address, token)
        }

        fun at(address: String, token: String = ""): Workbench {
            val uri = URI(address.trim())
            require(uri.rawPath in listOf("", "/") && uri.rawQuery == null && uri.rawFragment == null) {
                throw WorkbenchError(R.string.origin_without_pairing_data)
            }
            return Workbench(originOf(uri), token)
        }

        private fun originOf(uri: URI): String {
            val scheme = uri.scheme?.lowercase(Locale.ROOT)
            val host = uri.host?.lowercase(Locale.ROOT)
            require(scheme in setOf("http", "https") && host != null && uri.rawUserInfo == null &&
                !host.contains('%') && uri.port in -1..65535 && uri.port != 0) { throw WorkbenchError(R.string.invalid_workbench_address) }
            require(scheme == "https" || host in setOf("127.0.0.1", "localhost", "[::1]")) {
                throw WorkbenchError(R.string.https_required)
            }
            val port = uri.port
            return "$scheme://$host" + if (port == -1 || (scheme == "https" && port == 443) ||
                (scheme == "http" && port == 80)) "" else ":$port"
        }
    }
}

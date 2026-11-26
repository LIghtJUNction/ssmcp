package dev.ssmcp.mobile

import org.json.JSONObject
import java.io.InputStream
import java.net.URI
import java.util.Locale

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
                require(read > 0) { "无法读取配对文件。" }
                size += read
            }
            require(size in 1..8192) { "配对文件大小无效。" }
            val receipt = JSONObject(String(bytes, 0, size, Charsets.UTF_8))
            val token = receipt.opt("token") as? String
            val address = receipt.opt("url") as? String
            require(receipt.opt("schema") == "ssmcp-gui-pairing-v1" && address != null
                && token != null && Regex("[0-9a-f]{64}").matches(token)) { "请选择网关生成的配对文件。" }
            return at(address, token)
        }

        fun at(address: String, token: String = ""): Workbench {
            val uri = URI(address.trim())
            require(uri.rawPath in listOf("", "/") && uri.rawQuery == null && uri.rawFragment == null) {
                "请输入工作台的源地址，配对信息请使用网关生成的文件。"
            }
            return Workbench(originOf(uri), token)
        }

        private fun originOf(uri: URI): String {
            val scheme = uri.scheme?.lowercase(Locale.ROOT)
            val host = uri.host?.lowercase(Locale.ROOT)
            require(scheme in setOf("http", "https") && host != null && uri.rawUserInfo == null &&
                !host.contains('%') && uri.port in -1..65535 && uri.port != 0) { "工作台地址无效。" }
            require(scheme == "https" || host in setOf("127.0.0.1", "localhost", "[::1]")) {
                "远端工作台需要 HTTPS。模拟器访问开发机可用 adb reverse 和 127.0.0.1。"
            }
            val port = uri.port
            return "$scheme://$host" + if (port == -1 || (scheme == "https" && port == 443) ||
                (scheme == "http" && port == 80)) "" else ":$port"
        }
    }
}

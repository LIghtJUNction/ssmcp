package dev.ssmcp.mobile

import java.net.URI
import java.util.Locale

/** A gateway origin chosen by the user, or returned by our own CLI. */
class Workbench(val origin: String, val token: String = "") {
    fun owns(url: String): Boolean = runCatching {
        val uri = URI(url)
        uri.rawUserInfo == null && originOf(uri) == origin
    }.getOrDefault(false)

    companion object {
        fun at(address: String, token: String = ""): Workbench {
            val uri = URI(address.trim())
            require(uri.rawPath in listOf("", "/") && uri.rawQuery == null && uri.rawFragment == null) {
                "请输入工作台的源地址，配对令牌请单独填写。"
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

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

data class StartCommand(val text: String, val sessionArgs: List<String>) {
    companion object {
        const val DEFAULT = "ssmcp gui serve"

        fun parse(text: String): StartCommand {
            val words = text.trim().split(Regex("\\s+"))
            if (words == listOf("ssmcp", "gui", "serve")) return StartCommand(DEFAULT, emptyList())
            require(words.size in setOf(2, 4) && words[0] == "ssmcp") { "可用命令：ssmcp gui serve 或 ssmcp 用户名@服务器 [--name 名称]。" }
            val target = words[1]
            val split = target.split('@')
            require(split.size == 2 && Regex("[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}").matches(split[0])) { "请填写 用户名@服务器。" }
            val relay = URI(if ("://" in split[1]) split[1] else "https://" + split[1])
            require(relay.scheme in setOf("https", "http") && relay.host != null && relay.rawUserInfo == null &&
                relay.rawPath in listOf("", "/") && relay.rawQuery == null && relay.rawFragment == null &&
                relay.port in -1..65535 && relay.port != 0) { "中继地址无效。" }
            require(relay.scheme != "http" || relay.host == "[::1]" ||
                Regex("127(?:\\.[0-9]{1,3}){3}").matches(relay.host) &&
                    relay.host.split('.').all { it.toInt() in 0..255 }) { "中继的 HTTP 地址必须是回环 IP；远端中继请使用 HTTPS。" }
            if (words.size == 4) require(words[2] == "--name" && Regex("[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}").matches(words[3])) {
                "可选参数为 --name 名称。"
            }
            return StartCommand(words.joinToString(" "), words.drop(1) + listOf("--watchdog", "--shell", "/system/bin/sh"))
        }
    }
}

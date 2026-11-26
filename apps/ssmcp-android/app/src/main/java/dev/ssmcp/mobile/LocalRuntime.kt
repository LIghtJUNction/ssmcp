package dev.ssmcp.mobile

import android.content.Context
import android.os.SystemClock
import android.system.Os
import org.json.JSONObject
import java.io.File
import java.util.UUID
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/** All start/stop transitions run on one executor. Retain the ownership pipes across Activity recreation. */
internal object LocalRuntime {
    val executor = Executors.newSingleThreadExecutor()
    private var gateway: Process? = null
    private var pairing: File? = null
    private var page: Workbench? = null

    fun open(context: Context): Workbench {
        check(Os.geteuid() == context.applicationInfo.uid && Os.geteuid() != 0) { "本机工作台需要应用自身的身份。" }
        if (gateway?.isAlive == true) return checkNotNull(page)
        stop()
        val binary = File(context.applicationInfo.nativeLibraryDir, "libssmcp.so")
        check(binary.canExecute()) { "此 APK 缺少可执行的本机 CLI。请安装包含当前 ABI 的版本，或连接已有网关。" }
        fun directory(name: String) = File(context.filesDir, name).also {
            check(it.isDirectory || it.mkdir()) { "无法创建应用私有目录。" }
            Os.chmod(it.path, 448) // 0700, also required by the CLI.
        }
        val home = directory("home")
        val sessions = directory("s")
        val cwd = directory("workspace")
        val file = File(sessions, "gui-${UUID.randomUUID()}.json")
        try {
            pairing = file
            val process = ProcessBuilder(binary.path, "__android_app", "gui", "serve",
                "--session-mode", "owned", "--listen", "127.0.0.1:0", "--pairing-file", file.path)
                .directory(cwd).redirectErrorStream(true).redirectOutput(File("/dev/null")).apply {
                    environment().putAll(mapOf("HOME" to home.path, "SSMCP_SESSION_HOME" to sessions.path,
                        "TMPDIR" to context.cacheDir.path))
                }.start()
            gateway = process
            val deadline = SystemClock.elapsedRealtime() + 10_000
            while (!file.isFile && process.isAlive && SystemClock.elapsedRealtime() < deadline) Thread.sleep(50)
            check(process.isAlive && file.length() in 1L..8192L) { "本机工作台未就绪。请检查安装包后重试。" }
            val receipt = JSONObject(file.readText())
            val target = Workbench.at(receipt.getString("url"), receipt.getString("token"))
            check(receipt.getString("schema") == "ssmcp-gui-pairing-v1" &&
                target.origin.startsWith("http://127.0.0.1:") && Regex("[0-9a-f]{64}").matches(target.token)) { "本机工作台配对信息无效。" }
            page = target
            return target
        } catch (error: Exception) {
            stop()
            throw error
        }
    }

    fun stop() {
        gateway?.let { process ->
            process.outputStream.close() // Native __android_app observes HUP and shuts down.
            if (!process.waitFor(2, TimeUnit.SECONDS)) {
                process.destroy()
                if (!process.waitFor(2, TimeUnit.SECONDS)) {
                    process.destroyForcibly()
                    check(process.waitFor(2, TimeUnit.SECONDS)) { "原来的本机进程仍在退出，请稍后重试。" }
                }
            }
        }
        gateway = null
        pairing?.delete()
        pairing = null
        page = null
    }
}

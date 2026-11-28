package dev.ssmcp.mobile

import android.content.Context
import android.os.SystemClock
import android.system.Os
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
        check(Os.geteuid() == context.applicationInfo.uid && Os.geteuid() != 0) { context.getString(R.string.local_app_identity_required) }
        if (gateway?.isAlive == true) return checkNotNull(page)
        stop(context)
        val binary = File(context.applicationInfo.nativeLibraryDir, "libssmcp.so")
        check(binary.canExecute()) { context.getString(R.string.native_cli_missing) }
        fun directory(name: String) = File(context.filesDir, name).also {
            check(it.isDirectory || it.mkdir()) { context.getString(R.string.private_directory_failed) }
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
            check(process.isAlive && file.length() in 1L..8192L) { context.getString(R.string.local_workbench_not_ready) }
            val target = file.inputStream().use { Workbench.readPairing(it) }
            check(target.origin.startsWith("http://127.0.0.1:")) { context.getString(R.string.invalid_local_pairing_address) }
            page = target
            return target
        } catch (error: Exception) {
            stop(context)
            throw error
        }
    }

    fun stop(context: Context) {
        gateway?.let { process ->
            process.outputStream.close() // Native __android_app observes HUP and shuts down.
            if (!process.waitFor(2, TimeUnit.SECONDS)) {
                process.destroy()
                if (!process.waitFor(2, TimeUnit.SECONDS)) {
                    process.destroyForcibly()
                    check(process.waitFor(2, TimeUnit.SECONDS)) { context.getString(R.string.local_process_stopping) }
                }
            }
        }
        gateway = null
        pairing?.delete()
        pairing = null
        page = null
    }
}

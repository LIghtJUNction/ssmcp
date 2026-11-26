package dev.ssmcp.mobile

import android.app.Activity
import android.app.Instrumentation
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.os.SystemClock
import android.view.View
import android.view.ViewGroup
import android.webkit.WebView
import java.io.File
import java.net.ServerSocket
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/** Uses the shipped CLI, the shipped HTML/modules, the platform WebView and a disconnected relay target. */
class ProtocolInstrumentation : Instrumentation() {
    override fun onCreate(arguments: Bundle?) { super.onCreate(arguments); start() }

    override fun onStart() {
        val result = Bundle()
        try {
            val endpoint = Workbench.at("https://example.test:443/")
            check(endpoint.origin == "https://example.test")
            check(!endpoint.owns("https://example.test.attacker.test/"))
            check(!endpoint.owns("https://example.test@elsewhere.test/"))
            for (bad in listOf("file:///etc/", "content://provider/", "http://10.0.2.2:8787",
                "https://example.test/#token", "https://example.test/workspace", "https://user@example.test")) {
                check(runCatching { Workbench.at(bad) }.isFailure) { "Unexpected workbench origin" }
            }
            check(Workbench.at("http://127.0.0.1:8787").owns("http://127.0.0.1:8787/app.mjs"))
            check(runCatching { StartCommand.parse("ssmcp sample@example.test ; exit") }.isFailure)
            val info = targetContext.packageManager.getPackageInfo(targetContext.packageName, 0)
            val binary = File(targetContext.applicationInfo.nativeLibraryDir, "libssmcp.so")
            val version = ProcessBuilder(binary.path, "--version").start()
            try {
                check(version.waitFor(5, TimeUnit.SECONDS) && version.exitValue() == 0 &&
                    version.inputStream.bufferedReader().use { it.readLine() } == "ssmcp ${info.versionName}" &&
                    info.versionCode > 1) { "The packaged CLI and application release versions disagree" }
            } finally { version.destroy() }
            exerciseWorkbench()
            result.putString("stream", "SSMCP Android WebView checks passed: packaged CLI, loopback pairing, rendered sessions, control lease, observe, release, origin boundary.\n")
            finish(Activity.RESULT_OK, result)
        } catch (error: Throwable) {
            result.putString("stream", "SSMCP check failed: ${error.javaClass.simpleName}: ${error.message}\n")
            finish(Activity.RESULT_CANCELED, result)
        }
    }

    private fun webView(view: View): WebView? {
        if (view is WebView) return view
        if (view is ViewGroup) for (i in 0 until view.childCount) webView(view.getChildAt(i))?.let { return it }
        return null
    }

    private fun js(activity: Activity, expression: String): String? {
        val done = CountDownLatch(1)
        val result = AtomicReference<String?>()
        runOnMainSync {
            val web = webView(activity.window.decorView)
            if (web == null) done.countDown()
            else web.evaluateJavascript(expression) { result.set(it); done.countDown() }
        }
        check(done.await(4, TimeUnit.SECONDS)) { "WebView did not evaluate JavaScript" }
        return result.get()
    }

    private fun waitFor(activity: Activity, stage: String, expression: String) {
        val deadline = SystemClock.elapsedRealtime() + 20_000
        while (SystemClock.elapsedRealtime() < deadline) {
            if (js(activity, expression) == "true") return
            Thread.sleep(100)
        }
        error("Timed out waiting for $stage")
    }

    private fun exerciseWorkbench() {
        // Never displace an existing user's command, workspace or credentials on a developer phone.
        val prefs = targetContext.getSharedPreferences("workbench", Context.MODE_PRIVATE)
        check(prefs.all.isEmpty() && !targetContext.getSharedPreferences("pairing", Context.MODE_PRIVATE).contains("token")) {
            "Instrumentation requires a fresh app installation in an isolated Android user"
        }
        val unusedPort = ServerSocket(0, 1, java.net.InetAddress.getByName("127.0.0.1")).use { it.localPort }
        val name = "webview-fixture"
        check(prefs.edit().putString("mode", "local")
            .putString("command", "ssmcp androidfixture@http://127.0.0.1:$unusedPort --name $name").commit())
        var activity: Activity? = null
        try {
            val opened = startActivitySync(Intent(targetContext, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            activity = opened
            waitFor(opened, "automatic same-origin pairing", """
                document.title.includes('工作台') && document.querySelector('#auth-status')?.textContent === '已设置认证'
                    && location.protocol === 'http:' && location.hostname === '127.0.0.1' && !location.hash
            """.trimIndent())
            waitFor(opened, "the native foreground session", """
                (() => {
                  const row = [...document.querySelectorAll('.session-item')].find(x => x.querySelector('strong').textContent === '$name');
                  if (row) { row.click(); return true; }
                  document.getElementById('refresh-sessions').click();
                  return false;
                })()
            """.trimIndent())
            waitFor(opened, "explicit session selection", "!document.getElementById('connect-strip').hidden")
            js(opened, "document.getElementById('connection-mode').value='control'; document.getElementById('connect-button').click()")
            waitFor(opened, "native control lease", "document.getElementById('connection-label').textContent === 'SSMCP / CONTROL'")
            waitFor(opened, "event observation", "document.getElementById('observe-button').textContent === '暂停观察'")
            js(opened, "document.getElementById('observe-button').click()")
            waitFor(opened, "paused observation", "document.getElementById('observe-button').textContent === '继续观察'")
            js(opened, "document.getElementById('disconnect-button').click()")
            waitFor(opened, "verified release", """
                document.getElementById('connected-actions').hidden && document.getElementById('error-banner').hidden
            """.trimIndent())
        } finally {
            activity?.let { runOnMainSync { it.finish() } }
            LocalRuntime.executor.submit { LocalRuntime.stop() }.get(10, TimeUnit.SECONDS)
            prefs.edit().clear().commit()
        }
    }
}

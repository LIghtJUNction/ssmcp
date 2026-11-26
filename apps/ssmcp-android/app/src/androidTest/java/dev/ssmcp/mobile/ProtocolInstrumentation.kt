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
import java.net.ConnectException
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketTimeoutException
import java.net.URI
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
            val fixtureToken = "a1".repeat(32)
            val receipt = """{"schema":"ssmcp-gui-pairing-v1","url":"https://example.test:443/","token":"$fixtureToken"}"""
            val pairing = Workbench.readPairing(receipt.byteInputStream())
            check(pairing.origin == endpoint.origin && pairing.token == fixtureToken)
            for (invalid in listOf(receipt.replace("gui-pairing", "device-pairing"),
                receipt.replace("test:443/", "test:443/setup"), receipt.replace(fixtureToken, "short"), " ".repeat(8193))) {
                check(runCatching { Workbench.readPairing(invalid.byteInputStream()) }.isFailure)
            }
            check(android.system.Os.geteuid() == targetContext.applicationInfo.uid && android.system.Os.geteuid() != 0) {
                "Instrumentation must run under the non-root application UID"
            }
            val info = targetContext.packageManager.getPackageInfo(targetContext.packageName, 0)
            val binary = File(targetContext.applicationInfo.nativeLibraryDir, "libssmcp.so")
            val version = ProcessBuilder(binary.path, "--version").start()
            try {
                check(version.waitFor(5, TimeUnit.SECONDS) && version.exitValue() == 0 &&
                    version.inputStream.bufferedReader().use { it.readLine() } == "ssmcp ${info.versionName}" &&
                    info.versionCode > 1) { "The packaged CLI and application release versions disagree" }
            } finally { version.destroy() }
            exerciseWorkbench()
            result.putString("stream", "SSMCP Android WebView checks passed: packaged CLI, app UID, loopback pairing, legacy launch migration, recreation, explicit owned session, read-only observation, release and gateway exit.\n")
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
            if (web == null || !web.isShown) done.countDown()
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

    private fun awaitSetup(activity: Activity) = waitFor(activity, "paired local creation form", """
        document.querySelector('#launch-setup')?.hidden === false
            && document.querySelector('#workbench-shell')?.hidden === true
            && document.documentElement.dataset.ssmcpPairingReady === 'paired'
            && !document.querySelector('dialog[open]')
            && ![...document.querySelectorAll('input[type=password],input[type=file],#pairing-token')]
                .some(input => input.getClientRects().length)
            && document.querySelector('#launch-relay')?.getClientRects().length > 0
            && !document.querySelector('#launch-name')?.disabled
            && !document.querySelector('#launch-connect')?.disabled
            && location.protocol === 'http:' && location.hostname === '127.0.0.1' && !location.hash && !location.search
    """.trimIndent())

    private fun exerciseWorkbench() {
        val prefs = targetContext.getSharedPreferences("workbench", Context.MODE_PRIVATE)
        check(prefs.all.isEmpty() && !targetContext.getSharedPreferences("pairing", Context.MODE_PRIVATE).contains("token")) {
            "Instrumentation requires a fresh app installation in an isolated Android user"
        }
        val loopback = InetAddress.getByName("127.0.0.1")
        ServerSocket(0, 1, loopback).use { legacyRelay ->
            val legacy = "ssmcp legacy@http://127.0.0.1:${legacyRelay.localPort} --name legacy"
            check(prefs.edit().putString("mode", "local").putString("command", legacy).commit())
            var activity: Activity? = null
            try {
                val opened = startActivitySync(Intent(targetContext, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                activity = opened
                awaitSetup(opened)
                check(!prefs.contains("command")) { "The legacy CLI command was retained" }
                val origin = URI(js(opened, "location.origin")!!.trim('"'))
                val monitor = addMonitor(MainActivity::class.java.name, null, false)
                val recreated = try {
                    runOnMainSync { opened.recreate() }
                    checkNotNull(waitForMonitorWithTimeout(monitor, 10_000)) { "The activity was not recreated" }
                } finally {
                    removeMonitor(monitor)
                }
                activity = recreated
                awaitSetup(recreated)
                check(js(recreated, "location.origin") == "\"$origin\"") { "Recreation replaced the app-owned gateway" }
                legacyRelay.soTimeout = 500
                try {
                    legacyRelay.accept().use { error("The app attempted to replay the legacy relay connection") }
                } catch (_: SocketTimeoutException) { /* No connection arrived. */ }

                val unusedPort = ServerSocket(0, 1, loopback).use { it.localPort }
                js(recreated, """
                    document.getElementById('launch-name').value = 'androidfixture';
                    const relay = document.getElementById('launch-relay');
                    relay.value = 'http://127.0.0.1:$unusedPort';
                    relay.dispatchEvent(new Event('input', { bubbles: true }));
                    document.getElementById('launch-connect').click()
                """.trimIndent())
                waitFor(recreated, "the explicitly created owned session", """
                    !document.getElementById('workbench-shell').hidden
                        && !document.getElementById('connected-actions').hidden
                        && /^live:[0-9a-f]{32}$/.test(document.querySelector('#session-tabs [aria-selected="true"]')?.title)
                        && document.getElementById('connection-label').textContent === 'SSMCP / OBSERVE'
                """.trimIndent())
                waitFor(recreated, "event observation", "document.getElementById('observe-button').textContent === '暂停观察'")
                js(recreated, "document.getElementById('observe-button').click()")
                waitFor(recreated, "paused observation", "document.getElementById('observe-button').textContent === '继续观察'")
                js(recreated, "document.getElementById('disconnect-button').click()")
                waitFor(recreated, "verified release", """
                    document.getElementById('connected-actions').hidden && document.getElementById('error-banner').hidden
                """.trimIndent())
                runOnMainSync { recreated.finishAndRemoveTask() }
                activity = null
                val deadline = SystemClock.elapsedRealtime() + 10_000
                while (true) {
                    try { Socket().use { it.connect(InetSocketAddress(loopback, origin.port), 500) } }
                    catch (_: ConnectException) { break }
                    check(SystemClock.elapsedRealtime() < deadline) { "The app-owned gateway outlived its finished activity" }
                    Thread.sleep(100)
                }
            } finally {
                activity?.let { runOnMainSync { it.finishAndRemoveTask() } }
                LocalRuntime.executor.submit { LocalRuntime.stop() }.get(10, TimeUnit.SECONDS)
                prefs.edit().clear().commit()
            }
        }
    }
}

package dev.ssmcp.mobile

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.text.InputType
import android.util.Base64
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.webkit.WebView
import android.widget.*
import org.json.JSONArray

class MainActivity : Activity() {
    private val prefs by lazy { getSharedPreferences("workbench", Context.MODE_PRIVATE) }
    private val store by lazy { TokenStore(this) }
    private lateinit var statusLabel: TextView
    private lateinit var frame: FrameLayout
    private lateinit var settingsButton: Button
    private var web: WorkbenchView? = null
    private var receivePairingFile: ((Result<Workbench>) -> Unit)? = null
    private var generation = 0

    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        WebView.setWebContentsDebuggingEnabled(false)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setOnApplyWindowInsetsListener { view, insets ->
                view.setPadding(insets.systemWindowInsetLeft, insets.systemWindowInsetTop,
                    insets.systemWindowInsetRight, insets.systemWindowInsetBottom)
                insets
            }
        }
        val bar = LinearLayout(this).apply {
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(16), 0, dp(8), 0)
        }
        statusLabel = TextView(this).apply { textSize = 15f; maxLines = 2 }
        bar.addView(statusLabel, LinearLayout.LayoutParams(0, -2, 1f))
        settingsButton = Button(this, null, android.R.attr.borderlessButtonStyle).apply {
            text = getString(R.string.advanced_settings)
            setOnClickListener { configure() }
        }
        bar.addView(settingsButton)
        root.addView(bar)
        frame = FrameLayout(this)
        root.addView(frame, LinearLayout.LayoutParams(-1, 0, 1f))
        setContentView(root)
        try {
            when (prefs.getString("mode", "local")) {
                "local" -> openLocal()
                "remote" -> {
                    val previous = store.load()
                    openRemote(prefs.getString("origin", previous?.first) ?: "", null, previous)
                }
                else -> statusLabel.text = getString(R.string.workbench_stopped)
            }
        } catch (_: Exception) { statusLabel.text = getString(R.string.restore_settings_failed) }
    }

    private fun describeError(error: Throwable, fallback: Int): String =
        if (error is WorkbenchError) getString(error.messageResource) else error.message ?: getString(fallback)

    private fun dp(value: Int) = (resources.displayMetrics.density * value).toInt()

    private fun replacePage(target: Workbench) {
        removePage()
        statusLabel.text = if (prefs.getString("mode", "local") == "local") "SSMCP" else target.origin
        web = WorkbenchView(this, target) { statusLabel.text = it }.also { frame.addView(it) }
    }

    private fun removePage() {
        web?.let { frame.removeView(it); it.dispose() }
        web = null
    }

    @Deprecated("Platform result callback")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != 71) return
        val receiver = receivePairingFile ?: return
        if (resultCode != RESULT_OK) { receivePairingFile = null; return }
        val uri = data?.data
        LocalRuntime.executor.execute {
            val result = runCatching {
                require(uri?.scheme == "content") { getString(R.string.choose_device_pairing_file) }
                requireNotNull(contentResolver.openInputStream(requireNotNull(uri))) { getString(R.string.open_pairing_file_failed) }
                    .use { Workbench.readPairing(it) }
            }
            runOnUiThread {
                if (!isDestroyed && receivePairingFile === receiver) {
                    receivePairingFile = null
                    receiver(result)
                }
            }
        }
    }

    private fun transition(action: () -> Workbench?) {
        val attempt = ++generation
        settingsButton.isEnabled = false
        statusLabel.text = getString(R.string.opening_workbench)
        LocalRuntime.executor.execute {
            val result = runCatching(action)
            runOnUiThread {
                if (attempt != generation || isDestroyed) return@runOnUiThread
                settingsButton.isEnabled = true
                result.fold({ page ->
                    if (page != null) replacePage(page)
                    else { removePage(); statusLabel.text = getString(R.string.workbench_stopped) }
                }, { statusLabel.text = describeError(it, R.string.workbench_start_failed) })
            }
        }
    }

    private fun openLocal() {
        transition {
            val page = LocalRuntime.open(applicationContext)
            prefs.edit().putString("mode", "local").remove("command").apply()
            page
        }
    }

    private fun openRemote(address: String, imported: Workbench?, saved: Pair<String, String>?) {
        val origin = Workbench.at(address).origin
        require(imported == null || imported.origin == origin) { getString(R.string.pairing_origin_mismatch) }
        val token = imported?.token ?: saved?.takeIf { Workbench.at(it.first).origin == origin }?.second
        require(token != null && Regex("[0-9a-f]{64}").matches(token)) { getString(R.string.choose_gateway_pairing_file) }
        if (imported != null) store.save(origin, token)
        transition {
            LocalRuntime.stop(applicationContext)
            prefs.edit().putString("mode", "remote").putString("origin", origin).apply()
            Workbench(origin, token)
        }
    }

    private fun configure() {
        // An open control lease belongs to the web client. Let its regular disconnect path
        // verify release and preserve any unknown result before replacing the document.
        val page = web
        if (page == null) { showSettings(); return }
        page.evaluateJavascript("""(() => {
            const launch = document.getElementById('launch-setup');
            if (launch && !launch.hidden && (document.getElementById('launch-name')?.disabled
                || document.getElementById('launch-connect')?.disabled)) return 'creating';
            const actions = document.getElementById('connected-actions');
            const connect = document.getElementById('connect-button');
            const strip = document.getElementById('connect-strip');
            return (actions && !actions.hidden) || (strip && !strip.hidden && connect && connect.disabled)
                ? 'connected' : 'ready';
        })()""") { state ->
            if (isDestroyed || web !== page) return@evaluateJavascript
            when (state) {
                "\"creating\"" -> statusLabel.text = getString(R.string.verify_creation_before_settings)
                "\"connected\"" -> statusLabel.text = getString(R.string.disconnect_before_settings)
                "\"ready\"" -> showSettings()
                else -> statusLabel.text = getString(R.string.checking_workbench)
            }
        }
    }

    private fun showSettings() {
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(8), dp(24), dp(4))
        }
        val modes = RadioGroup(this).apply { orientation = RadioGroup.VERTICAL }
        val local = RadioButton(this).apply { id = View.generateViewId(); text = getString(R.string.this_device) }
        val remote = RadioButton(this).apply { id = View.generateViewId(); text = getString(R.string.existing_gateway) }
        modes.addView(local); modes.addView(remote); layout.addView(modes)
        val previous = runCatching { store.load() }.getOrNull()
        val address = EditText(this).apply {
            hint = "https://gui.example"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            setSingleLine()
            setText(prefs.getString("origin", previous?.first) ?: "")
            isSaveEnabled = false
            layout.addView(this)
        }
        val file = Button(this).apply { text = getString(R.string.select_pairing_file); layout.addView(this) }
        var picked: Workbench? = null
        val hint = TextView(this).apply { setPadding(0, dp(8), 0, dp(4)); layout.addView(this) }
        val forget = Button(this, null, android.R.attr.borderlessButtonStyle).apply {
            text = getString(R.string.delete_saved_pairing)
            layout.addView(this)
        }
        file.setOnClickListener {
            receivePairingFile = { result ->
                result.onSuccess { pairing ->
                    val text = address.text.toString().trim()
                    if (text.isNotEmpty() && runCatching { Workbench.at(text).origin }.getOrNull() != pairing.origin) {
                        hint.text = getString(R.string.pairing_origin_mismatch)
                    } else {
                        picked = pairing
                        address.setText(pairing.origin)
                        hint.text = getString(R.string.pairing_file_ready)
                    }
                }.onFailure { hint.text = getString(R.string.pairing_file_unreadable) }
            }
            try {
                startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                    addCategory(Intent.CATEGORY_OPENABLE)
                    type = "application/json"
                }, 71)
            } catch (_: Exception) {
                receivePairingFile = null
                hint.text = getString(R.string.file_picker_unavailable)
            }
        }
        var forgotten = false
        forget.setOnClickListener {
            runCatching { store.delete() }.onSuccess { forgotten = true; picked = null; forget.visibility = View.GONE }
                .onFailure { hint.text = getString(R.string.delete_pairing_failed) }
        }
        modes.setOnCheckedChangeListener { _, checked ->
            val onDevice = checked == local.id
            address.visibility = if (onDevice) View.GONE else View.VISIBLE
            file.visibility = address.visibility
            forget.visibility = if (!onDevice && previous != null && !forgotten) View.VISIBLE else View.GONE
            hint.text = if (onDevice) getString(R.string.local_workbench_hint)
                else getString(R.string.remote_workbench_hint)
        }
        modes.check(if (prefs.getString("mode", "local") == "remote") remote.id else local.id)
        val dialog = AlertDialog.Builder(this).setTitle(R.string.advanced_settings).setView(ScrollView(this).apply { addView(layout) })
            .setPositiveButton(R.string.open_workbench, null).setNegativeButton(R.string.cancel, null)
            .setNeutralButton(R.string.stop_local, null).create()
        if (getSharedPreferences("command_receipts", MODE_PRIVATE).contains("entries")) {
            layout.addView(Button(this, null, android.R.attr.borderlessButtonStyle).apply {
                text = getString(R.string.show_legacy_operations); setOnClickListener { legacyReceipts() }
            })
        }
        dialog.setOnShowListener {
            dialog.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener {
                try {
                    if (local.isChecked) openLocal()
                    else openRemote(address.text.toString(), picked, if (forgotten) null else previous)
                    dialog.dismiss()
                } catch (e: Exception) { hint.text = describeError(e, R.string.invalid_startup_settings) }
            }
            dialog.getButton(AlertDialog.BUTTON_NEUTRAL).apply {
                visibility = if (prefs.getString("mode", "local") == "local") View.VISIBLE else View.GONE
                setOnClickListener {
                    dialog.dismiss()
                    transition { LocalRuntime.stop(applicationContext); prefs.edit().putString("mode", "stopped").apply(); null }
                }
            }
        }
        dialog.setOnDismissListener { receivePairingFile = null }
        dialog.show()
    }

    private fun legacyReceipts() {
        val text = runCatching {
            val stored = getSharedPreferences("command_receipts", MODE_PRIVATE).getString("entries", "[]")
            val array = JSONArray(stored)
            (0 until array.length()).joinToString("\n\n") {
                val entry = array.getJSONObject(it)
                val body = entry.optString("text_base64").takeIf(String::isNotEmpty)?.let { encoded ->
                    "\n" + Base64.decode(encoded, Base64.DEFAULT).toString(Charsets.UTF_8)
                }.orEmpty()
                val state = entry.optString("state").ifEmpty { entry.optString("outcome", "unknown") }
                "${entry.optString("operation", entry.optString("command"))} · $state\n${entry.getString("id")}$body"
            }
        }.getOrDefault(getString(R.string.legacy_operations_unreadable))
        AlertDialog.Builder(this).setTitle(R.string.legacy_operations).setMessage(text.ifEmpty { getString(R.string.no_operations) })
            .setPositiveButton(R.string.close, null).show()
    }

    override fun onPause() { web?.onPause(); super.onPause() }
    override fun onResume() { super.onResume(); web?.onResume() }
    override fun onDestroy() {
        ++generation
        receivePairingFile = null
        removePage()
        if (isFinishing) LocalRuntime.executor.submit { LocalRuntime.stop(applicationContext) }
        super.onDestroy()
    }
}

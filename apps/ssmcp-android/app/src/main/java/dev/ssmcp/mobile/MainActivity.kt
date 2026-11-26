package dev.ssmcp.mobile

import android.app.Activity
import android.app.AlertDialog
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.text.InputType
import android.util.Base64
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.webkit.ValueCallback
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
    private var pendingFile: ValueCallback<Array<Uri>>? = null
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
            text = "启动设置"
            setOnClickListener { configure() }
        }
        bar.addView(settingsButton)
        root.addView(bar)
        frame = FrameLayout(this)
        root.addView(frame, LinearLayout.LayoutParams(-1, 0, 1f))
        setContentView(root)
        try {
            val previous = store.load()
            val mode = prefs.getString("mode", if (previous != null) "remote" else "local")
            when (mode) {
                "local" -> openLocal(prefs.getString("command", StartCommand.DEFAULT)!!)
                "remote" -> openRemote(prefs.getString("origin", previous?.first) ?: "", "", previous)
                else -> statusLabel.text = "工作台已停止"
            }
        } catch (_: Exception) { statusLabel.text = "无法恢复启动设置，请重新配置。" }
    }

    private fun dp(value: Int) = (resources.displayMetrics.density * value).toInt()

    private fun replacePage(target: Workbench) {
        removePage()
        statusLabel.text = target.origin
        web = WorkbenchView(this, target, { callback ->
            pendingFile?.onReceiveValue(null)
            pendingFile = callback
            try {
                startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                    addCategory(Intent.CATEGORY_OPENABLE)
                    type = "application/json"
                }, 71)
            } catch (_: Exception) {
                pendingFile = null
                callback.onReceiveValue(null)
                statusLabel.text = "设备没有文件选择器，可直接填写配对令牌。"
            }
        }, { statusLabel.text = it }).also { frame.addView(it) }
    }

    private fun removePage() {
        pendingFile?.onReceiveValue(null)
        pendingFile = null
        web?.let { frame.removeView(it); it.dispose() }
        web = null
    }

    @Deprecated("Platform result callback")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != 71) return
        val uri = data?.data?.takeIf { resultCode == RESULT_OK && it.scheme == "content" }
        pendingFile?.onReceiveValue(uri?.let { arrayOf(it) })
        pendingFile = null
    }

    private fun transition(action: () -> Workbench?) {
        val attempt = ++generation
        settingsButton.isEnabled = false
        statusLabel.text = "正在打开工作台…"
        LocalRuntime.executor.execute {
            val result = runCatching(action)
            runOnUiThread {
                if (attempt != generation || isDestroyed) return@runOnUiThread
                settingsButton.isEnabled = true
                result.fold({ page ->
                    if (page != null) replacePage(page)
                    else { removePage(); statusLabel.text = "工作台已停止" }
                }, { statusLabel.text = it.message ?: "工作台启动失败，请检查启动设置。" })
            }
        }
    }

    private fun openLocal(text: String) {
        val command = StartCommand.parse(text)
        transition {
            val page = LocalRuntime.open(applicationContext, command)
            prefs.edit().putString("mode", "local").putString("command", command.text).apply()
            page
        }
    }

    private fun openRemote(address: String, entered: String, saved: Pair<String, String>?) {
        val origin = Workbench.at(address).origin
        val token = entered.ifEmpty { saved?.takeIf { Workbench.at(it.first).origin == origin }?.second ?: "" }
        if (entered.isNotEmpty()) store.save(origin, entered)
        transition {
            LocalRuntime.stop()
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
            const actions = document.getElementById('connected-actions');
            const connect = document.getElementById('connect-button');
            const strip = document.getElementById('connect-strip');
            return Boolean((actions && !actions.hidden) || (strip && !strip.hidden && connect && connect.disabled));
        })()""") {
            if (isDestroyed) return@evaluateJavascript
            if (it == "true") statusLabel.text = "请先在工作台断开当前会话，再更改启动设置。"
            else showSettings()
        }
    }

    private fun showSettings() {
        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(8), dp(24), dp(4))
        }
        val modes = RadioGroup(this).apply { orientation = RadioGroup.HORIZONTAL }
        val local = RadioButton(this).apply { id = View.generateViewId(); text = "本机 CLI" }
        val remote = RadioButton(this).apply { id = View.generateViewId(); text = "已有网关" }
        modes.addView(local); modes.addView(remote); layout.addView(modes)
        val previous = runCatching { store.load() }.getOrNull()
        fun entry(hintText: String, value: String, kind: Int) = EditText(this).apply {
            hint = hintText; inputType = kind; setSingleLine(); setText(value); isSaveEnabled = false
            layout.addView(this)
        }
        val command = entry("ssmcp 用户名@服务器", prefs.getString("command", StartCommand.DEFAULT)!!,
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS)
        val address = entry("https://gui.example", prefs.getString("origin", previous?.first) ?: "",
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI)
        val token = entry("配对令牌（也可在工作台配对）", "", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD)
        val hint = TextView(this).apply { setPadding(0, dp(8), 0, dp(4)); layout.addView(this) }
        val forget = Button(this, null, android.R.attr.borderlessButtonStyle).apply {
            text = "删除已保存的配对"
            layout.addView(this)
        }
        var forgotten = false
        forget.setOnClickListener {
            runCatching { store.delete() }.onSuccess { forgotten = true; forget.visibility = View.GONE; token.setText("") }
                .onFailure { hint.text = "无法删除配对。" }
        }
        modes.setOnCheckedChangeListener { _, checked ->
            val onDevice = checked == local.id
            command.visibility = if (onDevice) View.VISIBLE else View.GONE
            address.visibility = if (onDevice) View.GONE else View.VISIBLE
            token.visibility = address.visibility
            forget.visibility = if (!onDevice && previous != null && !forgotten) View.VISIBLE else View.GONE
            hint.text = if (onDevice) "ssmcp gui serve 打开本机工作台。填写 ssmcp 用户名@服务器 可同时启动会话。"
                else "填写提供工作台的 HTTPS 地址。本机 HTTP 使用 127.0.0.1 和网关实际端口。"
        }
        modes.check(if (prefs.getString("mode", if (previous != null) "remote" else "local") == "remote") remote.id else local.id)
        val dialog = AlertDialog.Builder(this).setTitle("启动设置").setView(ScrollView(this).apply { addView(layout) })
            .setPositiveButton("打开", null).setNegativeButton("取消", null)
            .setNeutralButton("停止本机", null).create()
        if (getSharedPreferences("command_receipts", MODE_PRIVATE).contains("entries")) {
            layout.addView(Button(this, null, android.R.attr.borderlessButtonStyle).apply {
                text = "查看旧版操作记录"; setOnClickListener { legacyReceipts() }
            })
        }
        dialog.setOnShowListener {
            dialog.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener {
                try {
                    if (local.isChecked) openLocal(command.text.toString())
                    else openRemote(address.text.toString(), token.text.toString().trim(), if (forgotten) null else previous)
                    dialog.dismiss()
                } catch (e: Exception) { hint.text = e.message ?: "启动设置无效。" }
            }
            dialog.getButton(AlertDialog.BUTTON_NEUTRAL).apply {
                visibility = if (prefs.getString("mode", "local") == "local") View.VISIBLE else View.GONE
                setOnClickListener {
                    dialog.dismiss()
                    transition { LocalRuntime.stop(); prefs.edit().putString("mode", "stopped").apply(); null }
                }
            }
        }
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
        }.getOrDefault("旧版操作记录无法读取。")
        AlertDialog.Builder(this).setTitle("旧版操作记录").setMessage(text.ifEmpty { "没有记录。" })
            .setPositiveButton("关闭", null).show()
    }

    override fun onPause() { web?.onPause(); super.onPause() }
    override fun onResume() { super.onResume(); web?.onResume() }
    override fun onDestroy() { ++generation; removePage(); super.onDestroy() }
}

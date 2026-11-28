package dev.ssmcp.mobile

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.graphics.Bitmap
import android.os.SystemClock
import android.view.View
import android.net.Uri
import android.webkit.*
import java.io.ByteArrayInputStream

/** Runs the gateway's own HTML and modules. The page has no callable Android interface. */
@SuppressLint("SetJavaScriptEnabled")
internal class WorkbenchView(
    activity: Activity,
    private val target: Workbench,
    private val failed: (String) -> Unit
) : WebView(activity) {
    private val homePage = target.origin + "/"
    private var pairing = target.token
    private var waitingForPairing = pairing.isNotEmpty()
    private var pageGeneration = 0

    init {
        isSaveEnabled = false
        visibility = if (waitingForPairing) View.INVISIBLE else View.VISIBLE
        settings.apply {
            javaScriptEnabled = true
            allowFileAccess = false
            allowContentAccess = false
            mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
            javaScriptCanOpenWindowsAutomatically = false
            setSupportMultipleWindows(false)
            cacheMode = WebSettings.LOAD_NO_CACHE
        }
        importantForAutofill = IMPORTANT_FOR_AUTOFILL_NO_EXCLUDE_DESCENDANTS
        CookieManager.getInstance().setAcceptThirdPartyCookies(this, false)
        webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                val uri = request.url
                if (request.isForMainFrame && request.hasGesture() && request.method == "GET"
                    && uri.scheme == "https" && !uri.host.isNullOrEmpty() && uri.userInfo == null
                    && uri.path == "/setup" && uri.fragment == null && uri.queryParameterNames == setOf("code")
                    && uri.getQueryParameters("code").singleOrNull()?.matches(Regex("[A-Za-z0-9-]{4,32}")) == true) {
                    try {
                        activity.startActivity(Intent(Intent.ACTION_VIEW, uri).addCategory(Intent.CATEGORY_BROWSABLE))
                    } catch (_: ActivityNotFoundException) {
                        failed(context.getString(R.string.open_relay_setup_in_browser))
                    }
                    return true
                }
                return !target.owns(uri.toString())
            }

            override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) {
                ++pageGeneration
                if (target.owns(url) && (!waitingForPairing || url == homePage)) {
                    view.visibility = if (waitingForPairing) View.INVISIBLE else View.VISIBLE
                } else {
                    view.visibility = View.INVISIBLE
                    view.stopLoading()
                    failed(context.getString(R.string.unexpected_workbench_origin))
                }
            }

            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                if (target.owns(request.url.toString())) return null
                return WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden", emptyMap(),
                    ByteArrayInputStream(ByteArray(0)))
            }

            override fun onPageFinished(view: WebView, url: String) {
                if (waitingForPairing && url == homePage) {
                    finishPairing(++pageGeneration, SystemClock.uptimeMillis() + 5_000)
                }
            }

            override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
                if (request.isForMainFrame) failed(context.getString(R.string.workbench_load_failed))
            }
            override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
                if (request.isForMainFrame) failed(context.getString(R.string.workbench_http_error, response.statusCode))
            }
        }
        loadUrl(homePage)
    }

    private fun finishPairing(generation: Int, deadline: Long) {
        if (generation != pageGeneration || url != homePage || !waitingForPairing) return
        evaluateJavascript("document.documentElement.dataset.ssmcpPairingReady") { state ->
            if (generation != pageGeneration || url != homePage) return@evaluateJavascript
            when (state) {
                "\"1\"" -> if (pairing.isNotEmpty()) {
                    postWebMessage(WebMessage(pairing), Uri.parse(target.origin))
                    pairing = ""
                }
                "\"paired\"" -> if (pairing.isEmpty()) {
                    waitingForPairing = false
                    visibility = View.VISIBLE
                    return@evaluateJavascript
                }
            }
            if (SystemClock.uptimeMillis() < deadline) postDelayed({ finishPairing(generation, deadline) }, 100)
            else failed(context.getString(R.string.automatic_pairing_failed))
        }
    }

    fun dispose() {
        ++pageGeneration
        pairing = ""
        stopLoading()
        webChromeClient = null
        webViewClient = WebViewClient()
        destroy()
    }
}

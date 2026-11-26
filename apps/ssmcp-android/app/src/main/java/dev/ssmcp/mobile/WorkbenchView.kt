package dev.ssmcp.mobile

import android.annotation.SuppressLint
import android.app.Activity
import android.net.Uri
import android.webkit.*
import java.io.ByteArrayInputStream

/** Runs the gateway's own HTML and modules. The page has no callable Android interface. */
@SuppressLint("SetJavaScriptEnabled")
internal class WorkbenchView(
    activity: Activity,
    private val target: Workbench,
    private val choosePairingFile: (ValueCallback<Array<Uri>>) -> Unit,
    private val failed: (String) -> Unit
) : WebView(activity) {
    private var pairing = target.token

    init {
        isSaveEnabled = false
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
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest) =
                !target.owns(request.url.toString())

            override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse? {
                if (target.owns(request.url.toString())) return null
                return WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden", emptyMap(),
                    ByteArrayInputStream(ByteArray(0)))
            }

            override fun onPageFinished(view: WebView, url: String) {
                if (url != target.origin + "/" || pairing.isEmpty()) return
                // Install a one-shot receiver on the existing pairing form. The secret travels
                // in an origin-targeted message, never in a URL, an intent, or script source.
                evaluateJavascript("""
                    (() => {
                      const entry = document.getElementById('pairing-token');
                      const form = document.getElementById('auth-form');
                      const open = document.getElementById('auth-button');
                      if (!entry || !form || !open) return false;
                      window.addEventListener('message', function receive(event) {
                        if (event.source !== null || typeof event.data !== 'string') return;
                        window.removeEventListener('message', receive);
                        open.click();
                        if (!document.getElementById('auth-dialog').open) return;
                        entry.value = event.data;
                        form.requestSubmit();
                      });
                      return true;
                    })()
                """.trimIndent()) { ready ->
                    if (ready == "true" && this@WorkbenchView.url == target.origin + "/" && pairing.isNotEmpty()) {
                        postWebMessage(WebMessage(pairing), Uri.parse(target.origin))
                        pairing = ""
                    }
                }
            }

            override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
                if (request.isForMainFrame) failed("工作台未能加载，请检查网关地址或重新打开。")
            }
            override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
                if (request.isForMainFrame) failed("工作台返回 HTTP ${response.statusCode}，请检查网关地址。")
            }
        }
        webChromeClient = object : WebChromeClient() {
            override fun onShowFileChooser(view: WebView, callback: ValueCallback<Array<Uri>>, params: FileChooserParams): Boolean {
                if (view.url == target.origin + "/" && params.mode == FileChooserParams.MODE_OPEN) choosePairingFile(callback)
                else callback.onReceiveValue(null)
                return true
            }
        }
        loadUrl(target.origin + "/")
    }

    fun dispose() {
        pairing = ""
        stopLoading()
        webChromeClient = null
        webViewClient = WebViewClient()
        destroy()
    }
}

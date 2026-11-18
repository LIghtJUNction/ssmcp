//! Static, script-free account pages. All stored display data is escaped.
use super::store::Account;

pub(super) const CSS: &str = include_str!("portal.css");

const GITHUB_ICON: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 .8a11.2 11.2 0 0 0-3.54 21.83c.56.1.77-.24.77-.54v-2.08c-3.13.68-3.79-1.33-3.79-1.33-.51-1.3-1.25-1.65-1.25-1.65-1.03-.7.08-.69.08-.69 1.13.08 1.73 1.16 1.73 1.16 1 1.72 2.63 1.22 3.27.93.1-.73.4-1.22.72-1.5-2.5-.29-5.14-1.25-5.14-5.57 0-1.23.44-2.23 1.16-3.01-.12-.28-.5-1.43.11-2.98 0 0 .95-.3 3.08 1.15a10.73 10.73 0 0 1 5.6 0c2.14-1.45 3.08-1.15 3.08-1.15.62 1.55.23 2.7.12 2.98.72.78 1.15 1.78 1.15 3.01 0 4.33-2.64 5.28-5.16 5.56.41.35.77 1.03.77 2.08v3.09c0 .3.2.65.77.54A11.2 11.2 0 0 0 12 .8Z"/></svg>"#;

const KEY_ICON: &str = r#"<svg class="empty-symbol" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="8" cy="8" r="5"/><path d="m11.5 11.5 9 9M17 17l3-3m-6 0 3-3"/><circle cx="7" cy="7" r=".6" fill="currentColor" stroke="none"/></svg>"#;

pub(super) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) fn page(title: &str, body: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="color-scheme" content="dark">
  <meta name="theme-color" content="#0c0d0f">
  <meta name="description" content="使用用户名连接本机工具。GitHub 自动登录绑定，HTTPS 接入，管理账户与在线连接。">
  <title>{} · ssmcp</title>
  <link rel="stylesheet" href="/portal.css">
</head>
<body>
<a class="skip-link" href="#content">跳到主要内容</a>
<header class="site-header">
  <nav class="site-nav container" aria-label="主导航">
    <a class="brand" href="/" aria-label="ssmcp 首页"><span class="brand-mark" aria-hidden="true">&gt;_</span><span class="brand-name">ssmcp</span><span class="brand-label">账号门户</span></a>
    <div class="nav-links"><a href="/#guide">连接指南 <span class="arrow" aria-hidden="true">↓</span></a><a href="https://github.com/LIghtJUNction/ssmcp">GitHub <span class="arrow" aria-hidden="true">↗</span></a></div>
  </nav>
</header>
<main class="container" id="content">{body}</main>
<footer class="site-footer container"><p class="footer-brand">ssmcp<span>your machine, connected.</span></p><p>HTTPS 接入 · GitHub 身份验证 · MCP 工具调用</p><a href="https://github.com/LIghtJUNction/ssmcp">开源代码 ↗</a></footer>
</body>
</html>"##,
        escape(title)
    )
}

fn account_panel(
    account: Option<&Account>,
    csrf: &str,
    enabled: bool,
    management_enabled: bool,
) -> String {
    let (status_class, status) = if account.is_some() {
        ("status signed-in", "已登录")
    } else if enabled {
        ("status", "未登录")
    } else {
        ("status", "尚未启用")
    };
    let mut html = format!(
        r#"<section class="account-panel" id="account" aria-labelledby="account-title">
<div class="panel-bar"><span class="eyebrow">01 / ACCOUNT</span><span class="{status_class}">{status}</span></div><div class="account-body">"#
    );
    if let Some(a) = account {
        html.push_str(&format!(
            r##"<h2 id="account-title">{}</h2><p class="account-description">在项目目录运行连接命令，即可使用已授权的工具。</p>
<dl class="account-facts"><div><dt>服务器用户名</dt><dd class="mono">{}</dd></div><div><dt>GitHub 账户</dt><dd>{}</dd></div></dl>
<div class="account-actions"><a class="button" href="#connect">连接这台电脑 <span class="arrow" aria-hidden="true">↗</span></a><form method="post" action="/account/logout"><input type="hidden" name="csrf" value="{csrf}"><button class="secondary" type="submit">退出门户</button></form></div>
<small class="account-note">用户名绑定 GitHub 账号编号，GitHub 改名后保持不变。</small>"##,
            escape(a.handle.as_deref().unwrap_or(&a.github_login)),
            escape(a.handle.as_deref().unwrap_or("尚未选择 · 在终端命令中填写")),
            escape(&a.github_login),
        ));
    } else if enabled {
        html.push_str(&format!(
            r#"<h2 id="account-title">一条命令，连接电脑。</h2><p class="account-description">在终端选择用户名，客户端会自动打开 GitHub 登录并完成绑定。已有凭据时直接连接。</p>
<form class="login-form" method="post" action="/account/login"><input type="hidden" name="csrf" value="{csrf}"><button class="login-button" type="submit">{GITHUB_ICON}使用 GitHub 管理账户<span class="arrow" aria-hidden="true">↗</span></button></form>
<small class="account-note">门户用于查看账户和连接；从终端开始即可完成首次接入。</small>"#
        ));
    } else {
        html.push_str(
            r#"<h2 id="account-title">账户门户尚未启用。</h2><p class="account-description">服务器完成 GitHub 登录与账户存储配置后，即可用自选用户名连接。</p><small class="account-note">请联系服务器管理员完成配置。</small>"#,
        );
    }
    if account.is_some() && management_enabled {
        html.push_str(r#"<div class="account-shortcuts"><a class="text-link" href="/account/sessions">我的会话 ↗</a><a class="text-link" href="/account/history">历史记录 ↗</a></div>"#);
    }
    html.push_str("</div></section>");
    html
}

fn keys_panel(account: Option<&Account>, csrf: &str, enabled: bool) -> String {
    let count = account
        .map(|a| format!(r#"<span class="count">{} / 20</span>"#, a.keys.len()))
        .unwrap_or_default();
    let mut html = format!(
        r#"<section class="workspace-section" id="keys" aria-labelledby="keys-title"><div class="section-heading"><p class="eyebrow">02 / SSH KEYS</p><div class="section-title"><h2 id="keys-title">SSH 公钥</h2>{count}</div><p class="section-description">用一把公钥，识别你的电脑。</p></div>"#
    );
    if let Some(a) = account {
        if a.keys.is_empty() {
            html.push_str(&format!(
                r#"<div class="empty-state">{KEY_ICON}<h3>还没有登记公钥</h3><p>把本机的 Ed25519 公钥粘贴到下方，开始连接。</p></div>"#
            ));
        }
        for key in &a.keys {
            let label = if key.label.is_empty() {
                "未命名公钥"
            } else {
                &key.label
            };
            html.push_str(&format!(
                r#"<article class="key"><div class="key-heading"><span class="key-type">ED25519</span><strong>{}</strong></div><p class="fingerprint"><code>{}</code></p>
<details class="key-details"><summary>查看完整公钥</summary><pre><code>{}</code></pre></details>
<details class="key-details key-remove"><summary>删除这把公钥</summary><form method="post" action="/account/keys/delete"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="fingerprint" value="{}"><p>SSH 同步生效后，此公钥不能再发起新的连接；已有连接保持运行。</p><button class="danger" type="submit">确认删除公钥</button></form></details></article>"#,
                escape(label),
                escape(&key.fingerprint),
                escape(&key.public_key),
                escape(&key.fingerprint)
            ));
        }
        html.push_str(&format!(
            r#"<form class="key-form" method="post" action="/account/keys"><input type="hidden" name="csrf" value="{csrf}">
<label for="key-label">备注 <span class="muted">/ 可选</span><input id="key-label" name="label" maxlength="80" placeholder="例如：工作笔记本" autocomplete="off"></label>
<label for="public-key">SSH 公钥<textarea id="public-key" name="public_key" maxlength="16384" required rows="3" spellcheck="false" autocapitalize="none" autocomplete="off" placeholder="ssh-ed25519 AAAA…" aria-describedby="key-help"></textarea></label>
<small class="help-note" id="key-help">仅接受单行 Ed25519 公钥。上传 .pub 内容，私钥留在本机。</small><button type="submit">添加公钥 <span class="arrow" aria-hidden="true">+</span></button></form>
<small class="help-note">每个账户最多 20 把。启用 SSH 同步后，变更通常在 15 秒内生效；同步失败时请联系管理员。</small>"#
        ));
    } else {
        let login_link = if enabled {
            r##"<a class="text-link" href="#account">前往账户登录 <span class="arrow" aria-hidden="true">↗</span></a>"##
        } else {
            r#"<p class="muted">请联系管理员登记公钥。</p>"#
        };
        html.push_str(&format!(
            r##"<div class="empty-state">{KEY_ICON}<h3>为你的电脑准备一把密钥</h3><p>登录后，在这里添加、查看和管理公钥。已有 SSH 账户可继续使用原来的密钥。</p>{login_link}</div>"##
        ));
    }
    html.push_str(
        r#"<details><summary>在哪里找到我的公钥？</summary><p>在本机终端查看已有的公钥：</p><pre class="command"><code>cat ~/.ssh/id_ed25519.pub</code></pre><p>没有密钥时，创建一把 Ed25519 密钥：</p><pre class="command"><code>ssh-keygen -t ed25519</code></pre><p>只提交 <code>.pub</code> 文件中的单行 SSH 公钥（包括硬件安全密钥），由 OpenSSH 验证。私钥保留在本机，不要上传。</p></details></section>"#,
    );
    html
}

fn connect_panel(account: Option<&Account>, public_url: &str) -> String {
    let origin = public_url.trim_end_matches('/');
    let host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or(origin);
    let username = account
        .and_then(|account| account.handle.as_deref())
        .unwrap_or("myname");
    let command = format!("ssmcp {username}@{host}");
    let hint = if account.is_some_and(|account| account.handle.is_some()) {
        "已填入你的服务器用户名"
    } else {
        "把 myname 换成你想使用的用户名"
    };
    format!(
        r#"<section class="workspace-section" id="connect" aria-labelledby="connect-title"><div class="section-heading"><p class="eyebrow">02 / CONNECT</p><h2 id="connect-title">连接你的工作目录</h2><p class="section-description">安装客户端后，在需要操作的项目目录运行。</p></div>
<div class="terminal" aria-label="本机连接命令"><div class="terminal-bar"><span>terminal</span><span class="terminal-label">~/your-project</span></div><div class="terminal-content"><p class="terminal-comment"># 在本机执行</p><pre class="command"><code>{}</code></pre></div></div>
<div class="connect-actions"><p class="command-hint">{hint}</p></div>
<p class="section-description">首次连接时，浏览器会引导 GitHub 登录。核对用户名和设备后允许连接；后续自动复用本机凭据。</p>
<a class="text-link" href="https://github.com/LIghtJUNction/ssmcp#安装与升级">安装与升级 <span class="arrow" aria-hidden="true">↗</span></a>
<details><summary>用户名和工具发现</summary><p>用户名为 3–32 个英文字母、数字、下划线或连字符，以字母开头、字母或数字结尾。名称不区分大小写，已占名称只允许原绑定 GitHub 账户登录。</p><p>Shell 始终可用。客户端默认提供内置工具，外部 harness 仅在显式使用时加载；工具需要的本机登录和授权仍由你管理。</p></details></section>"#,
        escape(&command)
    )
}

const GUIDE: &str = r#"<section class="guide" id="guide" aria-labelledby="guide-title">
<div class="guide-header"><div><p class="eyebrow">03 / CHATGPT</p><h2 id="guide-title">最后一步，在 ChatGPT 中连接。</h2></div><a class="text-link" href="https://chatgpt.com/plugins">打开插件设置 <span class="arrow" aria-hidden="true">↗</span></a></div>
<div class="guide-grid">
<div class="guide-step"><span class="step-number">01</span><h3>添加一个插件</h3><p>启动本机客户端后，打开 <a href="https://chatgpt.com/plugins">ChatGPT Plugins</a>，点击 <strong>Add</strong>。</p></div>
<div class="guide-step"><span class="step-number">02</span><h3>填写连接信息</h3><dl class="connection-fields"><div><dt>Name</dt><dd>ssmcp</dd></div><div><dt>Connection</dt><dd>{mcp_url}</dd></div></dl></div>
<div class="guide-step"><span class="step-number">03</span><h3>授权，然后开始</h3><p>勾选同意后点击 <strong>Create</strong>。按提示使用 GitHub 登录，核对身份与客户端，再点击“允许连接”。</p></div>
</div>
<details class="guide-help"><summary>已添加插件，接下来怎么用？</summary><div><p>回到 ChatGPT，调用 <code>list</code> 查看在线客户端，再使用 <code>code_mode</code> 调用工具。列表为空时，检查本机客户端是否运行，以及插件与客户端是否使用同一 GitHub 账户。</p><p>插件授权时请核对 MCP 客户端与回调来源。插件可以调用你已授权的本机工具，包括执行命令和修改文件。</p></div></details>
</section>"#;

pub(super) fn home(
    account: Option<&Account>,
    csrf: Option<&str>,
    enabled: bool,
    management_enabled: bool,
    public_url: &str,
) -> String {
    let csrf = escape(csrf.unwrap_or(""));
    let mut body = String::from(
        r#"<section class="hero" aria-labelledby="hero-title"><div class="hero-copy"><p class="eyebrow">LOCAL TOOLS. OPEN POSSIBILITIES.</p><h1 id="hero-title">本机工具，<br><span>连接你的 AI。</span></h1><p class="hero-description">用一个用户名，把本机工具接入 ChatGPT。<br>工具在本机执行，访问权限由你掌控。</p><div class="protocol" aria-label="本机通过 HTTPS 和 MCP 连接 ChatGPT"><span>YOUR MACHINE</span><span class="arrow" aria-hidden="true">→</span><span>HTTPS / MCP</span><span class="arrow" aria-hidden="true">→</span><span>ChatGPT</span></div></div>"#,
    );
    body.push_str(&account_panel(account, &csrf, enabled, management_enabled));
    body.push_str(
        r#"</section><div class="workspace-header"><p class="eyebrow">WORKSPACE / 接入配置</p><p>选择用户名，完成 GitHub 绑定，即可连接。</p></div><div class="workspace-flow">"#,
    );
    body.push_str(&connect_panel(account, public_url));
    body.push_str("<details><summary>旧版 SSH 公钥管理</summary>");
    body.push_str(&keys_panel(account, &csrf, enabled));
    body.push_str("</details>");
    body.push_str("</div>");
    let mcp_url = format!("{}/mcp", public_url.trim_end_matches('/'));
    body.push_str(&GUIDE.replace("{mcp_url}", &escape(&mcp_url)));
    page("账号门户", &body)
}

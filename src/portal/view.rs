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
  <meta name="description" content="通过 SSH 把本机工具接入 ChatGPT。管理 ssmcp 账户、公钥和连接配置。">
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
<footer class="site-footer container"><p class="footer-brand">ssmcp<span>your machine, connected.</span></p><p>SSH 接入 · GitHub 身份验证 · MCP 工具调用</p><a href="https://github.com/LIghtJUNction/ssmcp">开源代码 ↗</a></footer>
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
            r##"<h2 id="account-title">{}</h2><p class="account-description">你的账户已就绪。管理公钥，连接工作目录。</p>
<dl class="account-facts"><div><dt>SSH 用户名</dt><dd class="mono">{}</dd></div><div><dt>已登记公钥</dt><dd class="mono">{} / 20</dd></div></dl>
<div class="account-actions"><a class="button" href="#keys">管理公钥 <span class="arrow" aria-hidden="true">↗</span></a><form method="post" action="/account/logout"><input type="hidden" name="csrf" value="{csrf}"><button class="secondary" type="submit">退出登录</button></form></div>
<small class="account-note">SSH 用户名绑定 GitHub 账号编号，改名后保持不变。</small>"##,
            escape(&a.github_login),
            escape(&a.username),
            a.keys.len()
        ));
    } else if enabled {
        html.push_str(&format!(
            r#"<h2 id="account-title">连接，从这里开始。</h2><p class="account-description">使用 GitHub 登录，获取你的 SSH 用户名，<br>管理有权连接的电脑公钥。</p>
<form class="login-form" method="post" action="/account/login"><input type="hidden" name="csrf" value="{csrf}"><button class="login-button" type="submit">{GITHUB_ICON}使用 GitHub 登录<span class="arrow" aria-hidden="true">↗</span></button></form>
<small class="account-note">公钥在这里登记，私钥始终留在你的电脑。</small>"#
        ));
    } else {
        html.push_str(
            r#"<h2 id="account-title">账户门户尚未启用。</h2><p class="account-description">已有 SSH 账户仍可连接。首次接入，请联系管理员登记公钥。</p><small class="account-note">管理员需配置 GitHub 登录与账户存储。</small>"#,
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
<label for="public-key">SSH 公钥<textarea id="public-key" name="public_key" maxlength="1024" required rows="3" spellcheck="false" autocapitalize="none" autocomplete="off" placeholder="ssh-ed25519 AAAA…" aria-describedby="key-help"></textarea></label>
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
        r#"<details><summary>在哪里找到我的公钥？</summary><p>在本机终端查看已有的公钥：</p><pre class="command"><code>cat ~/.ssh/id_ed25519.pub</code></pre><p>没有密钥时，创建一把 Ed25519 密钥：</p><pre class="command"><code>ssh-keygen -t ed25519</code></pre><p>只提交 <code>.pub</code> 文件中的单行 <code>ssh-ed25519</code> 公钥。私钥保留在本机，不要上传。</p></details></section>"#,
    );
    html
}

fn connect_panel(account: Option<&Account>) -> String {
    let (command, hint) = match account {
        Some(a) => (
            format!(
                "ssmcp --allow {}@github {}@ssmcp.lmm.best",
                a.github_login, a.username
            ),
            "已填入你的账户信息 · 点击命令可全选",
        ),
        None => (
            "ssmcp --allow USER@github gh-ID@ssmcp.lmm.best".into(),
            "命令示例 · 登录后显示你的专属命令",
        ),
    };
    format!(
        r#"<section class="workspace-section" id="connect" aria-labelledby="connect-title"><div class="section-heading"><p class="eyebrow">03 / CONNECT</p><h2 id="connect-title">连接你的工作目录</h2><p class="section-description">安装客户端后，在需要操作的项目目录运行。</p></div>
<div class="terminal" aria-label="本机连接命令"><div class="terminal-bar"><span>terminal</span><span class="terminal-label">~/your-project</span></div><div class="terminal-content"><p class="terminal-comment"># 在本机执行</p><pre class="command"><code>{}</code></pre></div></div>
<div class="connect-actions"><p class="command-hint">{hint}</p></div>
<p class="section-description">保持客户端运行，ChatGPT 就能发现你允许访问的在线工具。</p>
<a class="text-link" href="https://github.com/LIghtJUNction/ssmcp#安装与升级">安装与升级 <span class="arrow" aria-hidden="true">↗</span></a>
<details><summary>连接与工具发现说明</summary><p>Shell 始终可用。客户端自动发现 PATH 中已安装的 Codex、Claude Code、Pi 和 OpenCode；需要使用时，请先在本机完成安装与登录。</p><p>首次 SSH 连接请核对服务器主机密钥。GitHub 改名后，更新命令中的 <code>--allow</code> 用户名，SSH 用户名保持不变。</p></details></section>"#,
        escape(&command)
    )
}

const GUIDE: &str = r#"<section class="guide" id="guide" aria-labelledby="guide-title">
<div class="guide-header"><div><p class="eyebrow">04 / CHATGPT</p><h2 id="guide-title">最后一步，在 ChatGPT 中连接。</h2></div><a class="text-link" href="https://chatgpt.com/plugins">打开插件设置 <span class="arrow" aria-hidden="true">↗</span></a></div>
<div class="guide-grid">
<div class="guide-step"><span class="step-number">01</span><h3>添加一个插件</h3><p>完成公钥登记并启动本机客户端后，打开 <a href="https://chatgpt.com/plugins">ChatGPT Plugins</a>，点击 <strong>Add</strong>。</p></div>
<div class="guide-step"><span class="step-number">02</span><h3>填写连接信息</h3><dl class="connection-fields"><div><dt>Name</dt><dd>ssmcp</dd></div><div><dt>Connection</dt><dd>https://ssmcp.lmm.best/mcp</dd></div></dl></div>
<div class="guide-step"><span class="step-number">03</span><h3>授权，然后开始</h3><p>勾选同意后点击 <strong>Create</strong>。按提示使用 GitHub 登录，核对身份与客户端，再点击“允许连接”。</p></div>
</div>
<details class="guide-help"><summary>已添加插件，接下来怎么用？</summary><div><p>回到 ChatGPT，调用 <code>list</code> 查看在线客户端，再使用 <code>code_mode</code> 调用工具。列表为空时，检查本机进程是否运行，以及 <code>--allow</code> 是否包含当前 GitHub 用户。</p><p>首页登录与插件授权是两个独立步骤。授权时请核对 MCP 客户端与回调来源。插件可调用获准客户端的工具，包括执行命令和修改文件，请按需设置允许名单。</p></div></details>
</section>"#;

pub(super) fn home(
    account: Option<&Account>,
    csrf: Option<&str>,
    enabled: bool,
    management_enabled: bool,
) -> String {
    let csrf = escape(csrf.unwrap_or(""));
    let mut body = String::from(
        r#"<section class="hero" aria-labelledby="hero-title"><div class="hero-copy"><p class="eyebrow">LOCAL TOOLS. OPEN POSSIBILITIES.</p><h1 id="hero-title">本机工具，<br><span>连接你的 AI。</span></h1><p class="hero-description">通过 SSH，把终端接入 ChatGPT。<br>工具在本机执行，访问权限由你掌控。</p><div class="protocol" aria-label="本机通过 SSH 和 MCP 连接 ChatGPT"><span>YOUR MACHINE</span><span class="arrow" aria-hidden="true">→</span><span>SSH / MCP</span><span class="arrow" aria-hidden="true">→</span><span>ChatGPT</span></div></div>"#,
    );
    body.push_str(&account_panel(account, &csrf, enabled, management_enabled));
    body.push_str(
        r#"</section><div class="workspace-header"><p class="eyebrow">WORKSPACE / 接入配置</p><p>你的密钥，你的工具，你的工作方式。</p></div><div class="workspace-grid">"#,
    );
    body.push_str(&keys_panel(account, &csrf, enabled));
    body.push_str(&connect_panel(account));
    body.push_str("</div>");
    body.push_str(GUIDE);
    page("账号门户", &body)
}

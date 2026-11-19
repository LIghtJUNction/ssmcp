//! Server-rendered account pages. All stored display data is escaped.
use super::store::Account;
use crate::user_policy::TrafficQuota;

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
    let favicon_url = super::assets::url("favicon.svg");
    let base_css_url = super::assets::url("portal.css");
    let portal_css_url = super::assets::url("portal-refresh.css");
    let traffic_script_url = super::assets::url("traffic-meter.js");
    format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="color-scheme" content="dark">
  <meta name="theme-color" content="#0b0d0f">
  <meta name="description" content="使用用户名连接本机工具。GitHub 自动登录绑定，HTTPS 接入，管理账户与在线连接。">
  <title>{} · ssmcp</title>
  <link rel="icon" href="{favicon_url}" type="image/svg+xml">
  <link rel="stylesheet" href="{base_css_url}">
  <link rel="stylesheet" href="{portal_css_url}">
  <script defer src="{traffic_script_url}"></script>
</head>
<body>
<a class="skip-link" href="#content">跳到主要内容</a>
<header class="site-header">
  <nav class="site-nav container" aria-label="主导航">
    <a class="brand" href="/" aria-label="ssmcp 首页"><img class="brand-mark" src="{favicon_url}" width="36" height="36" alt="" aria-hidden="true"><span class="brand-name">ssmcp</span></a>
    <div class="nav-links"><a href="/#guide">连接指南 <span class="arrow" aria-hidden="true">↓</span></a><a href="https://github.com/LIghtJUNction/ssmcp">GitHub <span class="arrow" aria-hidden="true">↗</span></a></div>
  </nav>
</header>
<main class="container" id="content">{body}</main>
<footer class="site-footer container"><p class="footer-brand">ssmcp<span>本机工具 · MCP 接入</span></p><p>HTTPS 接入 · GitHub 身份验证 · MCP 工具调用</p><a href="https://github.com/LIghtJUNction/ssmcp">开源代码 ↗</a></footer>
</body>
</html>"##,
        escape(title)
    )
}

// Use decimal units and truncate rather than round: a positive balance remains
// visible and a balance just below 1 GB is never presented as a full 1 GB.
fn traffic_bytes(bytes: u64) -> String {
    for (scale, unit) in [(1_000_000_000, "GB"), (1_000_000, "MB"), (1_000, "KB")] {
        if bytes >= scale {
            let whole = bytes / scale;
            let remainder = bytes % scale;
            if remainder == 0 {
                return format!("{whole} {unit}");
            }
            let hundredths = remainder * 100 / scale;
            return format!("{whole}.{hundredths:02} {unit}");
        }
    }
    format!("{bytes} 字节")
}

fn traffic_expiration(period: &str) -> Option<String> {
    if !crate::user_policy::valid_traffic_period(period) {
        return None;
    }
    let (year, month) = period.split_once('-')?;
    let year = year.parse::<u32>().ok()?;
    let month = month.parse::<u32>().ok()?;
    let (year, month) = if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month + 1)
    };
    Some(format!("{year:04}-{month:02}-01 00:00"))
}

/// Renders only the server's account quota; callers gate access by login.
pub(super) fn traffic_panel(traffic: &TrafficQuota) -> String {
    let used = traffic_bytes(traffic.used_bytes);
    let offset = i64::from(traffic.utc_offset_minutes);
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.unsigned_abs();
    let timezone = format!("UTC{sign}{:02}:{:02}", offset / 60, offset % 60);
    let period = format!("{}（{timezone} 自然月）", escape(&traffic.period));
    let Some(limit_bytes) = traffic.limit_bytes else {
        return format!(
            r#"<dl class="account-facts" aria-label="账户流量"><div><dt>流量额度</dt><dd>无限</dd></div><div><dt>本月已用</dt><dd title="{} 字节">{used}</dd></div></dl>
<details class="traffic-details"><summary>流量统计说明</summary><p class="dashboard-note">管理员账户，无总流量上限。<br>{period}。<br>上下行合计，包含应用协议封装；重试会重新计入。</p></details>"#,
            traffic.used_bytes,
        );
    };
    let expiration = traffic_expiration(&traffic.period)
        .map(|date| format!(r#"<br>额度到期：<span class="mono">{date} {timezone}</span>。"#))
        .unwrap_or_default();
    let remaining_bytes = traffic
        .remaining_bytes
        .unwrap_or_else(|| limit_bytes.saturating_sub(traffic.used_bytes));
    let remaining = traffic_bytes(remaining_bytes);
    let limit = traffic_bytes(limit_bytes);
    let free = traffic_bytes(traffic.free_bytes);
    let paid = traffic_bytes(traffic.paid_bytes);
    let exhausted = if remaining_bytes == 0 {
        r#"<p class="traffic-alert" role="status">本月流量已用完。<a href="/account/traffic">查看流量与订单 <span aria-hidden="true">↗</span></a></p>"#
    } else {
        ""
    };
    format!(
        r#"<dl class="account-facts" aria-label="账户流量"><div><dt>剩余流量</dt><dd title="{remaining_bytes} 字节">{remaining}</dd></div><div><dt>本月已用</dt><dd title="{} 字节">{used}</dd></div><div><dt>本月额度</dt><dd title="{limit_bytes} 字节">{limit}</dd></div></dl>
{exhausted}<details class="traffic-details"><summary>额度与计费说明</summary><p class="dashboard-note">每月免费 <span title="{} 字节">{free}</span>，本月付费 <span title="{} 字节">{paid}</span>。<br>{period}，免费与付费流量月底到期，不滚存。{expiration}<br>上下行合计，包含应用协议封装；重试会重新计入。</p>
<p class="dashboard-note">服务器流量有限，经常使用请购买流量包。</p></details>"#,
        traffic.used_bytes, traffic.free_bytes, traffic.paid_bytes,
    )
}

fn account_panel(
    account: Option<&Account>,
    csrf: &str,
    enabled: bool,
    management_enabled: bool,
    traffic: Option<&TrafficQuota>,
) -> String {
    let (status_class, status) = if account.is_some() {
        ("status signed-in", "已登录")
    } else if enabled {
        ("status", "尚未登录")
    } else {
        ("status", "尚未启用")
    };
    let mut html = format!(
        r#"<section class="account-panel" id="account" aria-labelledby="account-title">
<div class="panel-bar"><span class="eyebrow">你的账户</span><span class="{status_class}">{status}</span></div><div class="account-body">"#
    );
    if let Some(a) = account {
        let username_form = if a.handle.is_none() {
            format!(
                r#"<form class="key-form" method="post" action="/account/username"><input type="hidden" name="csrf" value="{csrf}"><label for="account-username">服务器用户名<input id="account-username" name="username" required minlength="3" maxlength="32" autocapitalize="none" spellcheck="false" aria-describedby="username-help"></label><small id="username-help">3–32 个英文字母、数字、下划线或连字符。以字母开头，以字母或数字结尾。登记后绑定当前 GitHub 账号。</small><button type="submit">注册用户名</button></form>"#
            )
        } else {
            String::new()
        };
        let primary_action = if management_enabled {
            r#"<a class="button secondary" href="/account/sessions">查看我的会话 <span class="arrow" aria-hidden="true">↗</span></a>"#
        } else {
            r##"<a class="button secondary" href="#connect">连接电脑 <span class="arrow" aria-hidden="true">↗</span></a>"##
        };
        html.push_str(&format!(
            r#"<h2 id="account-title">账户信息</h2>
<dl class="account-facts identity-facts"><div><dt>服务器用户名</dt><dd class="mono">{}</dd></div><div><dt>GitHub 账户</dt><dd>{}</dd></div></dl>
{username_form}<div class="account-actions">{primary_action}</div>"#,
            escape(a.handle.as_deref().unwrap_or("尚未注册")),
            escape(&a.github_login),
        ));
        if management_enabled {
            html.push_str(r#"<div class="account-shortcuts"><a class="text-link" href="/account/history">历史记录 <span aria-hidden="true">↗</span></a>"#);
            if traffic.is_some_and(|quota| quota.limit_bytes.is_none()) {
                html.push_str(r#"<a class="text-link" href="/account/admin#registered-users">用户管理 <span aria-hidden="true">↗</span></a>"#);
            }
            html.push_str("</div>");
        }
        html.push_str(&format!(
            r#"<div class="account-footer"><small class="account-note">GitHub 身份已绑定</small><form method="post" action="/account/logout"><input type="hidden" name="csrf" value="{csrf}"><button class="quiet" type="submit">退出登录</button></form></div>"#
        ));
    } else if enabled {
        html.push_str(&format!(
            r#"<h2 id="account-title">从这里开始连接。</h2><p class="account-description">使用 GitHub 登录，管理用户名、连接与流量。</p>
<form class="login-form" method="post" action="/account/login"><input type="hidden" name="csrf" value="{csrf}"><button class="login-button" type="submit">{GITHUB_ICON}使用 GitHub 登录<span class="arrow" aria-hidden="true">↗</span></button></form>
<small class="account-note">首次使用？先安装客户端也可以。</small>"#
        ));
    } else {
        html.push_str(
            r##"<h2 id="account-title">账户登录暂未开放。</h2><p class="account-description">可以先安装客户端。账号服务启用后，在这里完成登录。</p><a class="text-link" href="#install">查看安装步骤 <span aria-hidden="true">↗</span></a>"##,
        );
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
    let copy_script_url = super::assets::url("copy-command.js");
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
        r#"<section class="workspace-section setup-section" id="connect" aria-labelledby="connect-title"><div class="section-heading"><p class="eyebrow">第二步</p><h2 id="connect-title">连接你的工作目录</h2><p class="section-description">安装客户端后，在需要操作的项目目录运行。</p></div>
<div class="section-content"><div class="terminal" aria-label="本机连接命令"><div class="terminal-bar"><span>terminal</span><span class="terminal-label">~/your-project</span></div><div class="terminal-content"><p class="terminal-comment"># 在本机执行</p><pre class="command"><code id="client-command">{}</code></pre></div></div>
<div class="connect-actions"><p class="command-hint">{hint}</p><button id="copy-client-command" class="secondary" type="button" hidden>复制命令</button><span id="copy-command-status" role="status"></span></div>
<script src="{copy_script_url}" defer></script>
<p class="section-description">首次连接时，浏览器会引导 GitHub 登录。核对用户名和设备后允许连接；后续自动复用本机凭据。</p>
<a class="text-link" href="https://github.com/LIghtJUNction/ssmcp#安装与升级">安装与升级 <span class="arrow" aria-hidden="true">↗</span></a>
<details><summary>用户名和工具发现</summary><p>用户名为 3–32 个英文字母、数字、下划线或连字符，以字母开头、字母或数字结尾。名称不区分大小写，已占名称只允许原绑定 GitHub 账户登录。</p><p>客户端会列出当前可用的工具；工具需要的本机登录和授权仍由你管理。</p></details></div></section>"#,
        escape(&command)
    )
}

const GUIDE: &str = r#"<section class="guide" id="guide" aria-labelledby="guide-title">
<div class="guide-header"><div><p class="eyebrow">第三步</p><h2 id="guide-title">在 ChatGPT 中添加连接</h2></div><a class="text-link" href="https://chatgpt.com/plugins">打开插件设置 <span class="arrow" aria-hidden="true">↗</span></a></div>
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
    traffic: Option<&TrafficQuota>,
) -> String {
    let csrf = escape(csrf.unwrap_or(""));
    let mut body = if let Some(account) = account {
        let name = escape(account.handle.as_deref().unwrap_or(&account.github_login));
        format!(
            r##"<div class="portal-home is-signed-in"><section class="account-workspace" aria-labelledby="hero-title"><header class="workspace-intro"><div><p class="eyebrow">账户概览</p><h1 id="hero-title">你好，{name}。</h1><p class="hero-description">管理你的连接与本月用量。</p></div><a class="button" href="#connect">连接电脑 <span class="arrow" aria-hidden="true">↗</span></a></header><div class="workspace-overview">"##
        )
    } else {
        String::from(
            r##"<div class="portal-home"><section class="hero" aria-labelledby="hero-title"><div class="hero-copy"><p class="eyebrow">本机工具，直接接入 AI</p><h1 id="hero-title">在熟悉的环境，<br><span>连接你的 AI。</span></h1><p class="hero-description">用一个用户名连接项目目录、终端与本机工具。<br>工具在你的电脑上运行，权限由你掌控。</p><div class="hero-actions"><a class="button secondary" href="#install">安装客户端 <span class="arrow" aria-hidden="true">↓</span></a><a class="text-link" href="#guide">查看连接步骤</a></div></div>"##,
        )
    };
    if let Some(traffic) = traffic.filter(|_| account.is_some()) {
        body.push_str(
            r#"<section class="account-traffic" aria-labelledby="traffic-title"><div class="traffic-heading"><div><p class="eyebrow">用量</p><h2 id="traffic-title">本月流量</h2></div><a class="text-link" href="/account/traffic">流量与订单 <span aria-hidden="true">↗</span></a></div>"#,
        );
        body.push_str(&traffic_panel(traffic));
        body.push_str("</section>");
    }
    body.push_str(&account_panel(
        account,
        &csrf,
        enabled,
        management_enabled,
        traffic,
    ));
    if account.is_some() {
        body.push_str("</div></section>");
    } else {
        body.push_str("</section>");
    }
    body.push_str(
        r#"<div class="workspace-header"><h2>连接新设备</h2><p>从安装客户端开始，三步完成连接。</p></div><div class="workspace-flow">"#,
    );
    body.push_str(&include_str!("client-install.html").replace(
        "{install_script_url}",
        super::assets::url("client-install-copy.js"),
    ));
    body.push_str(&connect_panel(account, public_url));
    body.push_str(r#"<details class="workspace-legacy"><summary>旧版 SSH 公钥管理</summary>"#);
    body.push_str(&keys_panel(account, &csrf, enabled));
    body.push_str("</details></div>");
    let mcp_url = format!("{}/mcp", public_url.trim_end_matches('/'));
    body.push_str(&GUIDE.replace("{mcp_url}", &escape(&mcp_url)));
    body.push_str("</div>");
    page("账号门户", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_account() -> Account {
        Account {
            subject: "123".into(),
            username: "gh-123".into(),
            github_login: "octocat".into(),
            keys: vec![],
            handle: Some("developer".into()),
            banned: false,
            rate_limit_bps: None,
            has_recharged: false,
        }
    }

    fn monthly_quota(used_bytes: u64) -> TrafficQuota {
        TrafficQuota {
            used_bytes,
            limit_bytes: Some(1_000_000_000),
            remaining_bytes: Some(1_000_000_000u64.saturating_sub(used_bytes)),
            period: "2026-11".into(),
            utc_offset_minutes: 480,
            free_bytes: 1_000_000_000,
            paid_bytes: 0,
        }
    }

    fn logged_in_home(traffic: &TrafficQuota) -> String {
        home(
            Some(&sample_account()),
            Some("csrf"),
            true,
            true,
            "https://example.test",
            Some(traffic),
        )
    }

    #[test]
    fn home_shows_monthly_quota_after_login() {
        let html = logged_in_home(&monthly_quota(250_000_000));
        assert!(html.contains(r#"title="750000000 字节">750 MB</dd>"#));
        assert!(html.contains(r#"title="250000000 字节">250 MB</dd>"#));
        assert!(html.contains(r#"title="1000000000 字节">1 GB</dd>"#));
        assert!(html.contains("每月免费"));
        assert!(html.contains("2026-11（UTC+08:00 自然月）"));
        assert!(html.contains("月底到期，不滚存"));
        assert!(html.contains("上下行合计，包含应用协议封装；重试会重新计入"));
        assert!(html.contains("2026-12-01 00:00 UTC+08:00"));
        assert!(html.contains("服务器流量有限，经常使用请购买流量包。"));
        assert!(!html.contains("不自动重置"));
        assert!(!html.contains("checkout"));
        assert!(!html.contains(r#"href="/account/admin#registered-users""#));
    }

    #[test]
    fn home_includes_paid_traffic_in_monthly_display() {
        let mut traffic = monthly_quota(250_000_000);
        traffic.paid_bytes = 500_000_000;
        traffic.limit_bytes = Some(1_500_000_000);
        traffic.remaining_bytes = Some(1_250_000_000);
        let html = logged_in_home(&traffic);
        assert!(html.contains(r#"title="1250000000 字节">1.25 GB</dd>"#));
        assert!(html.contains(r#"title="1500000000 字节">1.50 GB</dd>"#));
        assert!(html.contains(r#"本月付费 <span title="500000000 字节">500 MB</span>"#));
    }

    #[test]
    fn home_shows_admin_unlimited_quota_without_purchase_prompt() {
        let mut traffic = monthly_quota(2_000_000_000);
        traffic.limit_bytes = None;
        traffic.remaining_bytes = None;
        let html = logged_in_home(&traffic);
        assert!(html.contains("<dt>流量额度</dt><dd>无限</dd>"));
        assert!(html.contains(r#"href="/account/admin#registered-users""#));
        assert!(html.contains(r#"title="2000000000 字节">2 GB</dd>"#));
        assert!(!html.contains("每月免费"));
        assert!(!html.contains("到期"));
        assert!(!html.contains("购买流量包"));
        assert!(!html.contains("本月流量已用完"));
    }

    #[test]
    fn home_marks_exhaustion_and_preserves_a_one_byte_balance() {
        let exhausted = logged_in_home(&monthly_quota(1_000_000_000));
        assert!(exhausted.contains(r#"title="0 字节">0 字节</dd>"#));
        assert!(exhausted.contains("本月流量已用完。"));
        let available = logged_in_home(&monthly_quota(999_999_999));
        assert!(available.contains(r#"title="1 字节">1 字节</dd>"#));
        assert!(available.contains(r#"title="999999999 字节">999.99 MB</dd>"#));
        assert!(!available.contains("本月流量已用完。"));
    }

    #[test]
    fn home_hides_quota_before_login_even_when_supplied() {
        for enabled in [true, false] {
            let html = home(
                None,
                Some("csrf"),
                enabled,
                true,
                "https://example.test",
                Some(&monthly_quota(123_456_789)),
            );
            assert!(!html.contains(r#"aria-label="账户流量""#));
            assert!(!html.contains("123456789 字节"));
            assert!(!html.contains("购买流量包"));
            assert!(!html.contains("2026-11"));
        }
    }

    #[test]
    fn traffic_period_is_escaped_and_uses_server_time_zone() {
        let mut traffic = monthly_quota(0);
        traffic.period = "<script>alert(1)</script>".into();
        traffic.utc_offset_minutes = -330;
        let html = traffic_panel(&traffic);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("UTC-05:30"));
    }

    #[test]
    fn decimal_traffic_units_do_not_round_up_at_boundaries() {
        for (bytes, expected) in [
            (0, "0 字节"),
            (1, "1 字节"),
            (999, "999 字节"),
            (1_000, "1 KB"),
            (999_999, "999.99 KB"),
            (999_999_999, "999.99 MB"),
            (1_000_000_000, "1 GB"),
            (u64::MAX, "18446744073.70 GB"),
        ] {
            assert_eq!(traffic_bytes(bytes), expected);
        }
    }

    #[test]
    fn traffic_expiration_shows_next_month_and_crosses_year_boundary() {
        for (period, expected) in [
            ("2026-11", "2026-12-01 00:00 UTC+08:00"),
            ("2026-12", "2027-01-01 00:00 UTC+08:00"),
            ("2028-02", "2028-03-01 00:00 UTC+08:00"),
        ] {
            let mut traffic = monthly_quota(0);
            traffic.period = period.into();
            let html = traffic_panel(&traffic);
            assert!(html.contains("额度到期"));
            assert!(html.contains(expected), "{period}");
        }
        let mut traffic = monthly_quota(0);
        traffic.utc_offset_minutes = -330;
        assert!(traffic_panel(&traffic).contains("2026-12-01 00:00 UTC-05:30"));
    }

    #[test]
    fn invalid_month_is_escaped_without_inventing_an_expiration() {
        for period in ["", "2026-00", "2026-13", "2026-1", "<script>"] {
            let mut traffic = monthly_quota(0);
            traffic.period = period.into();
            let html = traffic_panel(&traffic);
            assert!(!html.contains("额度到期"), "{period}");
            assert!(!html.contains("<script>"));
            assert!(html.contains(&escape(period)));
        }
    }
}

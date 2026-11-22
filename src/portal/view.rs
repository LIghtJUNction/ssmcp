//! Server-rendered account pages. All stored display data is escaped.
use super::store::Account;
use crate::user_policy::TrafficQuota;

pub(super) const CSS: &str = include_str!("portal.css");

const GITHUB_ICON: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 .8a11.2 11.2 0 0 0-3.54 21.83c.56.1.77-.24.77-.54v-2.08c-3.13.68-3.79-1.33-3.79-1.33-.51-1.3-1.25-1.65-1.25-1.65-1.03-.7.08-.69.08-.69 1.13.08 1.73 1.16 1.73 1.16 1 1.72 2.63 1.22 3.27.93.1-.73.4-1.22.72-1.5-2.5-.29-5.14-1.25-5.14-5.57 0-1.23.44-2.23 1.16-3.01-.12-.28-.5-1.43.11-2.98 0 0 .95-.3 3.08 1.15a10.73 10.73 0 0 1 5.6 0c2.14-1.45 3.08-1.15 3.08-1.15.62 1.55.23 2.7.12 2.98.72.78 1.15 1.78 1.15 3.01 0 4.33-2.64 5.28-5.16 5.56.41.35.77 1.03.77 2.08v3.09c0 .3.2.65.77.54A11.2 11.2 0 0 0 12 .8Z"/></svg>"#;

pub(super) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(super) fn page(title: &str, body: &str) -> String {
    let favicon_url = super::assets::url("brand-mark.png");
    let base_css_url = super::assets::url("portal.css");
    let portal_css_url = super::assets::url("portal-refresh.css");
    let traffic_script_url = super::assets::url("traffic-meter.js");
    format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="color-scheme" content="light">
  <meta name="theme-color" content="#ffffff">
  <meta name="description" content="把 AI 对话连接到你的工作目录。三步安装和连接 ssmcp，在自己的电脑上执行命令、处理文件、跟进任务。">
  <title>{} · ssmcp</title>
  <link rel="icon" href="{favicon_url}" type="image/png">
  <link rel="stylesheet" href="{base_css_url}">
  <link rel="stylesheet" href="{portal_css_url}">
  <script defer src="{traffic_script_url}"></script>
</head>
<body>
<a class="skip-link" href="#content">跳到主要内容</a>
<header class="site-header">
  <nav class="site-nav container" aria-label="主导航">
    <a class="brand" href="/" aria-label="ssmcp 首页"><img class="brand-mark" src="{favicon_url}" width="36" height="36" alt="" aria-hidden="true"><span class="brand-name">ssmcp</span></a>
    <div class="nav-links"><a href="/guide/zh">使用指南</a><a href="https://github.com/LIghtJUNction/ssmcp">GitHub <span class="arrow" aria-hidden="true">↗</span></a><a class="nav-account" href="/#account">我的账户</a></div>
  </nav>
</header>
<main class="container" id="content">{body}</main>
</body>
</html>"##,
        escape(title)
    )
}

// Use decimal units and truncate rather than round: a positive balance remains
// visible and a balance just below 1 GB is never presented as a full 1 GB.
pub(super) fn traffic_bytes(bytes: u64) -> String {
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
            r#"<dl class="account-facts" aria-label="账户流量"><div><dt>流量额度</dt><dd aria-label="无限">∞</dd></div><div><dt>本月已用</dt><dd title="{} 字节">{used}</dd></div></dl>
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
        let traffic_action = if traffic.is_none() {
            r#"<a class="text-link" href="/account/traffic">流量与订单 <span aria-hidden="true">↗</span></a>"#
        } else {
            ""
        };
        html.push_str(&format!(
            r#"<h2 id="account-title">账户信息</h2>
<dl class="account-facts identity-facts"><div><dt>服务器用户名</dt><dd class="mono">{}</dd></div><div><dt>GitHub 账户</dt><dd>{}</dd></div></dl>
{username_form}<div class="account-actions">{primary_action}{traffic_action}</div>"#,
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
            r#"<h2 id="account-title">已有账户？在这里继续。</h2><p class="account-description">使用 GitHub 登录，查看用户名、在线连接与本月流量。</p>
<form class="login-form" method="post" action="/account/login"><input type="hidden" name="csrf" value="{csrf}"><button class="login-button" type="submit">{GITHUB_ICON}使用 GitHub 登录<span class="arrow" aria-hidden="true">↗</span></button></form>
<small class="account-note">第一次使用可从下面的安装步骤开始，连接时再完成登录。</small>"#
        ));
    } else {
        html.push_str(
            r##"<h2 id="account-title">账户登录暂未开放。</h2><p class="account-description">可以先安装客户端。账号服务启用后，在这里完成登录。</p><a class="text-link" href="#install">查看安装步骤 <span aria-hidden="true">↗</span></a>"##,
        );
    }
    html.push_str("</div></section>");
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
<p class="section-description">首次连接会引导你使用 GitHub 登录。核对用户名和设备后允许连接；后续会复用本机凭据。保持这个终端运行，客户端连接成功后继续第三步。</p>
<a class="text-link" href="/guide/zh#help">连接遇到问题？ <span class="arrow" aria-hidden="true">↗</span></a>
<details><summary>用户名和工具发现</summary><p>用户名为 3–32 个英文字母、数字、下划线或连字符，以字母开头、字母或数字结尾。名称不区分大小写，已占名称只允许原绑定 GitHub 账户登录。</p><p>连接后，应用可以调用当前客户端已注册的工具。终端命令和文件操作在你的电脑上执行；额外工具的安装、登录与授权仍由你管理。</p></details></div></section>"#,
        escape(&command)
    )
}

const GUIDE: &str = r#"<section class="workspace-section setup-section connection-section" id="guide" aria-labelledby="guide-title">
<div class="section-heading"><p class="eyebrow">第三步</p><h2 id="guide-title">在 AI 应用中添加连接</h2><p class="section-description">以下以 ChatGPT 为例。其他支持 MCP 的应用使用同一个连接地址。</p></div>
<div class="section-content"><ol class="connection-checklist">
<li><strong>打开插件设置。</strong><p>进入 <a href="https://chatgpt.com/plugins">ChatGPT Plugins</a>，点击 <strong>Add</strong>。</p></li>
<li><strong>填写连接信息。</strong><dl class="connection-fields"><div><dt>Name</dt><dd>ssmcp</dd></div><div><dt>Connection</dt><dd><code>{mcp_url}</code></dd></div></dl></li>
<li><strong>授权同一个 GitHub 账户。</strong><p>勾选同意后点击 <strong>Create</strong>。核对身份与客户端，再点击“允许连接”。回到对话，让 AI 列出在线客户端并选择你的电脑。</p></li>
</ol><details class="guide-help"><summary>添加后没有看到电脑？</summary><div><p>先检查本机客户端是否仍在运行，以及应用和客户端是否使用同一 GitHub 账户与中继。调用 <code>list</code> 刷新在线客户端，再通过 <code>code_mode</code> 查询并调用该客户端的工具。</p><p>插件授权时请核对 MCP 客户端与回调来源。获准的应用可以执行命令和修改文件。<a href="/guide/zh#tools">了解工具与授权边界</a>。</p></div></details></div>
</section>"#;

// A static explanation of the product, never a simulated live account or session.
const PRODUCT_SCENE: &str = r#"<figure class="product-scene" aria-labelledby="scene-caption">
<figcaption id="scene-caption">一个连接，继续用你熟悉的环境。<span>连接示意 · 非实时状态</span></figcaption>
<ol class="scene-flow"><li><span class="scene-index">01</span><strong>AI 应用</strong><span>通过 MCP 发起工具调用</span></li><li><span class="scene-index">02</span><strong>SSMCP 中继</strong><span>把已授权请求送到客户端</span></li><li><span class="scene-index">03</span><strong>你的工作目录</strong><span>本机执行，结果回到对话</span></li></ol>
</figure>"#;

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
            r##"<div class="portal-home"><section class="hero" aria-labelledby="hero-title"><div class="hero-copy"><p class="eyebrow">LOCAL TOOLS. YOUR WORKSPACE.</p><h1 id="hero-title">把对话，连接到<br><span>你的工作目录。</span></h1><p class="hero-description">在自己的电脑上执行命令、读写文件、跟进任务。<br>从安装到连接，三步开始。</p><div class="hero-actions"><a class="button" href="#install">开始连接 <span class="arrow" aria-hidden="true">↓</span></a><a class="button secondary" href="/guide/zh">阅读使用指南 <span class="arrow" aria-hidden="true">↗</span></a></div><p class="hero-platforms">Linux · macOS · Windows <span aria-hidden="true">/</span> MCP · code_mode</p></div>"##,
        )
    };
    if let Some(traffic) = traffic.filter(|_| account.is_some()) {
        body.push_str(
            r#"<section class="account-traffic" aria-labelledby="traffic-title"><div class="traffic-heading"><div><p class="eyebrow">用量</p><h2 id="traffic-title">本月流量</h2></div><a class="text-link" href="/account/traffic">流量与订单 <span aria-hidden="true">↗</span></a></div>"#,
        );
        body.push_str(&traffic_panel(traffic));
        body.push_str("</section>");
    }
    if account.is_none() {
        body.push_str(PRODUCT_SCENE);
        body.push_str("</section><div class=\"visitor-account\">");
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
        body.push_str("</div>");
    }
    body.push_str(
        r#"<div class="workspace-header"><h2>三步，连接你的电脑。</h2><p>准备一个 GitHub 账户，以及你要操作的项目目录。</p></div><div class="workspace-flow">"#,
    );
    body.push_str(&include_str!("client-install.html").replace(
        "{install_script_url}",
        super::assets::url("client-install-copy.js"),
    ));
    body.push_str(&connect_panel(account, public_url));
    body.push_str("</div>");
    let mcp_url = format!("{}/mcp", public_url.trim_end_matches('/'));
    body.push_str(&GUIDE.replace("{mcp_url}", &escape(&mcp_url)));
    body.push_str(
        r#"<section class="learn-more" aria-labelledby="learn-more-title"><div><p class="eyebrow">继续探索</p><h2 id="learn-more-title">先连起来，再按需要深入。</h2><p>文件引用、长任务、多人协作与连接排查，都放在使用指南中。</p></div><nav aria-label="深入使用"><a href="/guide/zh#basics">输入与文件引用 <span aria-hidden="true">↗</span></a><a href="/guide/zh#tools">工具与执行方式 <span aria-hidden="true">↗</span></a><a href="/guide/zh#help">连接排查 <span aria-hidden="true">↗</span></a></nav></section>"#,
    );
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
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            legacy_used_bytes: used_bytes,
            limit_bytes: Some(1_000_000_000),
            remaining_bytes: Some(1_000_000_000u64.saturating_sub(used_bytes)),
            period: "2026-11".into(),
            utc_offset_minutes: 480,
            free_bytes: 1_000_000_000,
            paid_bytes: 0,
            reset_at: Some(1_796_054_400),
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
        assert!(!html.contains('∞'));
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
    fn account_keeps_orders_navigation_without_a_quota_snapshot() {
        let account = sample_account();
        let quota = monthly_quota(0);
        let orders_link = r#"href="/account/traffic""#;
        for management_enabled in [false, true] {
            let without_quota =
                account_panel(Some(&account), "csrf", true, management_enabled, None);
            assert_eq!(without_quota.matches(orders_link).count(), 1);
            let with_quota = account_panel(
                Some(&account),
                "csrf",
                true,
                management_enabled,
                Some(&quota),
            );
            assert!(!with_quota.contains(orders_link));
        }
        assert!(!account_panel(None, "csrf", true, true, None).contains(orders_link));
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
        assert!(html.contains(r#"<dt>流量额度</dt><dd aria-label="无限">∞</dd>"#));
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

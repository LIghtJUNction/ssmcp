//! Server-rendered account and administrator consoles; never trusts UI roles.
use super::{
    store::AccountSummary,
    view::{escape, page, traffic_panel},
};
use crate::{
    history::{AuditAction, AuditEvent, AuditOutcome},
    management::{DashboardSnapshot, Scope, SessionSnapshot},
};
use std::collections::BTreeMap;

fn navigation(active: &str, admin: bool) -> String {
    let mut html = String::from(r#"<nav class="console-nav" aria-label="账户管理">"#);
    for (path, label) in [
        ("/", "我的账户"),
        ("/account/sessions", "我的会话"),
        ("/account/history", "历史记录"),
        ("/account/admin", "管理员"),
    ] {
        if path == "/account/admin" && !admin {
            continue;
        }
        let current = if path == active {
            r#" aria-current="page""#
        } else {
            ""
        };
        html.push_str(&format!(r#"<a href="{path}"{current}>{label}</a>"#));
    }
    html.push_str("</nav>");
    html
}

fn timestamp(seconds: u64) -> String {
    if seconds > 253_402_300_799 {
        return "时间不可用".into();
    }
    // Gregorian civil date from days since the Unix epoch.
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn heading(snapshot: &DashboardSnapshot, title: &str, description: &str, path: &str) -> String {
    format!(
        r#"<header class="dashboard-head"><div><p class="eyebrow">{}</p><h1>{title}</h1><p>{description}</p></div><div class="snapshot-meta"><span>更新于 {}</span><a class="text-link" href="{path}">刷新快照 <span class="arrow" aria-hidden="true">↻</span></a></div></header>"#,
        if snapshot.scope == Scope::All {
            "管理员工作台"
        } else {
            "账户管理"
        },
        timestamp(snapshot.generated_at)
    )
}

fn metric(value: usize, label: &str) -> String {
    format!(r#"<div class="metric"><dt>{label}</dt><dd>{value}</dd></div>"#)
}

fn session_cards(
    snapshot: &DashboardSnapshot,
    csrf: &str,
    return_to: &str,
    accounts: &[AccountSummary],
) -> String {
    if snapshot.sessions.is_empty() {
        return String::from(
            r#"<div class="console-empty"><span class="empty-prompt" aria-hidden="true">&gt;_</span><h3>当前没有在线会话</h3><p>在本机启动 ssmcp 后，刷新这里查看连接。</p><a class="text-link" href="/#connect">查看连接命令 ↗</a></div>"#,
        );
    }
    let owners: BTreeMap<_, _> = accounts.iter().map(|a| (a.subject.as_str(), a)).collect();
    let mut html = String::from(r#"<div class="session-list">"#);
    for session in &snapshot.sessions {
        html.push_str(&session_card(
            session,
            csrf,
            return_to,
            snapshot.scope == Scope::All,
            session
                .owner_subject
                .as_deref()
                .and_then(|id| owners.get(id).copied()),
        ));
    }
    html.push_str("</div>");
    html
}

fn session_card(
    session: &SessionSnapshot,
    csrf: &str,
    return_to: &str,
    admin: bool,
    account: Option<&AccountSummary>,
) -> String {
    let username = account
        .map(|a| a.handle.as_deref().unwrap_or(&a.username))
        .unwrap_or(&session.ssh_user);
    let mut html = format!(
        r#"<article class="session-card" id="session-{}"><header class="session-heading"><div><h3>{}</h3><p class="muted">连接于 {}</p></div><span class="status signed-in">在线 · {}</span></header>
<dl class="session-facts"><div class="session-id"><dt>会话 ID</dt><dd><code>{}</code></dd></div><div><dt>主机</dt><dd><code>{}</code></dd></div><div><dt>服务器用户</dt><dd><code>{}</code></dd></div><div class="session-path"><dt>工作目录</dt><dd><code>{}</code></dd></div>"#,
        escape(&session.id),
        escape(&session.name),
        timestamp(session.connected_at),
        escape(&session.mode),
        escape(&session.id),
        escape(&session.hostname),
        escape(username),
        escape(&session.cwd),
    );
    if admin {
        let owner = account
            .map(|a| {
                format!(
                    "{} · GitHub #{}",
                    escape(&a.github_login),
                    escape(&a.subject)
                )
            })
            .or_else(|| {
                session
                    .owner_subject
                    .as_ref()
                    .map(|id| format!("GitHub #{}", escape(id)))
            })
            .unwrap_or_else(|| "尚未绑定账户".into());
        html.push_str(&format!(r#"<div><dt>归属</dt><dd>{owner}</dd></div>"#));
    }
    html.push_str("</dl>");
    html.push_str(
        r#"<details class="session-access"><summary>获准使用工具的 GitHub 账户</summary>"#,
    );
    if session.allowed_github.is_empty() {
        html.push_str("<p>此会话使用登录码授权；访问权与会话管理权分别校验。</p>");
    } else {
        html.push_str(r#"<ul class="principal-list">"#);
        for identity in &session.allowed_github {
            html.push_str(&format!(
                r#"<li><code>{}</code><span>GitHub #{}</span></li>"#,
                escape(&identity.login),
                escape(&identity.subject)
            ));
        }
        html.push_str("</ul><p>此名单授予工具访问权；会话管理权由服务器验证的账户归属决定。</p>");
    }
    html.push_str(&format!(
        r#"</details><details class="disconnect-control"><summary>断开此会话</summary><form method="post" action="/account/sessions/disconnect"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="session_id" value="{}"><input type="hidden" name="return_to" value="{}"><p>将关闭当前工具连接。启用了自动重连的客户端可能重新连接；管理员可封禁用户，阻止继续访问。</p><button class="danger" type="submit">确认断开会话</button></form></details></article>"#,
        escape(csrf), escape(&session.id), escape(return_to)
    ));
    html
}

pub(super) fn sessions(snapshot: &DashboardSnapshot, csrf: &str) -> String {
    let mut body = navigation("/account/sessions", snapshot.is_admin);
    body.push_str(&heading(
        snapshot,
        "我的会话",
        "查看属于你的在线连接，检查工作目录，按需断开会话。",
        "/account/sessions",
    ));
    body.push_str(r#"<dl class="metrics">"#);
    body.push_str(&metric(snapshot.stats.active_sessions, "在线会话"));
    body.push_str(&metric(snapshot.stats.hosts, "报告的主机"));
    body.push_str(&metric(snapshot.retention_days as usize, "历史保留天数"));
    body.push_str("</dl>");
    if let Some(traffic) = &snapshot.traffic {
        body.push_str(r#"<section class="dashboard-section" aria-label="账户流量"><div class="dashboard-section-heading"><h2>账户流量</h2></div>"#);
        body.push_str(&traffic_panel(traffic));
        body.push_str("</section>");
    }
    body.push_str(r#"<section class="dashboard-section"><div class="dashboard-section-heading"><h2>在线连接</h2><a class="text-link" href="/account/history">查看连接历史 ↗</a></div><p class="dashboard-note">名称、主机和工作目录由客户端报告；会话归属按服务器验证的账户确认。</p>"#);
    body.push_str(&session_cards(snapshot, csrf, "/account/sessions", &[]));
    body.push_str("</section>");
    page("我的会话", &body)
}

fn action_label(action: AuditAction) -> &'static str {
    match action {
        AuditAction::Login => "账户登录",
        AuditAction::Logout => "退出登录",
        AuditAction::KeyAdded => "添加公钥",
        AuditAction::KeyRemoved => "删除公钥",
        AuditAction::AccessDenied => "访问被拒绝",
        AuditAction::SessionConnected => "会话已连接",
        AuditAction::SessionDisconnected => "会话已断开",
        AuditAction::SessionDisconnectRequested => "请求断开会话",
        AuditAction::ToolCall => "调用工具",
        AuditAction::AdminViewed => "查看管理员数据",
        AuditAction::UserPolicyChanged => "用户策略已更新",
        AuditAction::TrafficBalanceChanged => "调整用户流量余额",
    }
}

fn audit_rows(events: &[AuditEvent], admin: bool) -> String {
    if events.is_empty() {
        return String::from(
            r#"<div class="console-empty"><h3>保留期内还没有记录</h3><p>新的登录、公钥变更和连接活动会出现在这里。</p></div>"#,
        );
    }
    let mut html = String::from(r#"<ol class="audit-list" aria-label="审计事件">"#);
    for event in events {
        let (outcome, class) = match event.outcome {
            AuditOutcome::Success => ("成功", "success"),
            AuditOutcome::Denied => ("已拒绝", "denied"),
            AuditOutcome::Error => ("失败", "error"),
            AuditOutcome::Pending => ("已记录请求", "pending"),
        };
        html.push_str(&format!(
            r#"<li class="audit-row"><div class="audit-time"><span>{}</span><small>事件 #{}</small></div><div class="audit-event"><strong>{}</strong>"#,
            timestamp(event.at_unix), event.id, action_label(event.action)
        ));
        if let Some(tool) = &event.tool_name {
            html.push_str(&format!("<code>{}</code>", escape(tool)));
        }
        html.push_str(&format!(
            r#"</div><span class="audit-outcome {class}">{outcome}</span><div class="audit-context">"#
        ));
        if let Some(name) = &event.client_name {
            html.push_str(&format!("<span>{}</span>", escape(name)));
        }
        if let Some(hostname) = &event.hostname {
            html.push_str(&format!("<small>{}</small>", escape(hostname)));
        }
        if let Some(id) = &event.session_id {
            let short: String = id.chars().take(12).collect();
            html.push_str(&format!(
                r#"<details class="audit-id"><summary><code>{}…</code></summary><code>{}</code></details>"#,
                escape(&short), escape(id)
            ));
        }
        if admin {
            let actor = event
                .actor_subject
                .as_deref()
                .map(escape)
                .unwrap_or_else(|| "未记录 GitHub 操作者".into());
            html.push_str(&format!(r#"<small>操作者：{actor}</small>"#));
            if let Some(owner) = &event.owner_subject {
                html.push_str(&format!("<small>归属：GitHub #{}</small>", escape(owner)));
            }
        }
        html.push_str("</div></li>");
    }
    html.push_str("</ol>");
    html
}

pub(super) fn history(snapshot: &DashboardSnapshot) -> String {
    let all = snapshot.scope == Scope::All;
    let path = if all {
        "/account/admin/history"
    } else {
        "/account/history"
    };
    let title = if all { "安全审计" } else { "历史记录" };
    let active = if all { "/account/admin" } else { path };
    let description = if all {
        "查看服务范围内的账号活动、连接历史和安全事件。"
    } else {
        "查看自己的账号操作，以及所属会话的连接与工具调用记录。"
    };
    let mut body = navigation(active, snapshot.is_admin);
    body.push_str(&heading(snapshot, title, description, path));
    body.push_str(&format!(
        r#"<div class="retention-notice"><span class="retention-symbol" aria-hidden="true">◷</span><p>最多保留 <strong>{} 天</strong>，并按数量与空间上限提前轮转。自动清理；不记录命令正文、工具参数、执行结果或凭据。</p></div><section class="dashboard-section"><div class="dashboard-section-heading"><h2>最近活动</h2><span class="muted">当前页 {} 条 · UTC 时间</span></div>"#,
        snapshot.retention_days, snapshot.audit.len()
    ));
    body.push_str(&audit_rows(&snapshot.audit, all));
    body.push_str("</section>");
    body.push_str(&format!(r#"<nav class="history-pagination" aria-label="历史分页"><a class="text-link" href="{path}">最新记录</a>"#));
    if let Some(before) = snapshot.audit_next_before {
        body.push_str(&format!(r#"<a class="button secondary" href="{path}?before={before}">更早的记录 <span class="arrow" aria-hidden="true">→</span></a>"#));
    }
    body.push_str("</nav>");
    page(title, &body)
}

pub(super) fn mib_per_second(bytes: u64) -> String {
    let whole = bytes / 1_048_576;
    let remainder = bytes % 1_048_576;
    if remainder == 0 {
        return whole.to_string();
    }
    // One byte is exactly 0.00000095367431640625 MiB.
    let fraction = u128::from(remainder) * 95_367_431_640_625;
    format!("{whole}.{fraction:020}")
        .trim_end_matches('0')
        .into()
}

fn user_row(
    account: &AccountSummary,
    sessions: &[&SessionSnapshot],
    csrf: &str,
    actor_subject: &str,
    default_rate_limit_bps: u64,
    period: &str,
) -> String {
    let username = escape(account.handle.as_deref().unwrap_or(&account.username));
    let subject = escape(&account.subject);
    let csrf = escape(csrf);
    let own_account = account.subject == actor_subject;
    let mut html = format!(
        r#"<tr><th scope="row"><code>{username}</code>{}</th><td data-label="GitHub"><span>{}</span><small class="user-identifier">GitHub #{subject}</small></td><td data-label="连接">"#,
        if own_account {
            r#"<small class="user-self">你</small>"#
        } else if account.handle.is_none() {
            r#"<small class="user-identifier">待设置用户名</small>"#
        } else {
            ""
        },
        escape(&account.github_login),
    );
    if sessions.is_empty() {
        html.push_str(r#"<span class="muted">0</span>"#);
    } else {
        html.push_str(&format!(
            r#"<details class="user-connections"><summary>{} 在线</summary><ul>"#,
            sessions.len()
        ));
        for session in sessions {
            html.push_str(&format!(
                r##"<li><a href="#session-{}">{}</a></li>"##,
                escape(&session.id),
                escape(&session.name)
            ));
        }
        html.push_str("</ul></details>");
    }
    let (state_class, state) = if account.banned {
        ("banned", "已封禁")
    } else {
        ("active", "正常")
    };
    let value = account
        .rate_limit_bps
        .map(mib_per_second)
        .unwrap_or_default();
    let default_rate = mib_per_second(if account.has_recharged {
        default_rate_limit_bps.max(crate::user_policy::RECHARGED_RATE_LIMIT_BPS)
    } else {
        default_rate_limit_bps
    });
    html.push_str(&format!(
        r#"</td><td data-label="状态"><span class="account-state {state_class}">{state}</span></td><td class="user-rate-cell" data-label="限速 · MiB/s"><form class="user-rate-form" method="post" action="/account/admin/users"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="subject" value="{subject}"><input type="hidden" name="action" value="rate"><label class="visually-hidden" for="rate-{subject}">{username} 的限速，MiB/s</label><div class="rate-input"><input id="rate-{subject}" type="number" name="rate_mib" min="0.00000095367431640625" max="1024" step="any" inputmode="decimal" value="{value}" placeholder="默认 {default_rate}" aria-describedby="rate-help" autocomplete="off"><button class="secondary" type="submit">保存</button></div><small>{}</small></form></td><td class="user-rate-cell" data-label="当月流量余额">"#,
        if account.rate_limit_bps.is_some() { "自定义 · 留空恢复默认" } else if account.has_recharged { "已充值 · 使用充值默认限速" } else { "使用服务器默认" },
    ));
    if let Some(remaining) = account.traffic_remaining {
        let period = escape(period);
        html.push_str(&format!(
            r#"<form class="user-rate-form" method="post" action="/account/admin/users"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="subject" value="{subject}"><input type="hidden" name="action" value="traffic"><input type="hidden" name="period" value="{period}"><label class="visually-hidden" for="traffic-{subject}">{username} 当月剩余流量，字节</label><div class="rate-input"><input id="traffic-{subject}" type="text" inputmode="numeric" pattern="[0-9]+" maxlength="20" name="remaining_bytes" value="{remaining}" required><button class="secondary" type="submit">保存</button></div><small>{period} · {} · 输入单位为字节</small></form>"#,
            super::view::traffic_bytes(remaining),
        ));
    } else if period.is_empty() {
        html.push_str(r#"<span class="muted">不可用</span>"#);
    } else {
        html.push_str(r#"<span class="muted">不限量</span>"#);
    }
    html.push_str(r#"</td><td class="user-actions" data-label="访问管理">"#);
    html.push_str(&user_access_control(
        account,
        &username,
        &subject,
        &csrf,
        own_account,
    ));
    html.push_str("</td></tr>");
    html
}

// Attribute values and username are escaped once by user_row.
fn user_access_control(
    account: &AccountSummary,
    username: &str,
    subject: &str,
    csrf: &str,
    own_account: bool,
) -> String {
    if own_account {
        r#"<span class="muted">当前账户</span>"#.into()
    } else if account.banned {
        format!(
            r#"<form method="post" action="/account/admin/users"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="subject" value="{subject}"><input type="hidden" name="action" value="unban"><button class="secondary" type="submit">解除封禁</button></form>"#,
        )
    } else {
        format!(
            r#"<details class="user-ban"><summary>封禁用户</summary><form method="post" action="/account/admin/users"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="subject" value="{subject}"><input type="hidden" name="action" value="ban"><p>封禁 <strong>{username}</strong> 后，将断开其全部连接并阻止继续访问。</p><button class="danger" type="submit">确认封禁</button></form></details>"#,
        )
    }
}

pub(super) fn admin(
    snapshot: &DashboardSnapshot,
    accounts: &[AccountSummary],
    csrf: &str,
    actor_subject: &str,
    period: &str,
) -> String {
    let mut body = navigation("/account/admin", true);
    body.push_str(&heading(
        snapshot,
        "服务器管理",
        "管理这台服务器上的用户与连接。",
        "/account/admin",
    ));
    body.push_str(r#"<dl class="metrics metrics-four">"#);
    body.push_str(&metric(accounts.len(), "注册用户"));
    body.push_str(&metric(snapshot.stats.active_sessions, "在线连接"));
    body.push_str(&metric(
        accounts.iter().filter(|a| a.banned).count(),
        "已封禁用户",
    ));
    body.push_str(&metric(snapshot.stats.hosts, "在线主机"));
    body.push_str(r##"</dl><nav class="section-links" aria-label="管理员区域"><a href="#registered-users">用户</a><a href="#all-sessions">连接</a><a href="/account/admin/history">安全审计 ↗</a></nav>"##);
    body.push_str(r#"<section class="dashboard-section" id="registered-users"><div class="dashboard-section-heading"><h2>用户</h2><span class="muted">账号绑定 GitHub 身份</span></div>"#);
    body.push_str(&format!(
        r#"<p class="dashboard-note" id="rate-help">网络限速按每位用户的全部连接合计，上传、下载分别计量。服务器默认 <strong>{} MiB/s</strong>；留空使用默认值。1 MiB = 1,048,576 字节。</p>"#,
        mib_per_second(snapshot.default_rate_limit_bps),
    ));
    if accounts.is_empty() {
        body.push_str(r#"<div class="console-empty"><h3>尚无注册用户</h3><p>首次完成 GitHub 登录后，账户会出现在这里。</p></div>"#);
    } else {
        let mut sessions: BTreeMap<&str, Vec<&SessionSnapshot>> = BTreeMap::new();
        for session in &snapshot.sessions {
            if let Some(subject) = session.owner_subject.as_deref() {
                sessions.entry(subject).or_default().push(session);
            }
        }
        body.push_str(r#"<div class="table-scroll" tabindex="0" role="region" aria-label="用户管理表"><table class="users-table user-management-table"><thead><tr><th scope="col">用户名</th><th scope="col">GitHub</th><th scope="col">连接</th><th scope="col">状态</th><th scope="col">限速 · MiB/s</th><th scope="col">当月流量余额</th><th scope="col">访问管理</th></tr></thead><tbody>"#);
        for account in accounts {
            body.push_str(&user_row(
                account,
                sessions
                    .get(account.subject.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                csrf,
                actor_subject,
                snapshot.default_rate_limit_bps,
                period,
            ));
        }
        body.push_str("</tbody></table></div>");
    }
    body.push_str(r#"</section><section class="dashboard-section" id="all-sessions"><div class="dashboard-section-heading"><h2>连接</h2><a class="text-link" href="/account/admin/history">查看连接历史 ↗</a></div><p class="dashboard-note">名称、主机和工作目录由客户端报告。断开只关闭当前连接；封禁用户会阻止其重新连接。</p>"#);
    body.push_str(&session_cards(snapshot, csrf, "/account/admin", accounts));
    body.push_str("</section>");
    page("服务器管理", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates_cover_epoch_leap_day_and_year_boundary() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(timestamp(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(timestamp(1_735_689_599), "2024-12-31 23:59:59 UTC");
        assert_eq!(timestamp(253_402_300_800), "时间不可用");
    }

    fn sample_account() -> AccountSummary {
        AccountSummary {
            subject: "123".into(),
            username: "gh-123".into(),
            handle: Some("developer".into()),
            github_login: "octocat".into(),
            key_count: 0,
            banned: false,
            rate_limit_bps: Some(524_288),
            has_recharged: false,
            traffic_remaining: None,
        }
    }

    fn sample_session() -> SessionSnapshot {
        SessionSnapshot {
            id: "a".repeat(64),
            name: "build-machine".into(),
            hostname: "workstation".into(),
            cwd: "/workspace".into(),
            mode: "shell".into(),
            ssh_user: "gh-123".into(),
            owner_subject: Some("123".into()),
            connected_at: 1_700_000_000,
            allowed_github: vec![],
        }
    }

    #[test]
    fn user_actions_are_independent_and_hide_self_ban() {
        let mut account = sample_account();
        let row = user_row(&account, &[], "csrf", "999", 8 * 1_048_576, "2026-11");
        assert!(row.contains(r#"value="0.5""#));
        assert!(row.contains(r#"name="action" value="rate""#));
        assert!(row.contains(r#"name="action" value="ban""#));
        assert!(!row.contains(r#"name="banned""#));
        account.banned = true;
        let row = user_row(&account, &[], "csrf", "999", 8 * 1_048_576, "2026-11");
        assert!(row.contains("已封禁"));
        assert!(row.contains(r#"name="action" value="unban""#));
        assert!(!row.contains(r#"name="action" value="ban""#));
        let own = user_row(&account, &[], "csrf", "123", 8 * 1_048_576, "2026-11");
        assert!(own.contains("当前账户"));
        assert!(!own.contains(r#"name="action" value="unban""#));
        assert!(!own.contains(r#"name="action" value="ban""#));
        account.rate_limit_bps = None;
        let row = user_row(&account, &[], "csrf", "999", 4 * 1_048_576, "2026-11");
        assert!(row.contains(r#"value="" placeholder="默认 4""#));
    }

    #[test]
    fn user_and_connection_metadata_are_escaped_and_linked_by_subject() {
        let mut account = sample_account();
        account.handle = Some("<script>".into());
        account.github_login = r#"<img src=x onerror="alert(1)">"#.into();
        let mut session = sample_session();
        session.name = "<svg onload=alert(1)>".into();
        let row = user_row(
            &account,
            &[&session],
            r#"csrf" injected="#,
            "999",
            8 * 1_048_576,
            "2026-11",
        );
        for raw in ["<script>", "<img", "<svg", r#"value="csrf" injected="#] {
            assert!(!row.contains(raw), "{raw}");
        }
        assert!(row.contains("&lt;script&gt;"));
        assert!(row.contains("&lt;img"));
        assert!(row.contains("&lt;svg"));
        assert!(row.contains(&format!(r##"href="#session-{}""##, session.id)));
        let card = session_card(&session, "csrf", "/account/admin", true, Some(&account));
        assert!(card.contains(&format!(r#"id="session-{}""#, session.id)));
        assert!(card.contains("<dt>服务器用户</dt><dd><code>&lt;script&gt;</code>"));
        assert!(!card.contains("<svg"));
        assert!(!card.contains("<img"));
        assert!(!card.contains("<script>"));
    }

    #[test]
    fn admin_uses_server_default_and_counts_actual_owner_connections() {
        let account = sample_account();
        let own = sample_session();
        let mut foreign = sample_session();
        foreign.id = "b".repeat(64);
        foreign.owner_subject = Some("456".into());
        foreign.ssh_user = "gh-456".into();
        let snapshot = DashboardSnapshot {
            is_admin: true,
            scope: Scope::All,
            generated_at: 1_700_000_000,
            sessions: vec![own, foreign],
            stats: crate::management::DashboardStats {
                active_sessions: 2,
                owners: 2,
                hosts: 1,
            },
            audit: vec![],
            audit_next_before: None,
            retention_days: 7,
            default_rate_limit_bps: 3 * 1_048_576,
            traffic: None,
        };
        let html = admin(&snapshot, &[account], "csrf", "999", "2026-11");
        assert!(html.contains("服务器默认 <strong>3 MiB/s</strong>"));
        assert!(html.contains("<summary>1 在线</summary>"));
        assert!(!html.contains("<summary>2 在线</summary>"));
        assert!(html.contains("<dt>服务器用户</dt><dd><code>developer</code>"));
    }

    fn quota_snapshot(traffic: Option<crate::user_policy::TrafficQuota>) -> DashboardSnapshot {
        DashboardSnapshot {
            is_admin: false,
            scope: Scope::Own,
            generated_at: 1_700_000_000,
            sessions: vec![],
            stats: crate::management::DashboardStats {
                active_sessions: 0,
                owners: 0,
                hosts: 0,
            },
            audit: vec![],
            audit_next_before: None,
            retention_days: 7,
            default_rate_limit_bps: 8 * 1_048_576,
            traffic,
        }
    }

    fn monthly_traffic(used_bytes: u64) -> crate::user_policy::TrafficQuota {
        crate::user_policy::TrafficQuota {
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

    #[test]
    fn sessions_show_account_monthly_remaining_and_used_traffic() {
        let snapshot = quota_snapshot(Some(monthly_traffic(950_000_000)));
        let html = sessions(&snapshot, "csrf");
        assert!(html.contains("<h2>账户流量</h2>"));
        assert!(html.contains(r#"title="50000000 字节">50 MB</dd>"#));
        assert!(html.contains(r#"title="950000000 字节">950 MB</dd>"#));
        assert!(html.contains("2026-11（UTC+08:00 自然月）"));
        assert!(html.contains("月底到期，不滚存"));
        assert!(html.contains("服务器流量有限，经常使用请购买流量包。"));
    }

    #[test]
    fn sessions_show_unlimited_for_server_supplied_admin_quota() {
        let mut traffic = monthly_traffic(2_000_000_000);
        traffic.limit_bytes = None;
        traffic.remaining_bytes = None;
        let mut snapshot = quota_snapshot(Some(traffic));
        snapshot.is_admin = true;
        let html = sessions(&snapshot, "csrf");
        assert!(html.contains(r#"<dt>流量额度</dt><dd aria-label="无限">∞</dd>"#));
        assert!(html.contains(r#"title="2000000000 字节">2 GB</dd>"#));
        assert!(!html.contains("购买流量包"));
        assert!(!html.contains("本月流量已用完"));
    }

    #[test]
    fn sessions_show_exhaustion_and_do_not_invent_missing_quota() {
        let exhausted = sessions(
            &quota_snapshot(Some(monthly_traffic(1_000_000_000))),
            "csrf",
        );
        assert!(exhausted.contains("本月流量已用完。"));
        assert!(exhausted.contains(r#"title="0 字节">0 字节</dd>"#));
        let unavailable = sessions(&quota_snapshot(None), "csrf");
        assert!(!unavailable.contains("<h2>账户流量</h2>"));
        assert!(!unavailable.contains("无限"));
        assert!(!unavailable.contains('∞'));
        assert!(!unavailable.contains("购买流量包"));
    }
}

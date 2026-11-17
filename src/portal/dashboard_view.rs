//! Server-rendered account and administrator consoles; never trusts UI roles.
use super::{
    store::AccountSummary,
    view::{escape, page},
};
use crate::{
    history::{AuditAction, AuditEvent, AuditOutcome},
    management::{DashboardSnapshot, Scope, SessionSnapshot},
};
use std::collections::BTreeMap;

fn navigation(active: &str, admin: bool) -> String {
    let mut html = String::from(r#"<nav class="console-nav" aria-label="账户管理">"#);
    for (path, label) in [
        ("/", "账户与公钥"),
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
        r#"<header class="dashboard-head"><div><p class="eyebrow">CONTROL ROOM / {}</p><h1>{title}</h1><p>{description}</p></div><div class="snapshot-meta"><span>更新于 {}</span><a class="text-link" href="{path}">刷新快照 <span class="arrow" aria-hidden="true">↻</span></a></div></header>"#,
        if snapshot.scope == Scope::All {
            "ADMIN"
        } else {
            "ACCOUNT"
        },
        timestamp(snapshot.generated_at)
    )
}

fn metric(value: usize, label: &str) -> String {
    format!(r#"<div class="metric"><dt>{label}</dt><dd>{value}</dd></div>"#)
}

fn session_cards(snapshot: &DashboardSnapshot, csrf: &str, return_to: &str) -> String {
    if snapshot.sessions.is_empty() {
        return String::from(
            r#"<div class="console-empty"><span class="empty-prompt" aria-hidden="true">&gt;_</span><h3>当前没有在线会话</h3><p>在本机启动 ssmcp 后，刷新这里查看连接。</p><a class="text-link" href="/#connect">查看连接命令 ↗</a></div>"#,
        );
    }
    let mut html = String::from(r#"<div class="session-list">"#);
    for session in &snapshot.sessions {
        html.push_str(&session_card(
            session,
            csrf,
            return_to,
            snapshot.scope == Scope::All,
        ));
    }
    html.push_str("</div>");
    html
}

fn session_card(session: &SessionSnapshot, csrf: &str, return_to: &str, admin: bool) -> String {
    let mut html = format!(
        r#"<article class="session-card"><header class="session-heading"><div><h3>{}</h3><p class="muted">连接于 {}</p></div><span class="status signed-in">在线 · {}</span></header>
<dl class="session-facts"><div class="session-id"><dt>会话 ID</dt><dd><code>{}</code></dd></div><div><dt>主机</dt><dd><code>{}</code></dd></div><div><dt>SSH 账户</dt><dd><code>{}</code></dd></div><div class="session-path"><dt>工作目录</dt><dd><code>{}</code></dd></div>"#,
        escape(&session.name),
        timestamp(session.connected_at),
        escape(&session.mode),
        escape(&session.id),
        escape(&session.hostname),
        escape(&session.ssh_user),
        escape(&session.cwd),
    );
    if admin {
        let owner = session
            .owner_subject
            .as_ref()
            .map(|id| format!("GitHub #{}", escape(id)))
            .unwrap_or_else(|| "尚未绑定门户账户".into());
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
        html.push_str("</ul><p>此名单授予工具访问权；会话管理权由 SSH 账户归属决定。</p>");
    }
    html.push_str(&format!(
        r#"</details><details class="disconnect-control"><summary>断开此会话</summary><form method="post" action="/account/sessions/disconnect"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="session_id" value="{}"><input type="hidden" name="return_to" value="{}"><p>将关闭当前 SSH 工具连接。启用了看门狗的客户端可能重新连接；持续停用请在本机关闭客户端。</p><button class="danger" type="submit">确认断开会话</button></form></details></article>"#,
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
    body.push_str(r#"</dl><section class="dashboard-section"><div class="dashboard-section-heading"><h2>在线连接</h2><a class="text-link" href="/account/history">查看连接历史 ↗</a></div><p class="dashboard-note">名称、主机和工作目录由客户端报告；会话归属按服务器验证的 SSH 账户确认。</p>"#);
    body.push_str(&session_cards(snapshot, csrf, "/account/sessions"));
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

fn topology(snapshot: &DashboardSnapshot) -> String {
    let mut html = String::from(
        r#"<section class="dashboard-section" id="topology"><div class="dashboard-section-heading"><h2>连接拓扑</h2><span class="muted">网关 → SSH 账户 → 会话</span></div><p class="dashboard-note">展示当前实际登记的连接及授权关系；设备名称与主机名来自客户端报告。</p>"#,
    );
    if snapshot.sessions.is_empty() {
        html.push_str(r#"<div class="console-empty"><h3>网关当前没有在线客户端</h3><p>客户端连接后，拓扑会自动出现在新的快照中。</p></div></section>"#);
        return html;
    }
    html.push_str(r#"<div class="topology"><div class="topology-root"><span class="topology-symbol" aria-hidden="true">&gt;_</span><div><strong>ssmcp 网关</strong><code>MCP / SSH</code></div></div><div class="topology-branches">"#);
    let mut owners: BTreeMap<&str, Vec<&SessionSnapshot>> = BTreeMap::new();
    for session in &snapshot.sessions {
        owners.entry(&session.ssh_user).or_default().push(session);
    }
    for (owner, sessions) in owners {
        html.push_str(&format!(
            r#"<section class="topology-owner"><h3><span>SSH</span><code>{}</code></h3><ul>"#,
            escape(owner)
        ));
        for session in sessions {
            html.push_str(&format!(
                r#"<li><div class="topology-session"><div><strong>{}</strong><span class="status signed-in">{}</span></div><p><code>{}</code> · {}</p><details class="audit-id"><summary>会话 ID</summary><code>{}</code></details><p class="topology-allow">授权 GitHub： "#,
                escape(&session.name), escape(&session.mode), escape(&session.hostname),
                timestamp(session.connected_at), escape(&session.id)
            ));
            if session.allowed_github.is_empty() {
                html.push_str("登录码模式");
            } else {
                for (index, identity) in session.allowed_github.iter().enumerate() {
                    if index != 0 {
                        html.push('、');
                    }
                    html.push_str(&format!(
                        "<code>{}</code> <span>#{}</span>",
                        escape(&identity.login),
                        escape(&identity.subject)
                    ));
                }
            }
            html.push_str("</p></div></li>");
        }
        html.push_str("</ul></section>");
    }
    html.push_str("</div></div></section>");
    html
}

pub(super) fn admin(
    snapshot: &DashboardSnapshot,
    accounts: &[AccountSummary],
    csrf: &str,
) -> String {
    let mut body = navigation("/account/admin", true);
    body.push_str(&heading(
        snapshot,
        "管理员控制台",
        "查看注册用户、在线连接与授权拓扑，处理需要关注的安全事件。",
        "/account/admin",
    ));
    body.push_str(r#"<dl class="metrics metrics-four">"#);
    body.push_str(&metric(accounts.len(), "注册用户"));
    body.push_str(&metric(snapshot.stats.active_sessions, "在线会话"));
    body.push_str(&metric(snapshot.stats.hosts, "报告的主机"));
    body.push_str(&metric(snapshot.stats.owners, "在线 SSH 账户"));
    body.push_str(r##"</dl><nav class="section-links" aria-label="管理员区域"><a href="#topology">连接拓扑</a><a href="#registered-users">注册用户</a><a href="#all-sessions">会话管理</a><a href="/account/admin/history">安全审计 ↗</a></nav>"##);
    body.push_str(&topology(snapshot));
    body.push_str(r#"<section class="dashboard-section" id="registered-users"><div class="dashboard-section-heading"><h2>注册用户</h2><span class="muted">仅显示账户摘要</span></div>"#);
    if accounts.is_empty() {
        body.push_str(r#"<div class="console-empty"><h3>尚无注册用户</h3><p>首次完成 GitHub 登录后，账户会出现在这里。</p></div>"#);
    } else {
        body.push_str(r#"<div class="table-scroll" tabindex="0" role="region" aria-label="注册用户表"><table class="users-table"><thead><tr><th scope="col">GitHub 用户</th><th scope="col">SSH 用户名</th><th scope="col">GitHub ID</th><th scope="col">公钥数量</th></tr></thead><tbody>"#);
        for account in accounts {
            body.push_str(&format!(
                "<tr><th scope=\"row\">{}</th><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
                escape(&account.github_login), escape(&account.username), escape(&account.subject), account.key_count
            ));
        }
        body.push_str("</tbody></table></div>");
    }
    body.push_str(r#"</section><section class="dashboard-section" id="all-sessions"><div class="dashboard-section-heading"><h2>会话管理</h2><a class="text-link" href="/account/admin/history">查看安全审计 ↗</a></div>"#);
    body.push_str(&session_cards(snapshot, csrf, "/account/admin"));
    body.push_str("</section>");
    page("管理员控制台", &body)
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
}

//! Read-only account and monthly traffic status for the connected server.
use super::{Api, Credential, Method, StatusCode, Url, Value, public_identity};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) enum Traffic {
    Unknown,
    Unlimited,
    Remaining(u64),
}

impl Traffic {
    fn from_reply(reply: &Value) -> Self {
        let Some(traffic) = reply.get("traffic").and_then(Value::as_object) else {
            return Self::Unknown;
        };
        match (traffic.get("limit_bytes"), traffic.get("remaining_bytes")) {
            (Some(Value::Null), Some(Value::Null)) => Self::Unlimited,
            (Some(limit), Some(remaining)) => match (limit.as_u64(), remaining.as_u64()) {
                (Some(limit), Some(remaining)) if remaining <= limit => Self::Remaining(remaining),
                _ => Self::Unknown,
            },
            _ => Self::Unknown,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Unknown => "未提供".into(),
            Self::Unlimited => "无限".into(),
            Self::Remaining(0) => "0 GB".into(),
            Self::Remaining(bytes) if *bytes < 10_000_000 => "<0.01 GB".into(),
            Self::Remaining(bytes) => {
                // Decimal GB, rounded down so the display never overstates available traffic.
                format!(
                    "{}.{:02} GB",
                    bytes / 1_000_000_000,
                    (bytes % 1_000_000_000) / 10_000_000
                )
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) enum Status {
    Loading,
    SignedOut,
    Unavailable,
    Ready {
        login: String,
        username: Option<String>,
        traffic: Traffic,
    },
}

/// The live connection selects the origin. Never use the default server here.
pub(in crate::client) fn server_from_endpoint(endpoint: &str) -> Option<String> {
    if endpoint.len() > 2048
        || endpoint
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace() || byte == b'\\')
    {
        return None;
    }
    let endpoint = Url::parse(endpoint).ok()?;
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return None;
    }
    super::normalize_server(&endpoint.origin().ascii_serialization()).ok()
}

impl Status {
    pub(in crate::client) async fn refresh(server: &str) -> Self {
        let Ok(root) = super::state_root() else {
            return Self::Unavailable;
        };
        Self::refresh_at(&root, server).await
    }

    async fn refresh_at(root: &std::path::Path, server: &str) -> Self {
        // Same selected-server loader as credentials(); the live origin is always explicit.
        match super::load_at(root, Some(server), false) {
            Ok(Some(credential)) => Self::fetch(&credential).await,
            Ok(None) => Self::SignedOut,
            Err(_) => Self::Unavailable,
        }
    }

    async fn fetch(credential: &Credential) -> Self {
        let Ok(api) = Api::new(&credential.server) else {
            return Self::Unavailable;
        };
        // Reuse the account API's redirect policy, TLS trust, deadline and body bound.
        // This path never authorizes, launches a browser or logs a response/token.
        let Ok(reply) = api
            .request(
                Method::GET,
                "/api/client/whoami",
                Some(&credential.token),
                None,
            )
            .await
        else {
            return Self::Unavailable;
        };
        if reply.status == StatusCode::UNAUTHORIZED {
            return Self::SignedOut;
        }
        let Ok(value) = Api::checked(reply) else {
            return Self::Unavailable;
        };
        Self::from_reply(&value, credential)
    }

    fn from_reply(reply: &Value, credential: &Credential) -> Self {
        let Ok(identity) = public_identity(reply) else {
            return Self::Unavailable;
        };
        if identity["principal"].as_str() != Some(credential.principal.as_str())
            || credential
                .subject
                .as_deref()
                .is_some_and(|subject| identity["subject"].as_str() != Some(subject))
        {
            return Self::Unavailable;
        }
        Self::Ready {
            login: identity["login"].as_str().unwrap().to_owned(),
            username: identity["username"].as_str().map(str::to_owned),
            traffic: Traffic::from_reply(reply),
        }
    }

    pub(in crate::client) fn rows(&self) -> [String; 2] {
        let (account, remaining) = match self {
            Self::Loading => ("加载中…".into(), "加载中…".into()),
            Self::SignedOut => ("未登录".into(), "未知".into()),
            Self::Unavailable => ("暂不可用".into(), "未知".into()),
            Self::Ready {
                login,
                username,
                traffic,
            } => {
                let account = match username
                    .as_deref()
                    .filter(|username| *username != login.as_str())
                {
                    Some(username) => format!("{login} ({username})"),
                    None => login.clone(),
                };
                (account, traffic.label())
            }
        };
        [
            format!(" 账号  {account}"),
            format!(" 本月剩余  {remaining}"),
        ]
    }
}

#[cfg(test)]
mod tests;

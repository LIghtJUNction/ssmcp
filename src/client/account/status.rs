//! Read-only account and monthly traffic status for the connected server.
use super::{Api, Credential, Method, StatusCode, Url, Value, public_identity};
use crate::client::language::{Language, localized};

mod traffic;
pub(in crate::client) use traffic::Traffic;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) enum Status {
    Local,
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

    fn account_label(&self, language: Language) -> String {
        match self {
            Self::Local => language.text("current OS user", "当前系统用户").into(),
            Self::Loading => language.text("loading…", "加载中…").into(),
            Self::SignedOut => language.text("signed out", "未登录").into(),
            Self::Unavailable => language.text("unavailable", "不可用").into(),
            Self::Ready {
                login, username, ..
            } => username
                .as_deref()
                .filter(|username| !username.is_empty())
                .unwrap_or(login)
                .to_owned(),
        }
    }

    fn traffic(&self) -> &Traffic {
        match self {
            Self::Ready { traffic, .. } => traffic,
            _ => &Traffic::Unknown,
        }
    }

    /// This quota belongs to the saved login for the live origin, not its remote caller.
    #[cfg(test)]
    pub(in crate::client) fn rows(&self, width: u16) -> [String; 3] {
        self.rows_in(width, Language::English)
    }

    pub(in crate::client) fn rows_in(&self, width: u16, language: Language) -> [String; 3] {
        if *self == Self::Local {
            return [
                language
                    .text(" Local session  current OS user", " 本地会话  当前系统用户")
                    .into(),
                language
                    .text(
                        " Access  /logs for MCP address and one-time login code",
                        " 授权  /logs 查看 MCP 地址和一次性登录码",
                    )
                    .into(),
                String::new(),
            ];
        }
        let width = width.saturating_sub(1);
        let progress = if *self == Self::Loading {
            language.text("Quota loading…", "额度加载中…").into()
        } else {
            self.traffic().progress_in(width, language)
        };
        [
            localized!(
                language,
                " Local {}",
                " 本机账号 {}",
                self.account_label(language)
            ),
            format!(" {progress}"),
            format!(" {}", self.traffic().reset_label_in(width, language)),
        ]
    }

    pub(in crate::client) fn compact_rows(&self, language: Language) -> [String; 2] {
        if *self == Self::Local {
            return [
                language
                    .text("Local session  current OS user", "本地会话 当前系统用户")
                    .into(),
                language
                    .text("/logs for MCP access", "/logs 查看 MCP 授权")
                    .into(),
            ];
        }
        [
            localized!(
                language,
                "Local {}",
                "本机账号 {}",
                self.account_label(language)
            ),
            self.traffic().compact(language),
        ]
    }
}

#[cfg(test)]
mod tests;

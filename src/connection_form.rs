//! Shared connection-target validation and local frontend setup.
pub const DEFAULT_RELAY: &str = "ssmcp.lmm.best";

pub fn relay_origin(server: &str) -> Result<String, String> {
    crate::server_endpoint::parse_target(&format!("account@{}", server.trim()))
        .map(|(_, origin)| origin)
        .map_err(|_| "请填写有效的中继服务器地址。".into())
}

pub fn official_relay(destination: &str) -> bool {
    destination
        .split_once('@')
        .and_then(|(_, server)| relay_origin(server).ok())
        .and_then(|origin| reqwest::Url::parse(&origin).ok())
        .is_some_and(|url| {
            url.host_str()
                .is_some_and(|host| host.trim_end_matches('.') == DEFAULT_RELAY)
        })
}

pub fn desktop_profile() -> crate::Result<Option<String>> {
    crate::client::account::desktop_profile(None)
}

pub fn save_desktop_profile(destination: &str) -> crate::Result<()> {
    crate::client::account::desktop_profile(Some(destination)).map(|_| ())
}

/// Begin registration on the selected relay. Only return the browser link;
/// the device credential and relay response stay outside the frontend.
pub async fn registration_url(destination: &str) -> crate::Result<String> {
    crate::client::account::registration_url(destination).await
}

pub fn open_link(url: &str) -> crate::Result<()> {
    let url = reqwest::Url::parse(url)?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err("unsupported link".into());
    }
    crate::client::account::open_desktop_link(&url)
}

pub fn validate_destination(destination: &str) -> Result<(), String> {
    let (user, server) = destination.split_once('@').ok_or_else(|| {
        if destination.is_empty() {
            "请输入用户名和服务器域名。"
        } else {
            "用户名后需要 @ 和服务器域名。"
        }
        .to_owned()
    })?;
    crate::username::normalize(user).map_err(|error| {
        if error.contains("reserved") {
            "这个用户名由服务器保留，请使用你的账户名。"
        } else {
            "用户名需为 3–32 个字符，以字母开头，以字母或数字结尾，可含 _ 和 -。"
        }
        .to_owned()
    })?;
    if server.is_empty() {
        return Err("请填写服务器域名，或选择默认中继。".into());
    }
    crate::server_endpoint::parse_target(destination)
        .map(|_| ())
        .map_err(|_| "服务器地址无效，请检查域名和端口，例如 ssmcp.lmm.best。".into())
}

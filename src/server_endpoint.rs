//! Shared relay origins and qualified account names.
use crate::Result;
use reqwest::Url;
use std::net::IpAddr;

pub(crate) fn normalize_server(raw: &str) -> Result<String> {
    if raw.bytes().any(|b| b.is_ascii_whitespace() || b == b'\\') {
        return Err("server must not contain whitespace or backslashes".into());
    }
    let url = Url::parse(raw).map_err(|_| "server must be an absolute HTTPS origin")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || url.host_str().is_none()
        || url.port() == Some(0)
    {
        return Err("server must be an HTTPS origin without credentials, path, query or fragment; HTTP is allowed only for literal loopback addresses".into());
    }
    Ok(url.origin().ascii_serialization())
}

/// A named account destination always selects HTTPS; it never becomes an SSH alias.
pub(crate) fn parse_target(raw: &str) -> Result<(String, String)> {
    let (username, host) = raw
        .split_once('@')
        .ok_or("use username@server, for example alice@ssmcp.lmm.best")?;
    let username = crate::username::normalize(username)?;
    if host.is_empty()
        || host.contains('@')
        || host.bytes().any(|b| b.is_ascii_whitespace() || b == b'\\')
    {
        return Err("invalid server in username@server".into());
    }
    let server = if host.contains("://") {
        normalize_server(host)?
    } else {
        if host.contains(['/', '?', '#', '%']) {
            return Err("server must be a hostname, IPv4 address or bracketed IPv6 address with an optional port".into());
        }
        normalize_server(&format!("https://{host}"))?
    };
    Ok((username, server))
}

pub(crate) fn allow_target(raw: &str) -> Result<(String, String)> {
    if raw.len() > 512 {
        return Err("allow target is too long".into());
    }
    if raw
        .rsplit_once('@')
        .is_some_and(|(_, server)| server.eq_ignore_ascii_case("github"))
    {
        return Err("@github is no longer an allow target; use username@relay-server".into());
    }
    parse_target(raw)
}

pub(crate) fn allow_principal(username: &str, server: &str) -> Result<String> {
    let username = crate::username::normalize(username)?;
    let server = normalize_server(server)?;
    let authority = server.strip_prefix("https://").unwrap_or(&server);
    Ok(format!("{username}@{authority}"))
}

pub(crate) fn allow_handle_for_server(principal: &str, server: &str) -> Result<String> {
    let (handle, target_server) = allow_target(principal)?;
    let selected = normalize_server(server)?;
    if target_server != selected {
        return Err(format!("allow target belongs to {target_server}, but this connection uses {selected}; select the matching --server").into());
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_namespace_canonicalizes_handle_origin_and_ports() {
        for (raw, canonical) in [
            ("Alice@SSMCP.Example:443", "alice@ssmcp.example"),
            ("Alice@https://SSMCP.Example/", "alice@ssmcp.example"),
            ("Alice@ssmcp.example:8443", "alice@ssmcp.example:8443"),
            ("Alice@[::1]:8443", "alice@[::1]:8443"),
            ("Alice@http://127.0.0.1:9123", "alice@http://127.0.0.1:9123"),
        ] {
            let (name, server) = allow_target(raw).unwrap();
            assert_eq!(allow_principal(&name, &server).unwrap(), canonical);
            assert_eq!(
                allow_handle_for_server(canonical, &server).unwrap(),
                "alice"
            );
        }
        assert_eq!(
            allow_handle_for_server("alice@SSMCP.Example:443", "https://ssmcp.example/").unwrap(),
            "alice"
        );
    }

    #[test]
    fn allow_namespace_never_crosses_server_scheme_or_port() {
        for target in [
            "alice@other.example",
            "alice@ssmcp.example:8443",
            "alice@http://127.0.0.1:9123",
        ] {
            let error = allow_handle_for_server(target, "https://ssmcp.example").unwrap_err();
            assert!(error.to_string().contains("this connection uses"));
        }
        assert!(allow_handle_for_server("alice@127.0.0.1:9123", "http://127.0.0.1:9123").is_err());
    }

    #[test]
    fn allow_namespace_rejects_unqualified_provider_and_ambiguous_targets() {
        for target in [
            "alice",
            "alice@github",
            "alice@GitHub",
            "@ssmcp.example",
            "alice@",
            "alice@@ssmcp.example",
            "alice@user@ssmcp.example",
            "ali ce@ssmcp.example",
            "alice@ssmcp.example/path",
            "alice@ssmcp.example?query",
            "alice@ssmcp.example#fragment",
            "alice@ssmcp.example\\path",
            "alice@ssmcp.example:0",
            "alice@http://ssmcp.example",
            "alice@http://localhost:9123",
            "alice@https://name:password@ssmcp.example",
        ] {
            assert!(allow_target(target).is_err(), "{target}");
        }
        assert!(allow_target(&format!("alice@{}.example", "x".repeat(512))).is_err());
    }
}

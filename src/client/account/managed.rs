//! Device OAuth credentials are private to one relay and one requested account.
use super::{Api, create_tree, now, read_private, state_root, verify_directory, write_private};
use crate::Result;
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::File,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{Instant, sleep};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    token: String,
    subject: String,
    expires_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    direct: Option<Proof>,
    invited: Option<Proof>,
}

impl Credentials {
    fn selected(&self) -> Option<&Proof> {
        self.invited
            .as_ref()
            .or(self.direct.as_ref())
            .filter(|p| p.expires_at > now())
    }

    fn read(path: &Path) -> Result<Self> {
        let Some(bytes) = read_private(path)? else {
            return Ok(Self::default());
        };
        let data: Self =
            serde_json::from_slice(&bytes).map_err(|_| "stored device authorization is invalid")?;
        if data.direct.iter().chain(&data.invited).any(|p| {
            !secret(&p.token, "cd_")
                || p.subject.is_empty()
                || p.subject.len() > 64
                || !p.subject.bytes().all(|b| b.is_ascii_digit())
        }) {
            return Err("invalid stored device credential".into());
        }
        Ok(data)
    }

    fn write(&self, path: &Path) -> Result<()> {
        write_private(path, &serde_json::to_vec(self)?)
    }
}

fn secret(token: &str, prefix: &str) -> bool {
    token.strip_prefix(prefix).is_some_and(|body| {
        (32..=128).contains(&body.len())
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    })
}

fn path_in(root: &Path, name: &str, origin: &str) -> Result<PathBuf> {
    if crate::username::normalize(name)?.as_str() != name
        || !origin.starts_with("https://")
        || super::normalize_server(origin)? != origin
    {
        return Err("device authorization requires a named account on an HTTPS origin".into());
    }
    create_tree(root)?;
    verify_directory(root)?;
    let key = crate::hex::encode(Sha256::digest(format!("{origin}\n{name}").as_bytes()));
    Ok(root.join(format!("managed-{key}.json")))
}

async fn lock(path: &Path) -> Result<File> {
    let path = path.with_extension("lock");
    tokio::task::spawn_blocking(move || crate::session::lock(&path, false)).await?
}

pub(crate) fn authorized(name: &str, origin: &str) -> Result<bool> {
    Ok(Credentials::read(&path_in(&state_root()?, name, origin)?)?
        .selected()
        .is_some())
}

pub(crate) fn token(name: &str, origin: &str) -> Result<String> {
    let cred = Credentials::read(&path_in(&state_root()?, name, origin)?)?;
    cred.selected().map(|p| p.token.clone()).ok_or_else(|| {
        format!(
            "sign in on this device with ssmcp login {name}@{} --no-browser",
            &origin[8..]
        )
        .into()
    })
}

pub(crate) async fn expire_rejected(name: &str, origin: &str, token: &str) -> Result<()> {
    let path = path_in(&state_root()?, name, origin)?;
    let _gate = lock(&path).await?;
    let mut cred = Credentials::read(&path)?;
    if let Some(proof) = cred.invited.as_mut().or(cred.direct.as_mut())
        && proof.token == token
    {
        proof.expires_at = 0;
        cred.write(&path)?;
    }
    Ok(())
}

pub(crate) fn copy_for_session(name: &str, origin: &str, private_root: &Path) -> Result<()> {
    let cred = Credentials::read(&path_in(&state_root()?, name, origin)?)?;
    let proof = cred
        .selected()
        .ok_or("sign in on this device first")?
        .clone();
    let mut isolated = Credentials::default();
    if cred.invited.is_some() {
        isolated.invited = Some(proof);
    } else {
        isolated.direct = Some(proof);
    }
    isolated.write(&path_in(private_root, name, origin)?)
}

fn proof(value: &Value, name: &str) -> Result<Proof> {
    if value.get("access_token").is_some() {
        return Err(
            "relay returned a controller credential to a device authorization request".into(),
        );
    }
    let identity = super::public_identity(value)?;
    if !super::identity_matches_username(&identity, name) {
        return Err("the approved account does not match the chosen device account".into());
    }
    let token = value["device_token"]
        .as_str()
        .filter(|s| secret(s, "cd_"))
        .ok_or("relay did not issue a device-only OAuth credential; update ssmcpd")?;
    let seconds = value["expires_in"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= 366 * 86400)
        .ok_or("relay returned an invalid device authorization lifetime")?;
    Ok(Proof {
        token: token.into(),
        subject: identity["subject"]
            .as_str()
            .ok_or("relay omitted the stable account ID")?
            .into(),
        expires_at: now().saturating_add(seconds),
    })
}

async fn authorize<F>(name: &str, origin: &str, present: F) -> Result<Proof>
where
    F: FnOnce(&Url, &str) -> Result<()>,
{
    let api = Api::new(origin)?;
    let start = api
        .request(
            Method::POST,
            "/api/client/login/start",
            None,
            Some(json!({"name":"SSMCP controlled device","username":name,"device_only":true})),
        )
        .await?;
    let grant = Api::checked(start)?;
    let code = grant["user_code"]
        .as_str()
        .filter(|s| {
            (4..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .ok_or("invalid device verification code")?;
    let private = grant["device_code"]
        .as_str()
        .filter(|s| secret(s, ""))
        .ok_or("invalid device polling code")?;
    let lifetime = grant["expires_in"]
        .as_u64()
        .filter(|v| (1..=1800).contains(v))
        .ok_or("invalid device verification lifetime")?;
    let interval = Duration::from_secs(grant["interval"].as_u64().unwrap_or(5).clamp(1, 30));
    let deadline = Instant::now() + Duration::from_secs(lifetime);
    let mut link = Url::parse(origin)?.join("/setup")?;
    link.query_pairs_mut().append_pair("code", code);
    present(&link, code)?;
    loop {
        sleep(interval).await;
        if Instant::now() >= deadline {
            return Err("device login expired; start login again".into());
        }
        let reply = api
            .request(
                Method::POST,
                "/api/client/login/poll",
                None,
                Some(json!({"device_code":private})),
            )
            .await?;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            continue;
        }
        let result = Api::checked(reply)?;
        match result["status"].as_str() {
            Some("pending") => continue,
            Some("authorized") => return proof(&result, name),
            Some("denied") => return Err("device login was declined".into()),
            Some("expired") => return Err("device login expired; start login again".into()),
            _ => return Err("relay returned an unknown device login status".into()),
        }
    }
}

pub(crate) async fn ensure_with<F>(name: &str, origin: &str, present: F) -> Result<()>
where
    F: FnOnce(&Url, &str) -> Result<()>,
{
    let path = path_in(&state_root()?, name, origin)?;
    let _gate = lock(&path).await?;
    let mut cred = Credentials::read(&path)?;
    if cred.selected().is_some() {
        return Ok(());
    }
    // A temporary invitation is never silently replaced by wider authority.
    if cred.invited.is_some() {
        return Err(
            "this device invitation expired; use ssmcp login or join with a new invitation".into(),
        );
    }
    cred.direct = Some(authorize(name, origin, present).await?);
    cred.write(&path)
}

fn display(url: &Url, code: &str, browser: bool) -> Result<()> {
    eprintln!(
        "{}",
        super::super::terminal::locale::text("controllers.first_connection")
    );
    eprintln!("Verify {code} and sign in on this device: {url}");
    if browser {
        let _ = super::open_desktop_link(url);
    }
    Ok(())
}

pub(crate) async fn ensure(name: &str, origin: &str, interactive: bool) -> Result<()> {
    if authorized(name, origin)? {
        return Ok(());
    }
    if !interactive {
        return Err(format!(
            "sign in on this device with ssmcp login {name}@{} --no-browser",
            &origin[8..]
        )
        .into());
    }
    ensure_with(name, origin, |url, code| display(url, code, true)).await
}

pub(super) async fn command(command: &str, args: &[OsString]) -> Result<()> {
    let usage = if command == "join" {
        "Usage: ssmcp join USERNAME@SERVER --token-stdin [--no-browser]"
    } else {
        "Usage: ssmcp login USERNAME@SERVER [--no-browser]"
    };
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        println!("{usage}");
        return Ok(());
    }
    let target = args.first().and_then(|v| v.to_str()).ok_or(usage)?;
    let (name, origin) = crate::server_endpoint::parse_target(target)?;
    let mut browser = true;
    let mut token_stdin = false;
    for value in &args[1..] {
        match value.to_str() {
            Some("--no-browser") if browser => browser = false,
            Some("--token-stdin") if command == "join" && !token_stdin => token_stdin = true,
            _ => return Err(usage.into()),
        }
    }
    if command == "join" && !token_stdin {
        return Err(usage.into());
    }
    let path = path_in(&state_root()?, &name, &origin)?;
    let invitation = if token_stdin {
        if io::stdin().is_terminal() {
            return Err("pipe the device invitation to --token-stdin".into());
        }
        let mut value = String::new();
        io::stdin().take(160).read_to_string(&mut value)?;
        let value = value.trim_end_matches(['\r', '\n']);
        if !secret(value, "ci_") {
            return Err("invalid device invitation".into());
        }
        Some(value.to_owned())
    } else {
        None
    };
    let _gate = lock(&path).await?;
    let mut cred = Credentials::read(&path)?;
    if invitation.is_none() || cred.direct.as_ref().is_none_or(|p| p.expires_at <= now()) {
        cred.direct =
            Some(authorize(&name, &origin, |url, code| display(url, code, browser)).await?);
        if invitation.is_none() {
            cred.invited = None;
        }
        cred.write(&path)?;
    }
    if let Some(invitation) = invitation {
        let direct = cred
            .direct
            .as_ref()
            .ok_or("missing device OAuth authorization")?;
        let api = Api::new(&origin)?;
        let reply = api
            .request(
                Method::POST,
                "/api/client/enroll",
                Some(&direct.token),
                Some(json!({"token":invitation})),
            )
            .await?;
        let enrolled = proof(&Api::checked(reply)?, &name)?;
        if direct.subject != enrolled.subject {
            return Err("invitation belongs to a different account".into());
        }
        cred.invited = Some(enrolled);
        cred.write(&path)?;
    }
    println!("Device authorized for {name} on {origin}.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_proof_binds_account_and_credential_kind() {
        let device = format!("cd_{}", "e".repeat(64));
        let mut reply = json!({"device_token":device,"expires_in":3600,
            "identity":{"login":"login","provider":"github","subject":"123","username":"alice","username_aliases":["alice-old"]}});
        assert!(proof(&reply, "alice").is_ok());
        assert!(proof(&reply, "alice-old").is_ok());
        assert!(proof(&reply, "different-user").is_err());
        reply["access_token"] = json!(format!("cl_{}", "a".repeat(64)));
        assert!(proof(&reply, "alice").is_err());
        reply.as_object_mut().unwrap().remove("access_token");
        reply["device_token"] = json!(format!("cl_{}", "e".repeat(64)));
        assert!(proof(&reply, "alice").is_err());
    }

    #[test]
    fn an_expired_invitation_never_selects_a_wider_direct_credential() {
        let p = Proof {
            token: format!("cd_{}", "e".repeat(64)),
            subject: "42".into(),
            expires_at: now() + 100,
        };
        let restricted = Proof {
            expires_at: 1,
            ..p.clone()
        };
        let cache = Credentials {
            direct: Some(p),
            invited: Some(restricted),
        };
        assert!(cache.selected().is_none());
    }
}

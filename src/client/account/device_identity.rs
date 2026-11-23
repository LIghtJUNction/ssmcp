//! Per-origin installation signing key. Private material stays in the account
//! store; only a fresh challenge proof is sent over the existing trusted TLS path.
use super::*;
use crate::wire::device_address::{self, Proof};
use ring::signature::Ed25519KeyPair;
use std::sync::{Mutex, OnceLock};

pub(crate) const SESSION_ENV: &str = "SSMCP_RELAY_SESSION_KEY";

fn key_at(root: &Path, server: &str) -> Result<Ed25519KeyPair> {
    create_tree(root)?;
    verify_directory(root)?;
    let suffix = device_address::credential_key(server);
    let _lock = crate::session::lock(&root.join(format!("device-{suffix}.lock")), false)?;
    let path = root.join(format!("device-{suffix}.pk8"));
    let bytes = match read_private_limit(&path, 1024)? {
        Some(bytes) => bytes,
        None => {
            let document = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|_| "could not generate device identity")?;
            write_private_limit(&path, document.as_ref(), 1024)?;
            document.as_ref().to_vec()
        }
    };
    Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| {
        "invalid saved device identity; do not overwrite it while reconciling existing sessions"
            .into()
    })
}

pub(crate) fn process_session_key() -> Result<String> {
    static KEY: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let mut value = KEY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if value.is_none() {
        let mut bytes = [0; 32];
        platform::random_bytes(&mut bytes)?;
        *value = Some(device_address::encode(&bytes));
    }
    Ok(value.as_ref().unwrap().clone())
}

pub(crate) async fn proof_header(
    server: &str,
    token: &str,
    session_key: &str,
) -> Result<Option<String>> {
    device_address::decode::<32>(session_key)?;
    let api = Api::new(server)?;
    let mut response = api.client.post(format!("{}{}",api.server,device_address::CHALLENGE_PATH))
        .bearer_auth(token).send().await
        .map_err(|_| "device challenge failed or timed out; retained tasks must be reconciled before any replay")?;
    match response.status() {
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => return Ok(None),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(super::super::relay::failure(
                super::super::relay::AUTH_EXPIRED.into(),
            ));
        }
        status if !status.is_success() => {
            return Err(format!(
                "device challenge unavailable ({status}); no tool operation was dispatched"
            )
            .into());
        }
        _ => {}
    }
    let mut bytes = Vec::new();
    while let Some(part) = response
        .chunk()
        .await
        .map_err(|_| "device challenge response was interrupted")?
    {
        if bytes.len().saturating_add(part.len()) > 4096 {
            return Err("device challenge response exceeds its limit".into());
        }
        bytes.extend_from_slice(&part);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "invalid device challenge response")?;
    if value["format"] != "ssmcp-device-v1"
        || value["expires_in"]
            .as_u64()
            .is_none_or(|n| n == 0 || n > 30)
    {
        return Err("unsupported device challenge response".into());
    }
    let nonce = value["nonce"]
        .as_str()
        .ok_or("device challenge has no nonce")?;
    device_address::decode::<32>(nonce)?;
    let key = key_at(&state_root()?, &api.server)?;
    Ok(Some(
        Proof::sign(
            &key,
            &api.server,
            &device_address::credential_key(token),
            session_key,
            nonce,
        )?
        .header()?,
    ))
}

/// Recheck the saved grant without OAuth, credential replacement or key creation.
/// Recovery must not use a currently signed-in different account as a substitute.
#[cfg(target_os = "linux")]
pub(crate) async fn verified_device_id(server: &str) -> Result<String> {
    let credential = credentials(Some(server)).await?.ok_or(
        "recovery authorization is unavailable; explicitly reauthorize the original account",
    )?;
    let api = Api::new(&credential.server)?;
    let reply = api.get("/api/client/whoami", &credential.token).await?;
    let identity = public_identity(&reply)?;
    same_subject(&credential, &identity)?;
    let subject = identity["subject"]
        .as_str()
        .ok_or("recovery account has no verified subject")?;
    let root = state_root()?;
    verify_directory(&root)?;
    let path = root.join(format!(
        "device-{}.pk8",
        device_address::credential_key(&api.server)
    ));
    let bytes = read_private_limit(&path, 1024)?
        .ok_or("recovery device identity is unavailable; do not recreate it automatically")?;
    let key =
        Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| "recovery device identity is invalid")?;
    let proof = Proof::sign(
        &key,
        &api.server,
        &device_address::credential_key(&credential.token),
        &"0".repeat(64),
        &"0".repeat(64),
    )?;
    Ok(proof.identity(&api.server, subject).device_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;
    #[test]
    fn installation_key_is_persistent_origin_scoped_and_not_silently_replaced() {
        let mut random = [0; 8];
        platform::random_bytes(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-device-key-{}-{}",
            std::process::id(),
            device_address::encode(&random)
        ));
        let first = key_at(&root, "https://one.example").unwrap();
        let same = key_at(&root, "https://one.example").unwrap();
        let other = key_at(&root, "https://other.example").unwrap();
        assert_eq!(first.public_key().as_ref(), same.public_key().as_ref());
        assert_ne!(first.public_key().as_ref(), other.public_key().as_ref());
        let path = root.join(format!(
            "device-{}.pk8",
            device_address::credential_key("https://one.example")
        ));
        write_private_limit(&path, b"corrupt", 1024).unwrap();
        assert!(key_at(&root, "https://one.example").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn foreground_session_key_is_reused_only_by_the_current_process() {
        let first = process_session_key().unwrap();
        device_address::decode::<32>(&first).unwrap();
        assert_eq!(first, process_session_key().unwrap());
    }
}

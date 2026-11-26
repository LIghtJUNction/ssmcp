//! Account-scoped device and session addresses, proven with an existing TLS
//! account credential plus possession of an installation's Ed25519 key.
//! A server-issued, single-use challenge is consumed outside this wire module.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{self, Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CAPABILITY: &str = "device_address_v1";
pub const PROOF_HEADER: &str = "x-ssmcp-device-proof";
pub const CHALLENGE_PATH: &str = "/api/client/device/challenge";
pub const MAX_HEADER: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub version: u8,
    pub public_key: String,
    pub session_key: String,
    pub nonce: String,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub device_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub device_id: String,
    pub session_id: String,
    pub connection_id: String,
    pub alias: String,
    pub alias_kind: String,
    pub transport: String,
}

pub fn encode(bytes: &[u8]) -> String {
    crate::hex::encode(bytes)
}

pub fn decode<const N: usize>(value: &str) -> Result<[u8; N], &'static str> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("invalid device proof encoding");
    }
    let mut out = [0; N];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "invalid device proof encoding")?;
    }
    Ok(out)
}

fn fields(domain: &[u8], values: &[&[u8]]) -> Vec<u8> {
    let mut bytes = domain.to_vec();
    for value in values {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value);
    }
    bytes
}

pub fn credential_key(token: &str) -> String {
    encode(&Sha256::digest(token.as_bytes()))
}

impl Proof {
    fn message(&self, origin: &str, credential: &str) -> Result<Vec<u8>, &'static str> {
        if self.version != 1 || origin.len() > 2048 || decode::<32>(credential).is_err() {
            return Err("invalid device proof context");
        }
        let public = decode::<32>(&self.public_key)?;
        let session = decode::<32>(&self.session_key)?;
        let nonce = decode::<32>(&self.nonce)?;
        Ok(fields(
            b"ssmcp-device-proof-v1\0",
            &[
                origin.as_bytes(),
                credential.as_bytes(),
                &public,
                &session,
                &nonce,
            ],
        ))
    }

    pub fn sign(
        key: &Ed25519KeyPair,
        origin: &str,
        credential: &str,
        session_key: &str,
        nonce: &str,
    ) -> Result<Self, &'static str> {
        let mut proof = Self {
            version: 1,
            public_key: encode(key.public_key().as_ref()),
            session_key: session_key.into(),
            nonce: nonce.into(),
            signature: String::new(),
        };
        proof.signature = encode(key.sign(&proof.message(origin, credential)?).as_ref());
        Ok(proof)
    }

    pub fn verify(&self, origin: &str, credential: &str) -> Result<(), &'static str> {
        signature::UnparsedPublicKey::new(&signature::ED25519, decode::<32>(&self.public_key)?)
            .verify(
                &self.message(origin, credential)?,
                &decode::<64>(&self.signature)?,
            )
            .map_err(|_| "device proof signature is invalid")
    }

    pub fn header(&self) -> Result<String, &'static str> {
        let raw = serde_json::to_vec(self).map_err(|_| "invalid device proof")?;
        let value = URL_SAFE_NO_PAD.encode(raw);
        if value.len() > MAX_HEADER {
            return Err("device proof is too large");
        }
        Ok(value)
    }

    pub fn from_header(value: &str) -> Result<Self, &'static str> {
        if value.len() > MAX_HEADER {
            return Err("device proof is too large");
        }
        let raw = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| "invalid device proof")?;
        serde_json::from_slice(&raw).map_err(|_| "invalid device proof")
    }

    pub fn identity(&self, origin: &str, verified_subject: &str) -> Identity {
        let device_id = format!(
            "dev_{}",
            encode(&Sha256::digest(fields(
                b"ssmcp-device-id-v1\0",
                &[
                    origin.as_bytes(),
                    verified_subject.as_bytes(),
                    self.public_key.as_bytes()
                ],
            )))
        );
        let session_id = format!(
            "ses_{}",
            encode(&Sha256::digest(fields(
                b"ssmcp-session-id-v1\0",
                &[device_id.as_bytes(), self.session_key.as_bytes()],
            )))
        );
        Identity {
            device_id,
            session_id,
        }
    }
}

impl Identity {
    pub fn address(&self, connection_id: &str, name: &str, hostname: &str) -> Address {
        fn label(value: &str, fallback: &str) -> String {
            let value = value
                .chars()
                .take(63)
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let value = value.trim_matches('-');
            if value.is_empty() {
                fallback.into()
            } else {
                value.into()
            }
        }
        let host = hostname.split('.').next().unwrap_or(hostname);
        Address {
            device_id: self.device_id.clone(),
            session_id: self.session_id.clone(),
            connection_id: connection_id.into(),
            alias: format!("{}@{}.local", label(name, "session"), label(host, "device")),
            alias_kind: "display_only".into(),
            transport: "relay".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }
    #[test]
    fn proof_binds_origin_credential_session_and_challenge() {
        let credential = credential_key("synthetic-account-token");
        let proof = Proof::sign(
            &key(),
            "https://relay.example",
            &credential,
            &"a".repeat(64),
            &"b".repeat(64),
        )
        .unwrap();
        proof.verify("https://relay.example", &credential).unwrap();
        assert!(proof.verify("https://other.example", &credential).is_err());
        assert!(
            proof
                .verify("https://relay.example", &credential_key("other-token"))
                .is_err()
        );
        for field in ["session", "nonce", "public", "signature"] {
            let mut changed = proof.clone();
            match field {
                "session" => changed.session_key = "c".repeat(64),
                "nonce" => changed.nonce = "c".repeat(64),
                "public" => changed.public_key = encode(key().public_key().as_ref()),
                _ => changed.signature = "0".repeat(128),
            }
            assert!(
                changed
                    .verify("https://relay.example", &credential)
                    .is_err()
            );
        }
        Proof::from_header(&proof.header().unwrap())
            .unwrap()
            .verify("https://relay.example", &credential)
            .unwrap();
    }
    #[test]
    fn address_survives_credentials_and_hostname_changes_but_not_cross_account_claims() {
        let key = key();
        let sign = |token| {
            Proof::sign(
                &key,
                "https://relay.example",
                &credential_key(token),
                &"a".repeat(64),
                &"b".repeat(64),
            )
            .unwrap()
        };
        let first = sign("old").identity("https://relay.example", "42");
        let renewed = sign("renewed").identity("https://relay.example", "42");
        assert_eq!(first, renewed);
        assert_ne!(first, sign("old").identity("https://relay.example", "43"));
        assert_ne!(first, sign("old").identity("https://other.example", "42"));
        let old = first.address("old-connection", "work", "laptop");
        let new = first.address("new-connection", "work", "renamed.example.org");
        assert_eq!(old.session_id, new.session_id);
        assert_ne!(old.connection_id, new.connection_id);
        assert_eq!(new.alias, "work@renamed.local");
        assert_eq!(new.alias_kind, "display_only");
    }
    #[test]
    fn invalid_and_oversized_proof_headers_fail_closed() {
        assert!(Proof::from_header(&"x".repeat(MAX_HEADER + 1)).is_err());
        assert!(decode::<32>(&"A".repeat(64)).is_err());
        assert!(decode::<32>(&"界".repeat(64)).is_err());
        assert!(Proof::from_header("not-json").is_err());
    }
}

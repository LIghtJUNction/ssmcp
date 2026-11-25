//! A project key signs one fresh Ready connection ID, independently of account credentials.
use super::device_address::{decode, encode};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "project_continuity_v1";
pub const OFFER_HEADER: &str = "x-ssmcp-project-continuity";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub public_key: String,
    pub previous: Option<String>,
    pub signature: String,
}

impl Proof {
    fn message(&self, origin: &str, client_id: &str) -> Result<Vec<u8>, &'static str> {
        if origin.is_empty() || origin.len() > 2048 {
            return Err("invalid project proof origin");
        }
        let mut bytes = b"ssmcp-project-continuity-v1\0".to_vec();
        bytes.extend_from_slice(&(origin.len() as u64).to_be_bytes());
        bytes.extend_from_slice(origin.as_bytes());
        bytes.extend_from_slice(&decode::<32>(&self.public_key)?);
        bytes.extend_from_slice(&decode::<32>(client_id)?);
        if let Some(previous) = &self.previous {
            bytes.extend_from_slice(&decode::<32>(previous)?);
        }
        Ok(bytes)
    }

    pub fn sign(
        key: &Ed25519KeyPair,
        origin: &str,
        client_id: &str,
        previous: Option<&str>,
    ) -> Result<Self, &'static str> {
        let mut proof = Self {
            public_key: encode(key.public_key().as_ref()),
            previous: previous.map(str::to_owned),
            signature: String::new(),
        };
        proof.signature = encode(key.sign(&proof.message(origin, client_id)?).as_ref());
        Ok(proof)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn verifies(&self, origin: &str, client_id: &str) -> bool {
        use ring::signature;
        let (Ok(public), Ok(signature), Ok(message)) = (
            decode::<32>(&self.public_key),
            decode::<64>(&self.signature),
            self.message(origin, client_id),
        ) else {
            return false;
        };
        signature::UnparsedPublicKey::new(&signature::ED25519, public)
            .verify(&message, &signature)
            .is_ok()
    }
}

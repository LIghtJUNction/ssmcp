//! Provisioned control principals and a separate trusted device ingress.
use super::{Authorization, IDENTITY_RESOURCE};
use crate::identity::Identity;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use subtle::ConstantTimeEq;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAccess {
    device_ingress_sha256: String,
    principals: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) accounting: Option<crate::portal::relay_accounting::Config>,
}

impl RelayAccess {
    pub(crate) fn validate(&self) -> crate::Result<()> {
        let mut hashes = HashSet::new();
        let valid_digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !valid_digest(&self.device_ingress_sha256)
            || !hashes.insert(self.device_ingress_sha256.as_str())
            || !(1..=4096).contains(&self.principals.len())
            || self.principals.iter().any(|(name, value)| {
                crate::username::normalize(name).as_ref() != Ok(name)
                    || !valid_digest(value)
                    || !hashes.insert(value)
            })
        {
            return Err(
                "relay requires canonical principal names and distinct SHA-256 digests".into(),
            );
        }
        if let Some(config) = &self.accounting {
            config.validate(&self.principals, &self.device_ingress_sha256)?;
        }
        Ok(())
    }

    pub(super) fn authenticate(&self, resource: &str, token: &str) -> Option<Authorization> {
        if resource != IDENTITY_RESOURCE {
            return None;
        }
        self.principal_for_token(token).map(Authorization::Identity)
    }

    fn principal_for_token(&self, token: &str) -> Option<Identity> {
        let digest = token_digest(token)?;
        let name = self
            .principals
            .iter()
            .find(|(_, value)| bool::from(value.as_bytes().ct_eq(digest.as_bytes())))?
            .0;
        self.identity(name)
    }

    /// The trusted ingress authenticates the registering account separately
    /// from the device's local choice of permitted controllers.
    pub(crate) fn device_principal(&self, headers: &axum::http::HeaderMap) -> Option<Identity> {
        let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
        let token = values.next()?.to_str().ok()?.strip_prefix("Bearer ")?;
        if values.next().is_some() {
            return None;
        }
        self.principal_for_token(token)
    }

    pub(crate) fn permit_ingress(&self, headers: &axum::http::HeaderMap) -> bool {
        let mut values = headers.get_all("x-ssmcp-device-ingress").iter();
        let Some(token) = values.next().and_then(|value| value.to_str().ok()) else {
            return false;
        };
        values.next().is_none()
            && token_digest(token).is_some_and(|digest| {
                bool::from(
                    self.device_ingress_sha256
                        .as_bytes()
                        .ct_eq(digest.as_bytes()),
                )
            })
    }

    pub(super) fn identity(&self, name: &str) -> Option<Identity> {
        self.principals.contains_key(name).then(|| Identity {
            provider: "external".into(),
            subject: name.into(),
            login: name.into(),
        })
    }
}

fn token_digest(token: &str) -> Option<String> {
    (token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| crate::hex::encode(Sha256::digest(token.as_bytes())))
}

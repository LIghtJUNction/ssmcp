//! Authenticated operation provenance. Display claims live separately in Controller.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlAccount {
    pub provider: String,
    pub subject: String,
    pub login: String,
}

impl ControlAccount {
    pub fn validate(&self) -> Result<(), String> {
        if self.provider != "github"
            || self.subject.len() > 20
            || !self.subject.bytes().all(|byte| byte.is_ascii_digit())
            || !self.subject.parse::<u64>().is_ok_and(|id| id > 0)
            || super::canonical_github_principal(&format!("{}@github", self.login)).is_err()
        {
            return Err("invalid authenticated control account".into());
        }
        Ok(())
    }

    pub fn label(&self) -> String {
        format!("{}@{} #{}", self.login, self.provider, self.subject)
    }
}

/// A registry credential verified for this account and client, never a model attestation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlAgent {
    pub id: String,
    pub name: String,
}

impl ControlAgent {
    pub fn validate(&self) -> Result<(), String> {
        let id = self.id.strip_prefix("agent_").unwrap_or("");
        if id.len() != 32
            || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err("invalid authenticated control agent".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlAttribution {
    /// Account whose current authorization admitted this operation.
    pub account: ControlAccount,
    /// Account that authenticated the device registration, when present.
    /// Billing is attributed to the authenticated controller account above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ControlAccount>,
    /// Absent means no verified agent binding for this operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ControlAgent>,
}

impl ControlAttribution {
    pub fn validate(&self) -> Result<(), String> {
        self.account.validate()?;
        if let Some(owner) = &self.owner {
            owner.validate()?;
        }
        if let Some(agent) = &self.agent {
            agent.validate()?;
        }
        Ok(())
    }

    pub fn bytes(&self) -> usize {
        fn account(value: &ControlAccount) -> usize {
            value.provider.len() + value.subject.len() + value.login.len()
        }
        account(&self.account)
            + self.owner.as_ref().map_or(0, account)
            + self
                .agent
                .as_ref()
                .map_or(0, |agent| agent.id.len() + agent.name.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_attribution_is_bounded_and_has_no_credential_fields() {
        let mut value = ControlAttribution {
            account: ControlAccount {
                provider: "github".into(),
                subject: "123".into(),
                login: "alice".into(),
            },
            owner: None,
            agent: Some(ControlAgent {
                id: format!("agent_{}", "a".repeat(32)),
                name: "worker".into(),
            }),
        };
        assert!(value.validate().is_ok());
        let mut json = serde_json::to_value(&value).unwrap();
        json["agent"]["token"] = "forged".into();
        assert!(serde_json::from_value::<ControlAttribution>(json).is_err());
        value.account.subject = "0".into();
        assert!(value.validate().is_err());
        value.account.subject = "123".into();
        value.agent.as_mut().unwrap().name = "x".repeat(129);
        assert!(value.validate().is_err());
    }
}

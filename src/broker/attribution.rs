//! Account and agent claims are derived from authorization, never call arguments.
use super::{Authorization, Session};
use crate::{
    code_mode::RunBinding,
    identity::Identity,
    wire::{ControlAccount, ControlAgent, ControlAttribution},
};

fn account(identity: &Identity) -> ControlAccount {
    ControlAccount {
        provider: identity.provider.clone(),
        subject: identity.subject.clone(),
        login: identity.login.clone(),
    }
}

impl Session {
    pub(super) fn control_attribution(
        &self,
        authorization: Option<&Authorization>,
        binding: Option<&RunBinding>,
    ) -> Result<Option<ControlAttribution>, String> {
        let Some(Authorization::Identity(identity)) = authorization else {
            // Connection-bound legacy and internal calls have no verified account.
            return Ok(None);
        };
        let agent = if let Some(binding) = binding {
            let suffix = format!("\0{}\0{}", identity.provider, identity.subject);
            if !binding.owner.ends_with(&suffix) {
                return Err("bound agent belongs to another authenticated account".into());
            }
            // Recheck the registry credential at dispatch admission, including expiry
            // and leave/rejoin. No agent token leaves the server in display metadata.
            let agent =
                self.collaboration
                    .bind(&binding.owner, &binding.agent_id, &binding.token)?;
            Some(ControlAgent {
                id: agent.agent_id,
                name: agent.name,
            })
        } else {
            None
        };
        let attribution = ControlAttribution {
            account: account(identity),
            owner: self.owner_identity.as_ref().map(account),
            agent,
        };
        attribution.validate()?;
        Ok(Some(attribution))
    }
}

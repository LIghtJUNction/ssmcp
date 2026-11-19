//! Local access edits are bound to the controlled stream carrying the command.
use super::{Broker, Session};
use crate::{
    identity::Identity,
    wire::{AccessCommand, AccessSnapshot, MAX_ALLOWED_PRINCIPALS, canonical_allow_principal},
};

fn same_identity(left: &Identity, right: &Identity) -> bool {
    left.provider == right.provider && left.subject == right.subject
}

impl Session {
    pub(super) async fn access_command(
        &self,
        broker: &Broker,
        owner: Option<&Identity>,
        command: AccessCommand,
    ) -> Result<AccessSnapshot, String> {
        if !self.is_alive() || !self.identity_required {
            return Err("this connection has no explicit account access policy".into());
        }
        // This entry point is called only by this local controlled stream.
        // Controller HTTP requests cannot edit another connection's allow list.
        let message = match command {
            AccessCommand::List {} => None,
            AccessCommand::Add { principal } => {
                let principal = canonical_allow_principal(&principal)?;
                let identity = broker
                    .resolve_initial_allow(std::slice::from_ref(&principal))
                    .await?
                    .into_iter()
                    .next()
                    .ok_or("unknown server username")?;
                if !self.is_alive() {
                    return Err("connection closed; access was not changed".into());
                }
                let mut allow = self.allowed();
                if allow
                    .iter()
                    .any(|allowed| same_identity(allowed, &identity))
                {
                    Some(format!("{principal} already has access to this connection"))
                } else {
                    if allow.len() >= MAX_ALLOWED_PRINCIPALS {
                        return Err(format!(
                            "allow list accepts at most {MAX_ALLOWED_PRINCIPALS} accounts"
                        ));
                    }
                    allow.push(identity);
                    Some(format!("Allowed {principal} on this connection"))
                }
            }
            AccessCommand::Remove { principal } => {
                let principal = canonical_allow_principal(&principal)?;
                let target = broker.revoke_target(&principal).await?;
                if !self.is_alive() {
                    return Err("connection closed; access was not changed".into());
                }
                let mut allow = self.allowed();
                let index = allow
                    .iter()
                    .position(|identity| same_identity(identity, &target))
                    .ok_or("that account is not allowed on this connection")?;
                if allow.len() == 1 {
                    return Err("cannot remove the last allowed account; close this connection to stop access".into());
                }
                allow.remove(index);
                Some(format!("Revoked {principal} on this connection"))
            }
        };
        let allow = self.allowed().clone();
        broker.access_snapshot(&allow, owner, message).await
    }
}

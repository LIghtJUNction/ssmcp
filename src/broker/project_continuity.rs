//! Query-only links between registrations. Ordinary access checks never consult this table.
use super::{Authorization, Broker, Session};
use crate::wire::project_continuity::Proof;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_RECORDS: usize = 4096;
const RETENTION: Duration = Duration::from_secs(24 * 3600);
pub(super) const PROOF_WINDOW: Duration = Duration::from_secs(60);

pub(super) fn consume_challenge(deadline: &mut Option<Instant>, now: Instant) -> bool {
    deadline.take().is_some_and(|expires| now < expires)
}

#[derive(Default)]
pub(super) struct Registry(Mutex<HashMap<String, Record>>);

struct Record {
    public_key: String,
    scope: [u8; 32],
    previous: Option<String>,
    closed: Option<Instant>,
    ambiguous: bool,
}

impl Registry {
    pub(super) fn forget(&self, id: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    pub(super) fn disconnected(&self, id: &str) {
        if let Some(entry) = self.0.lock().unwrap_or_else(|e| e.into_inner()).get_mut(id) {
            entry.closed = Some(Instant::now());
            entry.previous = None;
        }
    }

    fn register(&self, id: &str, proof: Proof, scope: [u8; 32], now: Instant) {
        let mut records = self.0.lock().unwrap_or_else(|e| e.into_inner());
        records.retain(|_, r| r.closed.is_none_or(|t| now.duration_since(t) < RETENTION));
        let mut ambiguous = false;
        for r in records
            .values_mut()
            .filter(|r| r.closed.is_none() && r.public_key == proof.public_key)
        {
            r.ambiguous = true;
            r.previous = None;
            ambiguous = true;
        }
        let previous = proof.previous.filter(|p| {
            !ambiguous
                && records.get(p).is_some_and(|r| {
                    r.closed.is_some()
                        && !r.ambiguous
                        && r.scope == scope
                        && r.public_key == proof.public_key
                })
        });
        // Even a fresh start or a changed scope supersedes all older observations
        // for this key. Only the immediately preceding session may be linked.
        records.retain(|_, r| r.closed.is_none() || r.public_key != proof.public_key);
        if records.len() < MAX_RECORDS {
            records.insert(
                id.into(),
                Record {
                    public_key: proof.public_key,
                    scope,
                    previous,
                    closed: None,
                    ambiguous,
                },
            );
        }
    }
}

fn scope(session: &Session, allow: &[crate::identity::Identity]) -> [u8; 32] {
    let mut principals: Vec<_> = allow.iter().map(|i| (&i.provider, &i.subject)).collect();
    principals.sort_unstable();
    // Include only stable provider/subject identities. A login rename does not change the ACL.
    let bytes = serde_json::to_vec(&(
        &session.ssh_user,
        &session.info.cwd,
        session.info.mode,
        principals,
    ))
    .expect("project scope consists of serializable strings");
    Sha256::digest(bytes).into()
}

impl Broker {
    pub(super) fn register_project_proof(&self, session: &Session, proof: Proof) {
        if !proof.verifies(self.public_url.trim_end_matches('/'), session.client_id()) {
            return;
        }
        let allow = session.allowed();
        self.project_continuity.register(
            session.client_id(),
            proof,
            scope(session, &allow),
            Instant::now(),
        );
    }

    /// Call only after current endpoint/resource authorization. Never used for tool dispatch.
    pub(crate) fn project_previous(
        &self,
        session: &Session,
        auth: &Authorization,
    ) -> Option<String> {
        let Authorization::Identity(identity) = auth else {
            return None;
        };
        if !session.is_alive() || !self.authorized_account(auth) {
            return None;
        }
        let allow = session.allowed();
        if !allow
            .iter()
            .any(|i| i.provider == identity.provider && i.subject == identity.subject)
        {
            return None;
        }
        let records = self
            .project_continuity
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let record = records.get(session.client_id())?;
        if record.closed.is_some() || record.ambiguous || record.scope != scope(session, &allow) {
            return None;
        }
        record.previous.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_continuity_challenge_expires_at_deadline_and_is_consumed_once() {
        let start = Instant::now();
        let mut fresh = Some(start + PROOF_WINDOW);
        assert!(consume_challenge(&mut fresh, start));
        assert!(!consume_challenge(&mut fresh, start));
        let mut expired = Some(start + PROOF_WINDOW);
        assert!(!consume_challenge(&mut expired, start + PROOF_WINDOW));
        assert!(expired.is_none());
    }

    #[test]
    fn project_continuity_only_retains_the_immediate_unexpired_predecessor() {
        fn claim(previous: Option<&str>) -> Proof {
            Proof {
                public_key: "one fixture key".into(),
                previous: previous.map(str::to_owned),
                signature: String::new(),
            }
        }
        let registry = Registry::default();
        let now = Instant::now();
        registry.register("a", claim(None), [1; 32], now);
        registry.disconnected("a");
        registry.register("b", claim(None), [1; 32], Instant::now());
        registry.disconnected("b");
        registry.register("c", claim(Some("a")), [1; 32], Instant::now());
        assert!(registry.0.lock().unwrap()["c"].previous.is_none());
        registry.disconnected("c");
        registry.register("d", claim(Some("c")), [1; 32], Instant::now() + RETENTION);
        assert!(registry.0.lock().unwrap()["d"].previous.is_none());
    }
}

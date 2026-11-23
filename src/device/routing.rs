//! Bounded device enrollment challenges and last-known owner inventory.
//! Stable IDs never grant access: normal per-call account and session ACL checks
//! still select the exact current connection.
use super::*;
use crate::wire::device_address::{Identity as DeviceIdentity, Proof};
use std::{collections::HashMap, sync::Mutex, time::Instant};

const CHALLENGE_TTL: Duration = Duration::from_secs(30);
const WINDOW: Duration = Duration::from_secs(60);
const INVENTORY_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_CHALLENGES: usize = 4096;
const MAX_SUBJECT_PENDING: usize = 128;
const MAX_SUBJECT_PER_MINUTE: usize = 256;
const MAX_INVENTORY: usize = 8192;

struct Challenge {
    subject: String,
    credential: String,
    expires: Instant,
}
struct Record {
    subject: String,
    user: String,
    info: wire::ClientInfo,
    connected: bool,
    observed_at: u64,
    expires: Instant,
}
#[derive(Default)]
struct HubState {
    challenges: HashMap<String, Challenge>,
    rates: HashMap<String, (Instant, usize)>,
    inventory: BTreeMap<String, Record>,
}
#[derive(Default)]
pub(crate) struct Hub(Mutex<HubState>);

impl HubState {
    fn prune(&mut self, now: Instant) {
        self.challenges.retain(|_, value| value.expires > now);
        self.rates
            .retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
        self.inventory
            .retain(|_, value| value.connected || value.expires > now);
    }
}
impl Hub {
    pub(super) fn issue(&self, registration: &Registration) -> Result<Value, Problem> {
        let now = Instant::now();
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(now);
        if state.challenges.len() >= MAX_CHALLENGES
            || state
                .challenges
                .values()
                .filter(|v| v.subject == registration.identity.subject)
                .count()
                >= MAX_SUBJECT_PENDING
            || state
                .rates
                .get(&registration.identity.subject)
                .is_some_and(|(_, n)| *n >= MAX_SUBJECT_PER_MINUTE)
        {
            return Err(Problem::Busy);
        }
        let nonce = secret("")?;
        if state.challenges.contains_key(&nonce) {
            return Err(Problem::Storage);
        }
        state
            .rates
            .entry(registration.identity.subject.clone())
            .or_insert((now, 0))
            .1 += 1;
        state.challenges.insert(
            nonce.clone(),
            Challenge {
                subject: registration.identity.subject.clone(),
                credential: registration.key.clone(),
                expires: now + CHALLENGE_TTL,
            },
        );
        Ok(json!({"format":"ssmcp-device-v1","nonce":nonce,"expires_in":CHALLENGE_TTL.as_secs()}))
    }

    pub(super) fn verify(
        &self,
        registration: &Registration,
        origin: &str,
        header: &str,
    ) -> Result<DeviceIdentity, Problem> {
        let proof = Proof::from_header(header).map_err(|_| Problem::Invalid)?;
        proof
            .verify(origin, &registration.key)
            .map_err(|_| Problem::Unauthorized)?;
        let now = Instant::now();
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(now);
        let challenge = state
            .challenges
            .get(&proof.nonce)
            .ok_or(Problem::Unauthorized)?;
        if challenge.subject != registration.identity.subject
            || challenge.credential != registration.key
        {
            return Err(Problem::Unauthorized);
        }
        // Verify and consume under one lock; a concurrent replay can never win twice.
        state.challenges.remove(&proof.nonce);
        Ok(proof.identity(origin, &registration.identity.subject))
    }

    pub(crate) fn connected(
        &self,
        subject: &str,
        user: &str,
        info: &wire::ClientInfo,
    ) -> Result<(), &'static str> {
        let Some(address) = &info.routing else {
            return Ok(());
        };
        let now = Instant::now();
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(now);
        if state
            .inventory
            .get(&address.session_id)
            .is_some_and(|v| v.connected && v.info.id != info.id)
        {
            return Err(
                "device session is already connected; inspect its existing connection before reconnecting",
            );
        }
        if !state.inventory.contains_key(&address.session_id)
            && state.inventory.len() >= MAX_INVENTORY
        {
            let oldest = state
                .inventory
                .iter()
                .filter(|(_, v)| !v.connected)
                .min_by_key(|(_, v)| v.expires)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                state.inventory.remove(&oldest);
            } else {
                return Err("device inventory capacity reached; no connection was replaced");
            }
        }
        state.inventory.insert(
            address.session_id.clone(),
            Record {
                subject: subject.into(),
                user: user.into(),
                info: info.clone(),
                connected: true,
                observed_at: super::now(),
                expires: now + INVENTORY_TTL,
            },
        );
        Ok(())
    }

    pub(crate) fn disconnected(&self, info: &wire::ClientInfo) {
        let Some(address) = &info.routing else { return };
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = state.inventory.get_mut(&address.session_id)
            && record.info.id == info.id
        {
            record.connected = false;
            record.observed_at = super::now();
            record.expires = Instant::now() + INVENTORY_TTL;
        }
    }

    /// Offline history is owner-only. Live delegated sessions remain in clients;
    /// a former delegate cannot use retained inventory to observe revoked access.
    pub(crate) fn inventory(&self, user: &str, authorization: &Authorization) -> Vec<Value> {
        let Authorization::Identity(identity) = authorization else {
            return Vec::new();
        };
        if identity.provider != "github" {
            return Vec::new();
        }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.prune(Instant::now());
        state.inventory.values()
            .filter(|v| v.subject == identity.subject && (user == crate::broker::IDENTITY_RESOURCE || v.user == user))
            .map(|v| json!({
                "routing":v.info.routing,"name":v.info.name,"hostname":v.info.hostname,"cwd":v.info.cwd,
                "status":if v.connected {"connected"} else {"offline"},
                "client_id":if v.connected {Some(v.info.id.as_str())} else {None},
                "last_connection_id":v.info.id,"observed_at":v.observed_at,
                "retention":"broker_lifetime_or_24h_after_disconnect","authority":"owner_inventory_only"
            })).collect()
    }
}

pub(super) async fn challenge(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let registration = authenticate(&broker, &headers, true).await?;
    if !registration.is_live(&broker).await {
        return Err(problem(Problem::Unauthorized));
    }
    broker
        .device_routes
        .issue(&registration)
        .map(Json)
        .map_err(problem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::Ed25519KeyPair;
    fn registration(subject: &str, token: &str) -> Registration {
        Registration {
            identity: crate::identity::Identity {
                provider: "github".into(),
                subject: subject.into(),
                login: "same-label".into(),
            },
            username: format!("gh-{subject}"),
            session_access: true,
            control_attribution: true,
            key: hash(token),
            device_identity: None,
        }
    }
    fn proof(hub: &Hub, registration: &Registration) -> Proof {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let nonce = hub.issue(registration).unwrap()["nonce"]
            .as_str()
            .unwrap()
            .to_owned();
        Proof::sign(
            &key,
            "https://relay.example",
            &registration.key,
            &"a".repeat(64),
            &nonce,
        )
        .unwrap()
    }
    #[test]
    fn single_use_challenges_are_credential_bound_and_expire() {
        let hub = Hub::default();
        let owner = registration("42", "one");
        let p = proof(&hub, &owner);
        assert!(
            hub.verify(
                &registration("43", "other"),
                "https://relay.example",
                &p.header().unwrap()
            )
            .is_err()
        );
        hub.verify(&owner, "https://relay.example", &p.header().unwrap())
            .unwrap();
        assert!(
            hub.verify(&owner, "https://relay.example", &p.header().unwrap())
                .is_err()
        );
        let p = proof(&hub, &owner);
        hub.0
            .lock()
            .unwrap()
            .challenges
            .get_mut(&p.nonce)
            .unwrap()
            .expires = Instant::now();
        assert!(
            hub.verify(&owner, "https://relay.example", &p.header().unwrap())
                .is_err()
        );
    }
    #[test]
    fn pending_challenges_are_bounded_per_account() {
        let hub = Hub::default();
        let owner = registration("42", "one");
        for _ in 0..MAX_SUBJECT_PENDING {
            hub.issue(&owner).unwrap();
        }
        assert!(matches!(hub.issue(&owner), Err(Problem::Busy)));
        assert!(hub.issue(&registration("43", "other")).is_ok());
    }
    #[test]
    fn ten_same_name_devices_remain_distinct_and_offline_close_cannot_clobber_reconnect() {
        let hub = Hub::default();
        let owner = registration("42", "one");
        let auth = Authorization::Identity(owner.identity.clone());
        let mut records = Vec::new();
        for index in 0..10 {
            let p = proof(&hub, &owner);
            let identity = hub
                .verify(&owner, "https://relay.example", &p.header().unwrap())
                .unwrap();
            let id = format!("{index:064x}");
            let info = wire::ClientInfo {
                id: id.clone(),
                name: "work".into(),
                title: None,
                mode: wire::Mode::Shell,
                cwd: "/tmp".into(),
                hostname: "same-host".into(),
                routing: Some(identity.address(&id, "work", "same-host")),
            };
            hub.connected("42", "gh-42", &info).unwrap();
            records.push(info);
        }
        let inventory = hub.inventory("mcp", &auth);
        assert_eq!(inventory.len(), 10);
        let ids: std::collections::HashSet<_> = inventory
            .iter()
            .map(|v| v["routing"]["device_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 10);
        assert!(
            hub.inventory(
                "mcp",
                &Authorization::Identity(registration("43", "other").identity)
            )
            .is_empty()
        );
        let old = records.remove(0);
        let mut replacement = old.clone();
        replacement.id = "f".repeat(64);
        replacement.routing.as_mut().unwrap().connection_id = replacement.id.clone();
        assert!(hub.connected("42", "gh-42", &replacement).is_err());
        hub.disconnected(&old);
        assert_eq!(
            hub.inventory("mcp", &auth)
                .iter()
                .filter(|v| v["status"] == "offline")
                .count(),
            1
        );
        replacement.hostname = "renamed".into();
        hub.connected("42", "gh-42", &replacement).unwrap();
        hub.disconnected(&old);
        let current = hub.inventory("mcp", &auth);
        assert_eq!(
            current
                .iter()
                .filter(|v| v["status"] == "connected")
                .count(),
            10
        );
        assert!(
            current
                .iter()
                .any(|v| v["client_id"] == replacement.id && v["hostname"] == "renamed")
        );
    }
}

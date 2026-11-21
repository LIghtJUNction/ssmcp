//! Per-registration state for one explicitly paired TCP service. Only the broker
//! can dispatch these private operations. No address from the wire is dialed.
mod transport;

use crate::wire::peer_port::{Command, Mapping, Offer, Role};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::hmac;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
    time::Instant,
};

pub(crate) const CONFIG_ENV: &str = "SSMCP_PEER_PORT_CONFIG";
pub(crate) const UNREACHABLE: &str = "peer candidate unreachable";
const OFFER_WINDOW: Duration = Duration::from_secs(8);
const LEASE: Duration = Duration::from_secs(6);
type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    role: Role,
    listen: SocketAddr,
    target: SocketAddr,
    candidate: SocketAddr,
    #[serde(default)]
    bind: Option<SocketAddr>,
    pair_key: String,
}

struct Policy {
    role: Role,
    listen: SocketAddr,
    target: SocketAddr,
    candidate: SocketAddr,
    bind: SocketAddr,
    key: hmac::Key,
}

impl Config {
    fn policy(self) -> Result<Policy> {
        let key = STANDARD
            .decode(self.pair_key)
            .map_err(|_| "invalid peer pair key")?;
        let bind = self.bind.unwrap_or(self.candidate);
        if self.bind.is_some() && self.role != Role::Target {
            return Err("only a target may configure a local candidate bind".into());
        }
        if bind.ip().is_unspecified()
            || bind.ip().is_multicast()
            || bind.port() == 0
            || bind == self.target
            || bind == self.listen
        {
            return Err("invalid local candidate bind".into());
        }
        if key.len() != 32
            || !self.listen.ip().is_loopback()
            || !self.target.ip().is_loopback()
            || self.target.port() == 0
            || self.candidate.port() == 0
            || self.candidate.ip().is_unspecified()
            || self.candidate.ip().is_multicast()
            || self.candidate == self.target
            || self.candidate == self.listen
            || matches!(self.candidate.ip(), std::net::IpAddr::V4(ip) if ip.is_broadcast())
        {
            return Err("invalid peer port policy".into());
        }
        Ok(Policy {
            role: self.role,
            listen: self.listen,
            target: self.target,
            candidate: self.candidate,
            bind,
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
        })
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct Pending {
    mapping: Mapping,
    role: Role,
    offer: Offer,
    identity: rcgen::CertifiedKey<rcgen::KeyPair>,
    expires: Instant,
    deadline: Instant,
}

struct Active {
    id: String,
    deadline: Instant,
    lease: watch::Sender<Instant>,
    worker: JoinHandle<()>,
}

#[derive(Default)]
struct State {
    closed: bool,
    seen: HashMap<String, Instant>,
    pending: Option<Pending>,
    active: Option<Active>,
}

pub(crate) struct PeerPorts {
    client_id: String,
    policy: Option<Policy>,
    state: Mutex<State>,
}

impl PeerPorts {
    pub(crate) fn from_env(negotiated: bool, client_id: &str) -> Result<Arc<Self>> {
        let policy = if negotiated {
            let path = std::env::var_os(CONFIG_ENV).ok_or("missing local peer port policy")?;
            let file = std::fs::File::open(path).map_err(|_| "cannot open peer port policy")?;
            let meta = file
                .metadata()
                .map_err(|_| "cannot read peer port policy")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err("peer port policy requires owner-only permissions".into());
                }
            }
            if !meta.is_file() || meta.len() > 8192 {
                return Err("invalid peer port policy file".into());
            }
            let config: Config = serde_json::from_reader(file.take(8193))
                .map_err(|_| "invalid peer port policy JSON")?;
            Some(config.policy()?)
        } else {
            None
        };
        Ok(Arc::new(Self {
            client_id: client_id.into(),
            policy,
            state: Mutex::default(),
        }))
    }

    pub(crate) async fn perform(self: &Arc<Self>, command: Command) -> Result<Value> {
        let policy = self.policy.as_ref().ok_or("peer ports are disabled")?;
        let mut state = self.state.lock().await;
        let now = Instant::now();
        state.seen.retain(|_, expires| *expires > now);
        if state.closed {
            return Err("peer control connection closed".into());
        }
        if state.pending.as_ref().is_some_and(|p| p.expires <= now) {
            state.pending.take();
        }
        if state
            .active
            .as_ref()
            .is_some_and(|a| a.worker.is_finished())
        {
            state.active.take();
        }
        match command {
            Command::Prepare { mapping, role } => {
                let valid_id = |s: &str| !s.is_empty() && s.len() <= 64;
                let local = match role {
                    Role::Listen => &mapping.listen_client,
                    Role::Target => &mapping.target_client,
                };
                if role != policy.role
                    || local != &self.client_id
                    || mapping.listen_client == mapping.target_client
                    || mapping.listen != policy.listen
                    || mapping.target != policy.target
                    || ![&mapping.id, &mapping.listen_client, &mapping.target_client]
                        .iter()
                        .all(|s| valid_id(s))
                    || !(1000..=3_600_000).contains(&mapping.ttl_ms)
                {
                    return Err("mapping does not match the local peer policy".into());
                }
                let window = Duration::from_millis(mapping.offer_until_ms.saturating_sub(now_ms()));
                if window.is_zero() || window > OFFER_WINDOW || state.seen.contains_key(&mapping.id)
                {
                    return Err("expired or duplicate peer offer".into());
                }
                if state.pending.is_some() || state.active.is_some() || state.seen.len() >= 32 {
                    return Err("peer port slot is busy".into());
                }
                let identity = rcgen::generate_simple_self_signed(vec![transport::DNS.into()])
                    .map_err(|_| "peer key generation failed")?;
                let mut nonce = [0; 16];
                getrandom::fill(&mut nonce).map_err(|_| "peer nonce generation failed")?;
                let mut offer = Offer {
                    id: crate::hex::encode(nonce),
                    certificate: STANDARD.encode(identity.cert.der()),
                    proof: String::new(),
                };
                offer.proof = STANDARD.encode(hmac::sign(
                    &policy.key,
                    &transcript(&mapping, policy.candidate, role, &offer)?,
                ));
                let deadline = now + Duration::from_millis(mapping.ttl_ms);
                let expires = (now + window).min(deadline);
                state.seen.insert(mapping.id.clone(), now + window);
                let result = serde_json::to_value(&offer).map_err(|e| e.to_string())?;
                let weak = Arc::downgrade(self);
                let offer_id = offer.id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep_until(expires).await;
                    if let Some(peers) = weak.upgrade() {
                        let mut state = peers.state.lock().await;
                        if state
                            .pending
                            .as_ref()
                            .is_some_and(|p| p.offer.id == offer_id)
                        {
                            state.pending.take();
                        }
                    }
                });
                state.pending = Some(Pending {
                    mapping,
                    role,
                    offer,
                    identity,
                    expires,
                    deadline,
                });
                Ok(result)
            }
            Command::Activate {
                id,
                peer,
                peer_available,
            } => {
                if !state.pending.as_ref().is_some_and(|p| p.offer.id == id) {
                    return Err("unknown or consumed peer offer".into());
                }
                let p = state.pending.take().unwrap();
                if peer.id.len() != 32 || peer.certificate.len() > 2048 || peer.proof.len() != 44 {
                    return Err("peer identity or mapping did not authenticate".into());
                }
                let denial = "peer identity or mapping did not authenticate";
                let tag = STANDARD.decode(&peer.proof).map_err(|_| denial)?;
                let data = transcript(&p.mapping, policy.candidate, p.role.opposite(), &peer)?;
                hmac::verify(&policy.key, &data, &tag).map_err(|_| denial)?;
                if !peer_available {
                    return Err(UNREACHABLE.into());
                }
                let (lease, receiver) = watch::channel((now + LEASE).min(p.deadline));
                let (listener, worker) = tokio::time::timeout_at(
                    p.expires,
                    transport::activate(policy, &p, peer, receiver),
                )
                .await
                .map_err(|_| "peer offer expired during activation")??;
                state.active = Some(Active {
                    id,
                    deadline: p.deadline,
                    lease,
                    worker,
                });
                Ok(json!({"listener":listener}))
            }
            Command::Renew { id } => {
                let active = state
                    .active
                    .as_ref()
                    .filter(|a| a.id == id && *a.lease.borrow() > now)
                    .ok_or("peer lease is no longer live")?;
                active
                    .lease
                    .send((now + LEASE).min(active.deadline))
                    .map_err(|_| "peer worker stopped")?;
                Ok(json!({}))
            }
            Command::Close { id } => {
                if state.pending.as_ref().is_some_and(|p| p.offer.id == id) {
                    state.pending.take();
                }
                if state.active.as_ref().is_some_and(|a| a.id == id) {
                    stop(&mut state).await;
                }
                Ok(json!({"closed":true}))
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        state.closed = true;
        state.pending.take();
        stop(&mut state).await;
    }
}

async fn stop(state: &mut State) {
    if let Some(active) = state.active.take() {
        drop(active.lease);
        let _ = active.worker.await;
    }
}

fn transcript(
    mapping: &Mapping,
    candidate: SocketAddr,
    role: Role,
    offer: &Offer,
) -> Result<Vec<u8>> {
    // Fixed, ordered serialization. The pair key and candidate never enter a relay frame.
    serde_json::to_vec(&(
        "ssmcp-peer-port-v1",
        mapping,
        candidate,
        role,
        &offer.id,
        &offer.certificate,
    ))
    .map_err(|e| e.to_string())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

//! Per-registration state for one explicitly paired TCP service. Only the broker
//! can dispatch these private operations. Discovered candidates require both a
//! pair-key signature and an explicit local network policy before any dial.
mod candidates;
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
    net::TcpSocket,
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
    #[serde(default)]
    candidate: Option<SocketAddr>,
    #[serde(default)]
    auto_candidates: Option<Vec<String>>,
    #[serde(default)]
    bind: Option<SocketAddr>,
    pair_key: String,
}

struct Policy {
    role: Role,
    listen: SocketAddr,
    target: SocketAddr,
    candidates: CandidatePolicy,
    key: hmac::Key,
}

enum CandidatePolicy {
    Fixed {
        address: SocketAddr,
        bind: SocketAddr,
    },
    Automatic(candidates::Networks),
}

impl Config {
    fn capability(&self) -> &'static str {
        if self.auto_candidates.is_some() {
            crate::wire::peer_port::AUTO_CAPABILITY
        } else {
            crate::wire::peer_port::CAPABILITY
        }
    }

    fn negotiated_policy(self, capability: &str) -> Result<Policy> {
        if self.capability() != capability {
            return Err("peer candidate mode changed during registration".into());
        }
        self.policy()
    }

    fn policy(self) -> Result<Policy> {
        let key = STANDARD
            .decode(self.pair_key)
            .map_err(|_| "invalid peer pair key")?;
        if key.len() != 32
            || !self.listen.ip().is_loopback()
            || !self.target.ip().is_loopback()
            || self.target.port() == 0
        {
            return Err("invalid peer port policy".into());
        }
        let candidates = match (self.candidate, self.auto_candidates) {
            (Some(address), None) => {
                let bind = self.bind.unwrap_or(address);
                if self.bind.is_some() && self.role != Role::Target {
                    return Err("only a target may configure a local candidate bind".into());
                }
                for addr in [address, bind] {
                    if addr.ip().is_unspecified()
                        || addr.ip().is_multicast()
                        || addr.port() == 0
                        || addr == self.target
                        || addr == self.listen
                        || matches!(addr.ip(), std::net::IpAddr::V4(ip) if ip.is_broadcast())
                    {
                        return Err("invalid local candidate address".into());
                    }
                }
                CandidatePolicy::Fixed { address, bind }
            }
            (None, Some(networks)) if self.bind.is_none() => {
                CandidatePolicy::Automatic(candidates::Networks::parse(&networks)?)
            }
            _ => return Err(
                "configure either candidate or auto_candidates, without an automatic bind override"
                    .into(),
            ),
        };
        Ok(Policy {
            role: self.role,
            listen: self.listen,
            target: self.target,
            candidates,
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
        })
    }

    fn from_env() -> Result<Self> {
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
        serde_json::from_reader(file.take(8193)).map_err(|_| "invalid peer port policy JSON".into())
    }
}

/// Exactly one protocol is offered. Invalid files retain the v1 registration
/// path so the existing post-registration policy loader reports the error.
pub(crate) fn offered_capability() -> Option<&'static str> {
    std::env::var_os(CONFIG_ENV)?;
    Some(
        Config::from_env().map_or(crate::wire::peer_port::CAPABILITY, |config| {
            config.capability()
        }),
    )
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
    // Bound but not listening until the partner's signed offer is verified.
    sockets: Vec<TcpSocket>,
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
    pub(crate) fn from_env(negotiated: Option<&str>, client_id: &str) -> Result<Arc<Self>> {
        let policy = negotiated
            .map(|capability| Config::from_env()?.negotiated_policy(capability))
            .transpose()?;
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
                let (candidates, sockets) = reserve_candidates(policy)?;
                let mut offer = Offer {
                    id: crate::hex::encode(nonce),
                    certificate: STANDARD.encode(identity.cert.der()),
                    proof: String::new(),
                    candidates,
                };
                offer.proof = STANDARD.encode(hmac::sign(
                    &policy.key,
                    &transcript(&mapping, policy, role, &offer)?,
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
                    sockets,
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
                let data = transcript(&p.mapping, policy, p.role.opposite(), &peer)?;
                hmac::verify(&policy.key, &data, &tag).map_err(|_| denial)?;
                validate_candidates(policy, p.role.opposite(), &peer)?;
                if !peer_available {
                    return Err(UNREACHABLE.into());
                }
                let (lease, receiver) = watch::channel((now + LEASE).min(p.deadline));
                let deadline = p.deadline;
                let (listener, worker) = tokio::time::timeout_at(
                    p.expires,
                    transport::activate(policy, p, peer, receiver),
                )
                .await
                .map_err(|_| "peer offer expired during activation")??;
                state.active = Some(Active {
                    id,
                    deadline,
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

type ReservedCandidates = (Option<Vec<SocketAddr>>, Vec<TcpSocket>);

fn reserve_candidates(policy: &Policy) -> Result<ReservedCandidates> {
    let CandidatePolicy::Automatic(networks) = &policy.candidates else {
        return Ok((None, Vec::new()));
    };
    let mut addresses = Vec::new();
    let mut sockets = Vec::new();
    if policy.role == Role::Target {
        for ip in networks.gather()? {
            let socket = if ip.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            }
            .map_err(|_| "cannot create candidate socket")?;
            // Ephemeral ports are reserved before signing. The socket does not
            // accept connections until Activate authenticates the partner.
            if let Err(error) = socket.bind(SocketAddr::new(ip, 0)) {
                match error.kind() {
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::AddrInUse => {
                        continue;
                    }
                    _ => return Err("cannot bind candidate socket".into()),
                }
            }
            let address = socket
                .local_addr()
                .map_err(|_| "cannot read candidate address")?;
            if networks.allows(address) && address != policy.listen && address != policy.target {
                addresses.push(address);
                sockets.push(socket);
            }
        }
    }
    Ok((Some(addresses), sockets))
}

fn validate_candidates(policy: &Policy, role: Role, offer: &Offer) -> Result<()> {
    match (&policy.candidates, &offer.candidates) {
        (CandidatePolicy::Fixed { .. }, None) => Ok(()),
        (CandidatePolicy::Automatic(networks), Some(addresses)) => {
            if addresses.len() > candidates::MAX_CANDIDATES
                || (role == Role::Listen && !addresses.is_empty())
                || addresses.iter().enumerate().any(|(index, address)| {
                    !networks.allows(*address)
                        || *address == policy.listen
                        || *address == policy.target
                        || addresses[..index].contains(address)
                })
            {
                return Err("peer candidates violate the local network policy".into());
            }
            Ok(())
        }
        _ => Err("peer candidate mode did not authenticate".into()),
    }
}

fn transcript(mapping: &Mapping, policy: &Policy, role: Role, offer: &Offer) -> Result<Vec<u8>> {
    // Preserve the v1 transcript byte-for-byte. Automatic offers use a distinct
    // domain and authenticate the entire ordered candidate list, never the key.
    match (&policy.candidates, &offer.candidates) {
        (CandidatePolicy::Fixed { address, .. }, None) => serde_json::to_vec(&(
            "ssmcp-peer-port-v1",
            mapping,
            address,
            role,
            &offer.id,
            &offer.certificate,
        )),
        (CandidatePolicy::Automatic(_), Some(addresses))
            if addresses.len() <= candidates::MAX_CANDIDATES =>
        {
            serde_json::to_vec(&(
                "ssmcp-peer-port-auto-v1",
                mapping,
                addresses,
                role,
                &offer.id,
                &offer.certificate,
            ))
        }
        _ => return Err("peer candidate mode did not authenticate".into()),
    }
    .map_err(|e| e.to_string())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

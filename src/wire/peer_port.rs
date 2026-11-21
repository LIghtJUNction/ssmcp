//! Private control messages on the negotiated, authenticated relay connection.
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub const CAPABILITY: &str = "peer_ports_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Listen,
    Target,
}

impl Role {
    pub(crate) fn opposite(self) -> Self {
        match self {
            Self::Listen => Self::Target,
            Self::Target => Self::Listen,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub id: String,
    pub listen_client: String,
    pub target_client: String,
    pub listen: SocketAddr,
    pub target: SocketAddr,
    pub offer_until_ms: u64,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub id: String,
    pub certificate: String,
    pub proof: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Prepare {
        mapping: Mapping,
        role: Role,
    },
    Activate {
        id: String,
        peer: Offer,
        peer_available: bool,
    },
    Renew {
        id: String,
    },
    Close {
        id: String,
    },
}

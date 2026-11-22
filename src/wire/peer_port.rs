//! Private control messages on the negotiated, authenticated relay connection.
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub const CAPABILITY: &str = "peer_ports_v1";
pub const AUTO_CAPABILITY: &str = "peer_ports_auto_v1";
pub(crate) const MAX_CANDIDATES: usize = 4;

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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_candidates"
    )]
    pub candidates: Option<Vec<SocketAddr>>,
}

fn deserialize_candidates<'de, D>(deserializer: D) -> Result<Option<Vec<SocketAddr>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Bounded(Vec<SocketAddr>);
    impl<'de> Deserialize<'de> for Bounded {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct Addresses;
            impl<'de> serde::de::Visitor<'de> for Addresses {
                type Value = Bounded;

                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    formatter.write_str("at most four peer TCP candidates")
                }

                fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
                where
                    A: serde::de::SeqAccess<'de>,
                {
                    let mut addresses = Vec::new();
                    while let Some(address) = sequence.next_element()? {
                        if addresses.len() == MAX_CANDIDATES {
                            return Err(serde::de::Error::custom("too many peer TCP candidates"));
                        }
                        addresses.push(address);
                    }
                    Ok(Bounded(addresses))
                }
            }
            deserializer.deserialize_seq(Addresses)
        }
    }
    Option::<Bounded>::deserialize(deserializer).map(|value| value.map(|bounded| bounded.0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn static_offer_retains_the_exact_v1_wire_shape() {
        let legacy = json!({"id":"nonce", "certificate":"cert", "proof":"tag"});
        let offer: Offer = serde_json::from_value(legacy.clone()).unwrap();
        assert!(offer.candidates.is_none());
        assert_eq!(serde_json::to_value(offer).unwrap(), legacy);
        let mut unknown = legacy;
        unknown["unrecognized"] = json!(true);
        assert!(serde_json::from_value::<Offer>(unknown).is_err());
    }

    #[test]
    fn automatic_offer_roundtrips_only_a_bounded_literal_address_list() {
        let mut value = json!({"id":"nonce", "certificate":"cert", "proof":"tag",
            "candidates":["192.0.2.1:443", "[2001:db8::1]:443"]});
        let offer: Offer = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(offer.candidates.as_ref().unwrap().len(), 2);
        assert_eq!(serde_json::to_value(offer).unwrap(), value);
        value["candidates"] = json!(vec!["192.0.2.1:443"; 5]);
        assert!(serde_json::from_value::<Offer>(value.clone()).is_err());
        value["candidates"] = json!(["peer.example:443"]);
        assert!(serde_json::from_value::<Offer>(value).is_err());
    }
}

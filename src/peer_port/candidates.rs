//! Local network consent and host candidates. Enumeration never sends packets.
use std::net::{IpAddr, SocketAddr};

pub(super) use crate::wire::peer_port::MAX_CANDIDATES;
const MAX_NETWORKS: usize = 16;

struct Network {
    address: IpAddr,
    prefix: u8,
}

pub(super) struct Networks(Vec<Network>);

impl Networks {
    pub(super) fn parse(values: &[String]) -> Result<Self, String> {
        if values.is_empty() || values.len() > MAX_NETWORKS {
            return Err("automatic peer discovery requires 1 to 16 explicit networks".into());
        }
        let mut networks = Vec::with_capacity(values.len());
        for value in values {
            let (address, prefix) = value
                .split_once('/')
                .ok_or("peer network must be an IP literal with a CIDR prefix")?;
            let address: IpAddr = address
                .parse()
                .map_err(|_| "invalid peer network address")?;
            let prefix: u8 = prefix.parse().map_err(|_| "invalid peer network prefix")?;
            let canonical = match address {
                IpAddr::V4(ip) if (1..=32).contains(&prefix) => {
                    u32::from(ip) & u32::MAX.checked_shr(prefix.into()).unwrap_or(0) == 0
                }
                IpAddr::V6(ip) if (1..=128).contains(&prefix) => {
                    u128::from(ip) & u128::MAX.checked_shr(prefix.into()).unwrap_or(0) == 0
                }
                _ => false,
            };
            if !canonical || !usable_ip(address) {
                return Err(
                    "peer network must be a canonical unicast CIDR with a nonzero prefix".into(),
                );
            }
            networks.push(Network { address, prefix });
        }
        Ok(Self(networks))
    }

    pub(super) fn allows(&self, address: SocketAddr) -> bool {
        if address.port() == 0
            || !usable_ip(address.ip())
            || matches!(address, SocketAddr::V6(v6) if v6.scope_id() != 0 || v6.flowinfo() != 0)
        {
            return false;
        }
        self.0.iter().any(|network| network.contains(address.ip()))
    }

    pub(super) fn gather(&self) -> Result<Vec<IpAddr>, String> {
        let mut addresses = interface_addresses()?;
        // A nonzero placeholder port applies the same address policy used for
        // authenticated remote candidates. Real ports are assigned on bind.
        addresses.retain(|address| self.allows(SocketAddr::new(*address, 1)));
        addresses.sort_unstable();
        addresses.dedup();
        addresses.truncate(MAX_CANDIDATES);
        Ok(addresses)
    }
}

impl Network {
    fn contains(&self, address: IpAddr) -> bool {
        // A broad network that happens to contain loopback is not permission
        // to connect to local services. Loopback requires its own explicit CIDR.
        if address.is_loopback() && !self.address.is_loopback() {
            return false;
        }
        match (self.address, address) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                if ip.is_loopback() && self.prefix < 8 {
                    return false;
                }
                let host_mask = u32::MAX.checked_shr(self.prefix.into()).unwrap_or(0);
                let ip = u32::from(ip);
                ip & !host_mask == u32::from(network)
                    && (self.prefix >= 31 || ip & host_mask != host_mask)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                (!ip.is_loopback() || self.prefix == 128)
                    && u128::from(ip) & !u128::MAX.checked_shr(self.prefix.into()).unwrap_or(0)
                        == u128::from(network)
            }
            _ => false,
        }
    }
}

fn usable_ip(address: IpAddr) -> bool {
    if address.is_unspecified() || address.is_multicast() {
        return false;
    }
    match address {
        IpAddr::V4(ip) => !ip.is_broadcast() && !ip.is_link_local(),
        IpAddr::V6(ip) => !ip.is_unicast_link_local() && ip.to_ipv4_mapped().is_none(),
    }
}

#[cfg(unix)]
fn interface_addresses() -> Result<Vec<IpAddr>, String> {
    use std::net::{Ipv4Addr, Ipv6Addr};

    struct Interfaces(*mut libc::ifaddrs);
    impl Drop for Interfaces {
        fn drop(&mut self) {
            // SAFETY: getifaddrs owns this list until exactly this freeifaddrs.
            unsafe { libc::freeifaddrs(self.0) };
        }
    }

    let mut first = std::ptr::null_mut();
    // SAFETY: first is a valid out pointer; success transfers list ownership.
    if unsafe { libc::getifaddrs(&mut first) } != 0 {
        return Err("cannot enumerate local peer interfaces".into());
    }
    let interfaces = Interfaces(first);
    let mut next = interfaces.0;
    let mut addresses = Vec::new();
    while !next.is_null() {
        // SAFETY: the list remains allocated by interfaces for this iteration.
        let interface = unsafe { &*next };
        next = interface.ifa_next;
        if interface.ifa_addr.is_null() || interface.ifa_flags & libc::IFF_UP as u32 == 0 {
            continue;
        }
        // SAFETY: getifaddrs supplies a sockaddr matching its sa_family, with
        // storage and alignment for sockaddr_in / sockaddr_in6 respectively.
        let address = unsafe {
            match (*interface.ifa_addr).sa_family as libc::c_int {
                libc::AF_INET => {
                    let address = &*interface.ifa_addr.cast::<libc::sockaddr_in>();
                    Some(IpAddr::V4(Ipv4Addr::from(
                        address.sin_addr.s_addr.to_ne_bytes(),
                    )))
                }
                libc::AF_INET6 => {
                    let address = &*interface.ifa_addr.cast::<libc::sockaddr_in6>();
                    (address.sin6_scope_id == 0 && address.sin6_flowinfo == 0)
                        .then_some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
                }
                _ => None,
            }
        };
        if let Some(address) = address {
            addresses.push(address);
        }
    }
    Ok(addresses)
}

#[cfg(not(unix))]
fn interface_addresses() -> Result<Vec<IpAddr>, String> {
    // A target without an enumerator still signs its empty candidate offer.
    // Both peers authenticate before prefer_direct falls back to the relay.
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv6Addr, SocketAddrV6};

    fn networks(values: &[&str]) -> Networks {
        Networks::parse(
            &values
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn requires_bounded_canonical_literal_networks() {
        for value in [
            "example.com/24",
            "192.0.2.0",
            "192.0.2.1/24",
            "192.0.2.0/33",
            "192.0.2.0/-1",
            "192.0.2.0/0",
            "2001:db8::1/64",
            "2001:db8::/129",
            "::/0",
            "0.0.0.0/32",
            "224.0.0.0/4",
            "255.255.255.255/32",
            "169.254.0.0/16",
            "fe80::/10",
            "ff00::/8",
            "::ffff:192.0.2.0/120",
        ] {
            assert!(Networks::parse(&[value.into()]).is_err(), "{value}");
        }
        assert!(Networks::parse(&[]).is_err());
        assert!(Networks::parse(&vec!["192.0.2.0/24".into(); MAX_NETWORKS + 1]).is_err());
    }

    #[test]
    fn respects_ip_family_network_and_port_boundaries() {
        let policy = networks(&["192.0.2.0/24", "2001:db8:42::/48"]);
        for address in [
            "192.0.2.1:443",
            "192.0.2.254:443",
            "[2001:db8:42:ffff::1]:443",
        ] {
            assert!(policy.allows(address.parse().unwrap()), "{address}");
        }
        for address in [
            "192.0.3.1:443",
            "192.0.2.1:0",
            "192.0.2.255:443",
            "[2001:db8:43::1]:443",
            "[::ffff:192.0.2.1]:443",
        ] {
            assert!(!policy.allows(address.parse().unwrap()), "{address}");
        }
        assert!(networks(&["192.0.2.254/31"]).allows("192.0.2.255:443".parse().unwrap()));
        assert!(networks(&["192.0.2.255/32"]).allows("192.0.2.255:443".parse().unwrap()));
    }

    #[test]
    fn rejects_special_addresses_even_inside_broad_networks() {
        let policy = networks(&["128.0.0.0/1", "8000::/1"]);
        for address in [
            "169.254.1.1:443",
            "224.0.0.1:443",
            "255.255.255.255:443",
            "[fe80::1]:443",
            "[ff02::1]:443",
        ] {
            assert!(!policy.allows(address.parse().unwrap()), "{address}");
        }
        let policy = networks(&["2001:db8::/32"]);
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert!(!policy.allows(SocketAddr::V6(SocketAddrV6::new(ip, 443, 1, 0))));
        assert!(!policy.allows(SocketAddr::V6(SocketAddrV6::new(ip, 443, 0, 1))));
    }

    #[test]
    fn loopback_requires_a_wholly_loopback_network() {
        let broad = networks(&["64.0.0.0/2"]);
        assert!(!broad.allows("127.0.0.1:443".parse().unwrap()));
        let explicit = networks(&["127.0.0.0/8", "::1/128"]);
        assert!(explicit.allows("127.0.0.1:443".parse().unwrap()));
        assert!(explicit.allows("[::1]:443".parse().unwrap()));
        assert!(!explicit.allows("[::]:443".parse().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn gathers_only_allowed_sorted_unique_host_addresses() {
        let policy = networks(&["127.0.0.0/8", "::1/128"]);
        let addresses = policy.gather().unwrap();
        assert!(
            !addresses.is_empty(),
            "an up loopback interface is required"
        );
        assert!(addresses.len() <= MAX_CANDIDATES);
        assert!(addresses.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            addresses
                .iter()
                .all(|ip| ip.is_loopback() && policy.allows(SocketAddr::new(*ip, 1)))
        );
    }
}

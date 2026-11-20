//! Negotiated connection metadata; running server versions are not release offers.
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub const CAPABILITY: &str = "version_notice_v1";
pub(crate) const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub server_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_client: Option<PublishedClient>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishedClient {
    pub version: String,
    pub targets: Vec<String>,
}

impl PublishedClient {
    pub(crate) fn valid(&self) -> bool {
        Version::parse(&self.version).is_some_and(|version| version.stable())
            && !self.targets.is_empty()
            && self.targets.len() <= TARGETS.len()
            && self.targets.iter().enumerate().all(|(index, target)| {
                TARGETS.contains(&target.as_str()) && !self.targets[..index].contains(target)
            })
    }
}

impl Notice {
    pub(crate) fn valid(&self) -> bool {
        Version::parse(&self.server_version).is_some()
            && self
                .published_client
                .as_ref()
                .is_none_or(PublishedClient::valid)
    }
}

/// SemVer precedence without build metadata. Published stable versions and
/// installed prereleases share one comparator; display text never reaches it unchecked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    pub(crate) core: [u64; 3],
    prerelease: Vec<String>,
}

impl Version {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > 128 {
            return None;
        }
        let value = value.strip_prefix('v').unwrap_or(value);
        let (precedence, build) = value
            .split_once('+')
            .map_or((value, None), |(v, b)| (v, Some(b)));
        if build.is_some_and(|build| !identifiers(build, false)) {
            return None;
        }
        let (core, prerelease) = precedence
            .split_once('-')
            .map_or((precedence, None), |(v, p)| (v, Some(p)));
        if prerelease.is_some_and(|pre| !identifiers(pre, true)) {
            return None;
        }
        let mut parts = core.split('.');
        let mut numbers = [0; 3];
        for number in &mut numbers {
            let part = parts.next()?;
            if part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return None;
            }
            *number = part.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            core: numbers,
            prerelease: prerelease
                .map(|pre| pre.split('.').map(str::to_owned).collect())
                .unwrap_or_default(),
        })
    }

    pub(crate) fn stable(&self) -> bool {
        self.prerelease.is_empty()
    }
}

fn identifiers(value: &str, prerelease: bool) -> bool {
    value.split('.').all(|part| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !(prerelease
                && part.len() > 1
                && part.starts_with('0')
                && part.bytes().all(|b| b.is_ascii_digit()))
    })
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core = self.core.cmp(&other.core);
        if core != Ordering::Equal {
            return core;
        }
        match (self.stable(), other.stable()) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        for (left, right) in self.prerelease.iter().zip(&other.prerelease) {
            let numeric = |value: &str| value.bytes().all(|b| b.is_ascii_digit());
            let order = match (numeric(left), numeric(right)) {
                (true, true) => left.len().cmp(&right.len()).then_with(|| left.cmp(right)),
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => left.cmp(right),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        self.prerelease.len().cmp(&other.prerelease.len())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> Version {
        Version::parse(value).unwrap()
    }

    #[test]
    fn version_notice_uses_semver_precedence_including_prereleases_and_builds() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(version(pair[0]) < version(pair[1]), "{pair:?}");
        }
        assert!(version("0.9.99") < version("0.10.0"));
        assert!(version("0.10.9") < version("0.10.10"));
        assert_eq!(version("v1.2.3+local.01"), version("1.2.3+published"));
        assert!(version("1.2.3-alpha.9999999999999999999999999999") > version("1.2.3-alpha.10"));
        for invalid in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-alpha.01",
            "1.2.3-alpha..one",
            "1.2.3+x+y",
            "1.2.3\n",
            "1.2.3-界",
            "18446744073709551616.0.0",
        ] {
            assert!(Version::parse(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn version_notice_accepts_only_bounded_stable_platform_publications() {
        let mut notice = Notice {
            server_version: "0.10.18-dev+build".into(),
            published_client: None,
        };
        assert!(notice.valid());
        notice.published_client = Some(PublishedClient {
            version: "0.10.17".into(),
            targets: vec![TARGETS[0].into()],
        });
        assert!(notice.valid());
        let published = notice.published_client.as_mut().unwrap();
        published.targets.push(TARGETS[0].into());
        assert!(!notice.valid());
        notice.published_client.as_mut().unwrap().targets = vec!["../../other".into()];
        assert!(!notice.valid());
        notice.published_client.as_mut().unwrap().targets = vec![TARGETS[0].into()];
        notice.published_client.as_mut().unwrap().version = "0.10.18-rc.1".into();
        assert!(!notice.valid());
    }
}

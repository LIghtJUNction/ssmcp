use super::{GitSnapshot, GitState, fetch, valid_oid};
use crate::wire::GitTrackingChange;
use std::path::Path;
use tokio::sync::watch;

#[derive(PartialEq, Eq)]
struct Tips {
    branch: String,
    upstream: String,
    head: String,
    upstream_oid: String,
}

impl Tips {
    async fn read(git: &Path, root: &Path, stopped: &mut watch::Receiver<bool>) -> Option<Self> {
        // Fixed arguments derive both names from HEAD without passing repository-controlled
        // names to Git as revisions. --symbolic-full-name affects the arguments following it.
        let output = fetch::read(
            git,
            root,
            &[
                "rev-parse",
                "HEAD",
                "@{upstream}",
                "--symbolic-full-name",
                "HEAD",
                "@{upstream}",
            ],
            stopped,
        )
        .await
        .ok()?;
        if output.code != Some(0) {
            return None;
        }
        let mut lines = std::str::from_utf8(&output.stdout).ok()?.lines();
        let head = lines.next()?;
        let upstream_oid = lines.next()?;
        let branch = lines.next()?.strip_prefix("refs/heads/")?;
        let upstream = lines.next()?;
        if lines.next().is_some()
            || !valid_oid(head.as_bytes())
            || !valid_oid(upstream_oid.as_bytes())
            || head.len() != upstream_oid.len()
        {
            return None;
        }
        Some(Self {
            branch: branch.to_owned(),
            upstream: upstream.to_owned(),
            head: head.to_owned(),
            upstream_oid: upstream_oid.to_owned(),
        })
    }

    async fn divergence(
        &self,
        git: &Path,
        root: &Path,
        stopped: &mut watch::Receiver<bool>,
    ) -> Option<(u64, u64)> {
        let range = format!("{}...{}", self.head, self.upstream_oid);
        let output = fetch::read(
            git,
            root,
            &["rev-list", "--left-right", "--count", &range, "--"],
            stopped,
        )
        .await
        .ok()?;
        if output.code != Some(0) {
            return None;
        }
        let mut counts = std::str::from_utf8(&output.stdout)
            .ok()?
            .split_ascii_whitespace();
        let ahead = counts.next()?.parse().ok()?;
        let behind = counts.next()?.parse().ok()?;
        counts.next().is_none().then_some((ahead, behind))
    }
}

fn upstream_matches(full: &str, shown: &str) -> bool {
    // Porcelain can retain the namespace in a short name to disambiguate it.
    full == shown
        || full.strip_prefix("refs/") == Some(shown)
        || full.strip_prefix("refs/remotes/") == Some(shown)
}

struct Sample {
    tips: Tips,
    counts: (u64, u64),
}

#[derive(Default)]
pub(super) struct Tracking {
    previous: Option<Sample>,
}

impl Tracking {
    pub(super) async fn observe(
        &mut self,
        git: &Path,
        root: &Path,
        snapshot: &GitSnapshot,
        stopped: &mut watch::Receiver<bool>,
    ) -> Option<GitTrackingChange> {
        if snapshot.status == GitState::NotRepository {
            self.previous = None;
        }
        if snapshot.status != GitState::Ready || snapshot.stale {
            return None;
        }
        let (Some(branch), Some(head), Some(upstream)) = (
            snapshot.branch.as_deref(),
            snapshot.head.as_deref(),
            snapshot.upstream.as_deref(),
        ) else {
            self.previous = None;
            return None;
        };
        if snapshot.detached || snapshot.unborn {
            self.previous = None;
            return None;
        }
        if self.previous.as_ref().is_some_and(|sample| {
            sample.tips.branch != branch || !upstream_matches(&sample.tips.upstream, upstream)
        }) {
            self.previous = None;
        }

        if snapshot.ahead.is_none() || snapshot.behind.is_none() {
            return None;
        }
        let mut tips = Tips::read(git, root, stopped).await?;
        for _ in 0..2 {
            if tips.branch != branch || tips.head != head {
                return None;
            }
            if !tips.upstream.starts_with("refs/remotes/") {
                self.previous = None;
                return None;
            }
            if !upstream_matches(&tips.upstream, upstream)
                || self.previous.as_ref().is_some_and(|sample| {
                    sample.tips == tips
                        && Some(sample.counts) == snapshot.ahead.zip(snapshot.behind)
                })
            {
                return None;
            }

            // Status has no upstream OID. Pin both commits for the count, including
            // when a shallow boundary moves without changing either commit.
            let counts = tips.divergence(git, root, stopped).await?;
            let verified = Tips::read(git, root, stopped).await?;
            if tips != verified {
                tips = verified;
                continue;
            }
            let current = Sample { tips, counts };
            let event = self.previous.as_ref().and_then(|previous| {
                if previous.tips.branch != current.tips.branch
                    || previous.tips.upstream != current.tips.upstream
                    || (previous.tips.upstream_oid == current.tips.upstream_oid
                        && previous.counts == current.counts)
                {
                    return None;
                }
                let event = GitTrackingChange {
                    branch: current.tips.branch.clone(),
                    upstream: current.tips.upstream.clone(),
                    head: current.tips.head.clone(),
                    upstream_oid: current.tips.upstream_oid.clone(),
                    previous_upstream_oid: previous.tips.upstream_oid.clone(),
                    ahead: counts.0,
                    behind: counts.1,
                };
                event.valid().then_some(event)
            });
            self.previous = Some(current);
            return event;
        }
        None
    }
}

#[cfg(test)]
mod tests;

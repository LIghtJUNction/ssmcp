use super::{Failure, GitSnapshot, GitState};
use crate::wire::repository_events::{Counts, Event};

#[derive(Default)]
pub(super) struct Comparison {
    last: Option<(String, String, Counts)>,
}

impl Comparison {
    pub(super) fn observe(&mut self, observation: &Result<GitSnapshot, Failure>) -> Option<Event> {
        let snapshot = match observation {
            Ok(snapshot) => snapshot,
            Err(Failure::NotRepository) => {
                self.last = None;
                return None;
            }
            Err(_) => return None,
        };
        if snapshot.status == GitState::NotRepository || snapshot.detached || snapshot.unborn {
            self.last = None;
            return None;
        }
        if snapshot.status != GitState::Ready || snapshot.stale {
            return None;
        }
        let (Some(branch), Some(upstream)) = (&snapshot.branch, &snapshot.upstream) else {
            self.last = None;
            return None;
        };
        let previous = self
            .last
            .as_ref()
            .and_then(|(old_branch, old_upstream, counts)| {
                (old_branch == branch && old_upstream == upstream).then_some(*counts)
            });
        if previous.is_none() {
            self.last = None;
        }
        let counts = Counts {
            ahead: snapshot.ahead?,
            behind: snapshot.behind?,
        };
        if previous == Some(counts) {
            return None;
        }
        // The terminal view escapes controls, replaces invalid UTF-8 and truncates
        // after 512 characters. Such labels cannot identify a ref unambiguously.
        if [branch, upstream]
            .iter()
            .any(|name| name.contains(['\\', '\u{fffd}']) || name.chars().count() > 512)
        {
            return None;
        }
        let event = Event::UpstreamChanged {
            branch: branch.clone(),
            upstream: upstream.clone(),
            ahead: counts.ahead,
            behind: counts.behind,
            previous,
        };
        event.validate().ok()?;
        self.last = Some((branch.clone(), upstream.clone(), counts));
        (previous.is_some() || counts.ahead != 0 || counts.behind != 0).then_some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::git_status::parse;

    fn status(
        branch: &str,
        upstream: &str,
        ahead: u64,
        behind: u64,
    ) -> Result<GitSnapshot, Failure> {
        let record = format!(
            "# branch.oid abcdef\0# branch.head {branch}\0# branch.upstream {upstream}\0# branch.ab +{ahead} -{behind}\0"
        );
        parse(record.as_bytes()).map_err(Failure::Error)
    }

    fn changed(ahead: u64, behind: u64, prior: Option<(u64, u64)>) -> Option<Event> {
        Some(Event::UpstreamChanged {
            branch: "main".into(),
            upstream: "origin/main".into(),
            ahead,
            behind,
            previous: prior.map(|(ahead, behind)| Counts { ahead, behind }),
        })
    }

    #[test]
    fn reports_each_divergence_transition_once_and_preserves_last_good_counts() {
        let mut tracker = Comparison::default();
        let zero = status("main", "origin/main", 0, 0);
        let remote_ahead = status("main", "origin/main", 0, 4);
        let diverged = status("main", "origin/main", 2, 4);
        assert_eq!(tracker.observe(&zero), None);
        assert_eq!(tracker.observe(&remote_ahead), changed(0, 4, Some((0, 0))));
        assert_eq!(tracker.observe(&remote_ahead), None);
        assert_eq!(tracker.observe(&Err(Failure::Error("temporary"))), None);
        let mut stale = status("main", "origin/main", 100, 100).unwrap();
        stale.stale = true;
        assert_eq!(tracker.observe(&Ok(stale)), None);
        assert_eq!(tracker.observe(&diverged), changed(2, 4, Some((0, 4))));
        assert_eq!(tracker.observe(&zero), changed(0, 0, Some((2, 4))));
    }

    #[test]
    fn resets_on_new_tracking_relationship_or_after_leaving_a_repository() {
        let mut tracker = Comparison::default();
        let initial = status("main", "origin/main", 1, 2);
        assert_eq!(tracker.observe(&initial), changed(1, 2, None));
        let switched = status("main", "fork/main", 0, 0);
        assert_eq!(tracker.observe(&switched), None);
        assert_eq!(tracker.observe(&initial), changed(1, 2, None));
        for observation in [
            parse(b"# branch.oid abcdef\0# branch.head (detached)\0").map_err(Failure::Error),
            parse(b"# branch.oid abcdef\0# branch.head main\0").map_err(Failure::Error),
            Err(Failure::NotRepository),
        ] {
            assert_eq!(tracker.observe(&observation), None);
            assert_eq!(tracker.observe(&initial), changed(1, 2, None));
        }
    }

    #[test]
    fn unknown_counts_and_ambiguous_names_never_create_an_event() {
        let mut tracker = Comparison::default();
        let mut unknown = status("main", "origin/main", 4, 2).unwrap();
        unknown.behind = None;
        assert_eq!(tracker.observe(&Ok(unknown)), None);
        assert_eq!(
            tracker.observe(&status(&"x".repeat(514), "origin/main", 3, 4)),
            None
        );
        assert_eq!(
            tracker.observe(&status("main", "origin/main", 4, 2)),
            changed(4, 2, None)
        );
    }
}

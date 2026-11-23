//! Bounded reconnect pacing shared by full clients and the recovery link.
use std::time::Duration;

pub(super) fn delay(base_seconds: u64, consecutive_failures: u32, entropy: u8) -> Duration {
    let base = base_seconds.clamp(1, 3600).saturating_mul(1000);
    let cap = base.max(60_000);
    let exponential = base
        .saturating_mul(1u64 << consecutive_failures.min(16))
        .min(cap);
    Duration::from_millis(
        exponential
            .saturating_add(exponential * u64::from(entropy) / 1024)
            .min(cap),
    )
}

/// Both values are non-secret (verified-subject identifier, credential digest).
/// Changing the account cannot silently continue a revoked recovery identity.
pub(super) fn renewed_same_account(
    previous: Option<&(String, String)>,
    current: Option<&(String, String)>,
) -> bool {
    matches!((previous,current),(Some((old_subject,old_key)),Some((subject,key)))
        if old_subject==subject && old_key!=key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exponential_reconnect_has_a_bound_and_never_overflows() {
        assert_eq!(delay(5, 0, 0), Duration::from_secs(5));
        assert_eq!(delay(5, 1, 0), Duration::from_secs(10));
        assert!(delay(5, 2, 255) > delay(5, 1, 255));
        assert_eq!(delay(5, u32::MAX, 255), Duration::from_secs(60));
        assert_eq!(delay(3600, u32::MAX, 255), Duration::from_secs(3600));
        assert!(delay(0, 0, 0) >= Duration::from_secs(1));
    }
    #[test]
    fn revoked_credentials_do_not_resume_on_time_or_an_account_switch() {
        let old = ("42".into(), "old".into());
        assert!(!renewed_same_account(Some(&old), Some(&old)));
        assert!(!renewed_same_account(Some(&old), None));
        assert!(!renewed_same_account(
            Some(&old),
            Some(&("43".into(), "new".into()))
        ));
        assert!(renewed_same_account(
            Some(&old),
            Some(&("42".into(), "new".into()))
        ));
        assert!(!renewed_same_account(
            None,
            Some(&("42".into(), "new".into()))
        ));
    }
}

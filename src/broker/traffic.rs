//! One authoritative monthly allowance for all transports and connections.
use super::Broker;
use crate::{
    portal::Problem,
    user_policy::{TrafficError, TrafficQuota, traffic_period_at, valid_traffic_utc_offset},
};
use std::sync::atomic::Ordering;

impl Broker {
    /// Called once at server startup, before accepting connections or payments.
    pub fn configure_traffic_timezone(&self, utc_offset_minutes: i32) -> Result<(), String> {
        if !valid_traffic_utc_offset(utc_offset_minutes) {
            return Err("traffic UTC offset must be between -720 and 840 minutes".into());
        }
        self.traffic_utc_offset_minutes
            .store(utc_offset_minutes, Ordering::Release);
        Ok(())
    }

    pub fn traffic_period(&self) -> String {
        traffic_period_at(
            crate::management::now(),
            self.traffic_utc_offset_minutes.load(Ordering::Acquire),
        )
    }

    pub(crate) async fn traffic_quota(&self, subject: &str) -> Result<TrafficQuota, Problem> {
        let unlimited = self
            .management
            .as_ref()
            .is_some_and(|state| state.policy.is_admin_subject(subject));
        let offset = self.traffic_utc_offset_minutes.load(Ordering::Acquire);
        let subject = subject.to_owned();
        self.portal
            .storage(move |store| {
                let period = traffic_period_at(crate::management::now(), offset);
                store.traffic_quota(&subject, &period, offset, unlimited)
            })
            .await
    }

    /// Only commerce's verified settlement path calls this; there is no public
    /// request field or transport frame that can grant bytes or administrator status.
    pub(crate) async fn grant_paid_traffic(
        &self,
        subject: &str,
        period: &str,
        order_id: &str,
        bytes: u64,
    ) -> Result<bool, Problem> {
        let subject = subject.to_owned();
        let period = period.to_owned();
        let order_id = order_id.to_owned();
        let offset = self.traffic_utc_offset_minutes.load(Ordering::Acquire);
        self.portal
            .storage(move |store| {
                let current = traffic_period_at(crate::management::now(), offset);
                store.grant_paid_traffic(&subject, &period, &current, &order_id, bytes)
            })
            .await
    }

    /// The rate limit and durable monthly allowance are independent. The small
    /// reservation is synced before the caller forwards a valid application frame.
    pub(crate) async fn charge_relay(
        &self,
        subject: &str,
        outbound: bool,
        bytes: usize,
    ) -> Result<(), TrafficError> {
        self.user_policies
            .get(subject)
            .charge(outbound, bytes)
            .await
            .map_err(|_| TrafficError::Banned)?;
        if !self.portal.has_storage() {
            // Explicit legacy deployments without account storage have no quota
            // product. In account mode an unowned session is rejected at attach.
            return Ok(());
        }
        let unlimited = self
            .management
            .as_ref()
            .is_some_and(|state| state.policy.is_admin_subject(subject));
        let subject = subject.to_owned();
        let offset = self.traffic_utc_offset_minutes.load(Ordering::Acquire);
        let bytes = u64::try_from(bytes).map_err(|_| TrafficError::Exhausted)?;
        self.portal
            .storage(move |store| {
                let period = traffic_period_at(crate::management::now(), offset);
                store.reserve_traffic_frame(
                    &subject,
                    &period,
                    offset,
                    crate::user_policy::TrafficFrame {
                        uploaded: outbound,
                        bytes,
                    },
                    unlimited,
                )
            })
            .await
            .map_err(|error| match error {
                Problem::TrafficQuotaExceeded | Problem::Limit => TrafficError::Exhausted,
                Problem::Banned => TrafficError::Banned,
                Problem::Unauthorized => TrafficError::AccountRequired,
                _ => TrafficError::Storage,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        history::{AuditConfig, AuditLog},
        identity::Identity,
        management::{ManagementPolicy, Scope},
        portal::Portal,
        user_policy::{DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES, FREE_MONTHLY_TRAFFIC_BYTES},
        wire::{self, Frame, Operation},
    };
    use std::{os::unix::fs::MetadataExt, path::PathBuf, sync::Arc};
    use tokio::io::BufReader;

    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn fixture() -> (Temporary, Arc<Broker>) {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let directory = Temporary(
            std::env::temp_dir().join(format!("ssmcp-traffic-{:x}", u128::from_ne_bytes(random))),
        );
        let mut broker = Broker::new("https://ssmcp.test".into());
        broker.portal = Portal::open(&directory.0).unwrap();
        broker
            .portal
            .storage(|store| {
                for (subject, login) in [("123", "alice"), ("456", "bob"), ("999", "maintainer")] {
                    store.login(&Identity {
                        provider: "github".into(),
                        subject: subject.into(),
                        login: login.into(),
                    })?;
                }
                Ok(())
            })
            .await
            .unwrap();
        broker.configure_management(
            ManagementPolicy::new(vec!["999".into()], Default::default()).unwrap(),
            Arc::new(AuditLog::open(&directory.0.join("audit"), AuditConfig::default()).unwrap()),
        );
        (directory, Arc::new(broker))
    }

    async fn use_until(broker: &Broker, subject: &str, remaining: u64) {
        let period = broker.traffic_period();
        let subject = subject.to_owned();
        broker
            .portal
            .storage(move |store| {
                // Seed ordinary allowance usage even when the broker exposes an
                // unlimited quota for a server-configured administrator.
                let available = store
                    .traffic_quota(&subject, &period, DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES, false)?
                    .remaining_bytes
                    .expect("ordinary account allowance is finite");
                let reserve = available
                    .checked_sub(remaining)
                    .expect("target allowance cannot increase");
                store.reserve_traffic(
                    &subject,
                    &period,
                    DEFAULT_TRAFFIC_UTC_OFFSET_MINUTES,
                    reserve,
                    false,
                )
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn quota_reservations_share_both_directions_connections_and_restart() {
        let (directory, broker) = fixture().await;
        use_until(&broker, "123", 200).await;
        let account_inode = std::fs::metadata(directory.0.join("accounts.json"))
            .unwrap()
            .ino();
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let mut calls = Vec::new();
        for index in 0..16 {
            let broker = broker.clone();
            let barrier = barrier.clone();
            calls.push(tokio::spawn(async move {
                barrier.wait().await;
                broker.charge_relay("123", index % 2 == 0, 25).await
            }));
        }
        barrier.wait().await;
        let mut accepted = 0;
        for call in calls {
            match call.await.unwrap() {
                Ok(()) => accepted += 1,
                Err(error) => assert_eq!(error, TrafficError::Exhausted),
            }
        }
        assert_eq!(accepted, 16);
        let quota = broker.traffic_quota("123").await.unwrap();
        assert_eq!(quota.used_bytes, FREE_MONTHLY_TRAFFIC_BYTES);
        assert_eq!(quota.remaining_bytes, Some(0));
        assert_eq!(
            broker.charge_relay("123", false, 1).await,
            Err(TrafficError::Exhausted)
        );
        assert_eq!(broker.traffic_quota("456").await.unwrap().used_bytes, 0);
        // Relay debit appends to the small ledger; it does not replace accounts.json.
        assert_eq!(
            account_inode,
            std::fs::metadata(directory.0.join("accounts.json"))
                .unwrap()
                .ino()
        );
        drop(broker);
        let mut reopened = Broker::new("https://ssmcp.test".into());
        reopened.portal = Portal::open(&directory.0).unwrap();
        assert_eq!(
            reopened.traffic_quota("123").await.unwrap().remaining_bytes,
            Some(0)
        );
        assert_eq!(
            reopened.charge_relay("123", true, 1).await,
            Err(TrafficError::Exhausted)
        );
    }

    #[tokio::test]
    async fn quota_administrator_is_only_the_server_policy_subject() {
        let (_directory, broker) = fixture().await;
        use_until(&broker, "999", 0).await;
        broker.charge_relay("999", true, 7).await.unwrap();
        let quota = broker.traffic_quota("999").await.unwrap();
        assert_eq!(quota.limit_bytes, None);
        assert_eq!(quota.remaining_bytes, None);
        assert_eq!(quota.used_bytes, FREE_MONTHLY_TRAFFIC_BYTES + 7);
        // A matching login string cannot turn another numeric identity into admin.
        let forged_name = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "maintainer".into(),
        };
        let snapshot = broker
            .management_snapshot(&forged_name, Scope::Own)
            .await
            .unwrap();
        assert!(!snapshot.is_admin);
        assert_eq!(
            snapshot.traffic.unwrap().limit_bytes,
            Some(FREE_MONTHLY_TRAFFIC_BYTES)
        );
        assert!(
            broker
                .management_snapshot(&forged_name, Scope::All)
                .await
                .is_err()
        );
        assert_eq!(
            broker.charge_relay("0999", false, 1).await,
            Err(TrafficError::AccountRequired)
        );
        let admin = Identity {
            provider: "github".into(),
            subject: "999".into(),
            login: "renamed-admin".into(),
        };
        assert!(
            broker
                .management_snapshot(&admin, Scope::Own)
                .await
                .unwrap()
                .traffic
                .unwrap()
                .limit_bytes
                .is_none()
        );
    }

    #[tokio::test]
    async fn quota_paid_grants_use_current_month_and_survive_retry() {
        let (directory, broker) = fixture().await;
        let period = broker.traffic_period();
        assert!(
            broker
                .grant_paid_traffic("123", &period, "order-one", 500)
                .await
                .unwrap()
        );
        assert!(
            !broker
                .grant_paid_traffic("123", &period, "order-one", 500)
                .await
                .unwrap()
        );
        assert_eq!(
            broker
                .grant_paid_traffic("456", &period, "order-one", 500)
                .await,
            Err(Problem::TrafficGrantConflict)
        );
        assert_eq!(
            broker
                .grant_paid_traffic("123", "2000-01", "late-order", 500)
                .await,
            Err(Problem::TrafficPeriodMismatch)
        );
        assert_eq!(
            broker.traffic_quota("123").await.unwrap().limit_bytes,
            Some(FREE_MONTHLY_TRAFFIC_BYTES + 500)
        );
        drop(broker);
        let mut reopened = Broker::new("https://ssmcp.test".into());
        reopened.portal = Portal::open(&directory.0).unwrap();
        assert!(
            !reopened
                .grant_paid_traffic("123", &period, "order-one", 500)
                .await
                .unwrap()
        );
    }

    fn hello() -> Frame {
        Frame::Hello {
            version: wire::VERSION,
            mode: wire::Mode::Shell,
            metadata: wire::ClientMetadata {
                name: "quota-test".into(),
                title: None,
                cwd: "/tmp".into(),
                hostname: "test".into(),
                allow: vec![],
            },
        }
    }

    #[tokio::test]
    async fn quota_account_mode_rejects_unowned_legacy_before_registration() {
        let (_directory, broker) = fixture().await;
        let (stream, mut peer) = tokio::io::duplex(8192);
        let server = tokio::spawn(broker.clone().attach_stream(stream, "old-ssh".into(), None));
        wire::write_frame(&mut peer, &hello()).await.unwrap();
        let mut peer = BufReader::new(peer);
        assert!(matches!(wire::read_frame(&mut peer).await.unwrap(),
            Some(Frame::Reject { message }) if message.contains("ssmcp client login")));
        server.await.unwrap().unwrap();
        assert!(broker.online_session("old-ssh").await.is_none());
    }

    #[tokio::test]
    async fn quota_legacy_gateway_rejects_call_before_forwarding() {
        let (_directory, broker) = fixture().await;
        use_until(&broker, "123", 0).await;
        let (stream, mut peer) = tokio::io::duplex(8192);
        let server = tokio::spawn(broker.clone().attach_stream(stream, "gh-123".into(), None));
        wire::write_frame(&mut peer, &hello()).await.unwrap();
        let mut peer = BufReader::new(peer);
        assert!(matches!(
            wire::read_frame(&mut peer).await.unwrap(),
            Some(Frame::Ready { .. })
        ));
        let session = broker.online_session("gh-123").await.unwrap();
        let call =
            tokio::spawn(async move { session.call(Operation::ListTools { cursor: None }).await });
        let message = match wire::read_frame(&mut peer).await.unwrap() {
            Some(Frame::Reject { message }) => message,
            _ => panic!("quota rejection must reach the client"),
        };
        assert!(message.starts_with("traffic_quota_exceeded:"));
        assert!(message.contains("https://ssmcp.test/account/traffic"));
        assert!(message.contains("未执行"));
        assert_eq!(call.await.unwrap().unwrap_err(), message);
        assert!(server.await.unwrap().is_err());
        assert_eq!(
            broker.traffic_quota("123").await.unwrap().remaining_bytes,
            Some(0)
        );
    }

    #[tokio::test]
    async fn quota_legacy_retains_pending_queued_and_disconnected_call_causes() {
        let (_directory, broker) = fixture().await;
        let (stream, mut peer) = tokio::io::duplex(8192);
        let server = tokio::spawn(broker.clone().attach_stream(stream, "gh-123".into(), None));
        wire::write_frame(&mut peer, &hello()).await.unwrap();
        let mut peer = BufReader::new(peer);
        assert!(matches!(
            wire::read_frame(&mut peer).await.unwrap(),
            Some(Frame::Ready { .. })
        ));
        let session = broker.online_session("gh-123").await.unwrap();
        let first_session = session.clone();
        let first = tokio::spawn(async move {
            first_session
                .call(Operation::ListTools { cursor: None })
                .await
        });
        assert!(matches!(
            wire::read_frame(&mut peer).await.unwrap(),
            Some(Frame::Call { .. })
        ));
        use_until(&broker, "123", 0).await;

        // Both are admitted without yielding. The first frame is rejected by
        // the transport, while the second remains in the broker's request queue.
        let (second_reply, second) = tokio::sync::oneshot::channel();
        let (third_reply, third) = tokio::sync::oneshot::channel();
        for reply in [second_reply, third_reply] {
            assert!(
                session
                    .sender
                    .try_send(super::super::Call {
                        operation: Operation::ListTools { cursor: None },
                        controller: None,
                        code_context: None,
                        authorization: None,
                        reply,
                    })
                    .is_ok()
            );
        }
        let terminal = match wire::read_frame(&mut peer).await.unwrap() {
            Some(Frame::Reject { message }) => message,
            _ => panic!("expected quota rejection"),
        };
        let pending = first.await.unwrap().unwrap_err();
        let blocked = second.await.unwrap().unwrap_err();
        let queued = third.await.unwrap().unwrap_err();
        for error in [&terminal, &pending, &blocked, &queued] {
            assert!(error.starts_with("traffic_quota_exceeded:"));
            assert!(error.contains("https://ssmcp.test/account/traffic"));
        }
        assert!(pending.contains("可能已经执行"));
        assert!(!pending.contains("未执行"));
        assert!(blocked.contains("未执行"));
        assert_eq!(blocked, terminal);
        assert_eq!(queued, blocked);
        assert!(server.await.unwrap().is_err());
        let after = session
            .call(Operation::ListTools { cursor: None })
            .await
            .unwrap_err();
        assert_eq!(after, queued);
    }

    #[tokio::test]
    async fn quota_legacy_lost_reply_keeps_uncertain_outcome_and_recharge_link() {
        let (_directory, broker) = fixture().await;
        let (stream, mut peer) = tokio::io::duplex(8192);
        let server = tokio::spawn(broker.clone().attach_stream(stream, "gh-123".into(), None));
        wire::write_frame(&mut peer, &hello()).await.unwrap();
        let mut peer = BufReader::new(peer);
        assert!(matches!(
            wire::read_frame(&mut peer).await.unwrap(),
            Some(Frame::Ready { .. })
        ));
        let session = broker.online_session("gh-123").await.unwrap();
        let call =
            tokio::spawn(async move { session.call(Operation::ListTools { cursor: None }).await });
        let id = match wire::read_frame(&mut peer).await.unwrap() {
            Some(Frame::Call { id, .. }) => id,
            _ => panic!("expected dispatched call"),
        };
        use_until(&broker, "123", 0).await;
        wire::write_frame(
            peer.get_mut(),
            &Frame::Reply {
                id,
                result: Ok(serde_json::json!({"marker": "reply must not reach the caller"})),
            },
        )
        .await
        .unwrap();
        let terminal = match wire::read_frame(&mut peer).await.unwrap() {
            Some(Frame::Reject { message }) => message,
            _ => panic!("expected quota rejection"),
        };
        let error = call.await.unwrap().unwrap_err();
        assert_eq!(error, terminal);
        assert!(error.starts_with("traffic_quota_exceeded:"));
        assert!(error.contains("https://ssmcp.test/account/traffic"));
        assert!(error.contains("客户端结果未能送达"));
        assert!(error.contains("可能已经执行"));
        assert!(!error.contains("未执行"));
        assert!(server.await.unwrap().is_err());
    }

    #[test]
    fn quota_timezone_is_explicit_and_validated() {
        let broker = Broker::new("https://ssmcp.test".into());
        assert!(broker.configure_traffic_timezone(840).is_ok());
        assert!(broker.configure_traffic_timezone(-720).is_ok());
        assert!(broker.configure_traffic_timezone(841).is_err());
        assert!(broker.configure_traffic_timezone(-721).is_err());
    }
}

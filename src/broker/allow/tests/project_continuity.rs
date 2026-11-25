use super::*;
use crate::wire::project_continuity::{CAPABILITY, OFFER_HEADER, Proof};
use ring::signature::Ed25519KeyPair;
use std::sync::atomic::Ordering;

async fn open(f: &Fixture, targets: &[&str]) -> (Socket, Arc<Session>) {
    let mut req = f.request(None, true);
    req.headers_mut().insert(OFFER_HEADER, "1".parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    send(&mut socket, legacy_self_hello(names(targets))).await;
    let Frame::Ready {
        client_id,
        capabilities,
        login_code,
        ..
    } = receive(&mut socket).await
    else {
        panic!("expected Ready")
    };
    assert!(capabilities.iter().any(|c| c == CAPABILITY));
    assert!(login_code.is_empty());
    let session = f
        .broker
        .client(IDENTITY_RESOURCE, &alice(), &client_id)
        .await
        .unwrap();
    assert!(session.owner_identity.is_none());
    assert!(session.info.routing.is_none());
    (socket, session)
}

async fn barrier(s: &mut Socket) {
    send(
        s,
        Frame::Access {
            id: 1,
            command: AccessCommand::List {},
        },
    )
    .await;
    assert!(matches!(
        receive(s).await,
        Frame::AccessReply {
            id: 1,
            result: Ok(_)
        }
    ));
}

async fn prove(s: &mut Socket, proof: Proof) {
    send(s, Frame::ProjectProof { proof }).await;
    barrier(s).await;
}

async fn disconnect(s: Socket, session: &Session) {
    close(s).await;
    timeout(Duration::from_secs(3), async {
        loop {
            let notified = session.closed_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if session.closed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
}

fn alice() -> Authorization {
    Authorization::Identity(identity("42", "alice"))
}
fn key() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap()
}
fn signed(session: &Session, previous: Option<&Session>) -> Proof {
    Proof::sign(
        &key(),
        "https://ssmcp.test",
        session.client_id(),
        previous.map(Session::client_id),
    )
    .unwrap()
}

#[tokio::test]
async fn project_continuity_round_trip_scopes_visibility_and_forgets_after_acl_edit() {
    let f = Fixture::new().await;
    let (mut s, a) = open(&f, &["alice", "colleague"]).await;
    prove(&mut s, signed(&a, None)).await;
    disconnect(s, &a).await;
    let (mut s, b) = open(&f, &["colleague", "alice"]).await;
    prove(&mut s, signed(&b, Some(&a))).await;
    assert_eq!(
        f.broker.project_previous(&b, &alice()).as_deref(),
        Some(a.client_id())
    );
    assert!(
        f.broker
            .project_previous(&b, &Authorization::Identity(identity("123", "alice")))
            .is_none()
    );
    assert!(
        f.broker
            .client(IDENTITY_RESOURCE, &alice(), a.client_id())
            .await
            .is_err()
    );
    send(
        &mut s,
        Frame::Access {
            id: 2,
            command: AccessCommand::Remove {
                principal: "colleague@ssmcp.test".into(),
            },
        },
    )
    .await;
    assert!(matches!(
        receive(&mut s).await,
        Frame::AccessReply { result: Ok(_), .. }
    ));
    assert!(f.broker.project_previous(&b, &alice()).is_none());
    // The consumed connection challenge cannot re-register after revocation.
    prove(&mut s, signed(&b, Some(&a))).await;
    disconnect(s, &b).await;
    let (mut s, c) = open(&f, &["alice"]).await;
    prove(&mut s, signed(&c, Some(&b))).await;
    assert!(f.broker.project_previous(&c, &alice()).is_none());
}

#[tokio::test]
async fn project_continuity_rejects_replay_wrong_endpoint_and_different_acl() {
    let f = Fixture::new().await;
    let (mut s, a) = open(&f, &["alice"]).await;
    let first = signed(&a, None);
    prove(&mut s, first.clone()).await;
    disconnect(s, &a).await;
    let (mut s, b) = open(&f, &["alice"]).await;
    prove(&mut s, first).await;
    prove(&mut s, signed(&b, Some(&a))).await;
    assert!(f.broker.project_previous(&b, &alice()).is_none());
    disconnect(s, &b).await;
    let (mut s, c) = open(&f, &["alice"]).await;
    let wrong = Proof::sign(
        &key(),
        "https://other.test",
        c.client_id(),
        Some(a.client_id()),
    )
    .unwrap();
    prove(&mut s, wrong).await;
    assert!(f.broker.project_previous(&c, &alice()).is_none());
    disconnect(s, &c).await;
    let (mut s, d) = open(&f, &["alice", "colleague"]).await;
    prove(&mut s, signed(&d, Some(&a))).await;
    assert!(f.broker.project_previous(&d, &alice()).is_none());
}

#[tokio::test]
async fn project_continuity_parallel_proofs_are_ambiguous_and_stop_invalidates_successor() {
    let f = Fixture::new().await;
    let (mut first, a) = open(&f, &["alice"]).await;
    prove(&mut first, signed(&a, None)).await;
    let (mut second, b) = open(&f, &["alice"]).await;
    prove(&mut second, signed(&b, Some(&a))).await;
    assert!(f.broker.project_previous(&b, &alice()).is_none());
    disconnect(first, &a).await;
    disconnect(second, &b).await;
    let (mut s, c) = open(&f, &["alice"]).await;
    prove(&mut s, signed(&c, Some(&b))).await;
    assert!(f.broker.project_previous(&c, &alice()).is_none());
    send(&mut s, Frame::ProjectForget {}).await;
    barrier(&mut s).await;
    disconnect(s, &c).await;
    let (mut s, d) = open(&f, &["alice"]).await;
    prove(&mut s, signed(&d, Some(&c))).await;
    assert!(f.broker.project_previous(&d, &alice()).is_none());
}

#[tokio::test]
async fn project_continuity_unoffered_connection_keeps_ordinary_registration() {
    let f = Fixture::new().await;
    let mut socket = f.socket(None, names(&["alice"])).await;
    let Frame::Ready {
        client_id,
        capabilities,
        ..
    } = receive(&mut socket).await
    else {
        panic!("expected ordinary Ready")
    };
    assert!(!capabilities.iter().any(|c| c == CAPABILITY));
    let session = f
        .broker
        .client(IDENTITY_RESOURCE, &alice(), &client_id)
        .await
        .unwrap();
    assert!(f.broker.project_previous(&session, &alice()).is_none());
    barrier(&mut socket).await;
}

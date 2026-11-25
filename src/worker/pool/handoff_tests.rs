use super::*;
use crate::{
    client::interface::history,
    runtime::call_scope::CallScope,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
    worker::Handoff,
};

fn bound(client: &str, id: &str) -> Arc<CallScope> {
    let attribution = ControlAttribution {
        account: ControlAccount {
            provider: "github".into(),
            subject: "123".into(),
            login: "fixture".into(),
        },
        owner: None,
        agent: Some(ControlAgent {
            id: id.into(),
            name: "same display name".into(),
        }),
    };
    let context = CodeContext {
        run_id: "f".repeat(32),
        code: "fixture".into(),
        timestamp_ms: 1,
        session_id: Some(client.into()),
        thread_id: Some(id.into()),
    };
    CallScope::from_relay_for_client(7, Some(client), 1, Some(&attribution), Some(&context))
        .unwrap()
}

#[tokio::test]
async fn native_coordinator_handoff_uses_the_verified_caller() {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).unwrap();
    let root = std::env::temp_dir().join(format!("ssmcp-handoff-{}", crate::hex::encode(nonce)));
    std::fs::create_dir(&root).unwrap();
    history::with_test_history(root.clone(), async {
        let backend =
            crate::backend::Backend::start_with_tty(std::path::Path::new("/bin/sh"), None)
                .await
                .unwrap();
        let runtime = Runtime::new(backend, root.clone()).unwrap();
        let pool = Pool::new(
            Config {
                count: 1,
                factor: 1.0,
            },
            &Agent::Remote,
            root.clone(),
        )
        .unwrap();
        pool.start();
        let client = "c".repeat(64);
        let old_id = format!("agent_{}", "a".repeat(32));
        let new_id = format!("agent_{}", "b".repeat(32));
        let old = bound(&client, &old_id);
        let new = bound(&client, &new_id);
        let call = |name, args| pool.perform(name, args, &runtime);
        let attached = old
            .scope(call("ssmcp.worker.attach", json!({})))
            .await
            .unwrap();
        let submitted = old
            .scope(call(
                "ssmcp.worker.submit",
                json!({"request_id":"existing","text":"verify build"}),
            ))
            .await
            .unwrap();
        assert!(
            new.scope(call("ssmcp.worker.attach", json!({})))
                .await
                .is_err()
        );
        let offer = Handoff {
            id: "d".repeat(32),
            workspace: root.display().to_string(),
            control: "9".repeat(64),
            pool: client,
            generation: pool.generation.clone(),
            predecessor: old_id,
            successor: new_id,
            attached_at_ms: attached["manager"]["attached_at_ms"].as_u64().unwrap(),
            note: "Build verification is pending.".into(),
        };
        pool.queue.offer_handoff(offer.clone()).unwrap();
        let args = json!({"handoff_id":offer.id});
        assert!(
            old.scope(call("ssmcp.worker.accept_handoff", args.clone()))
                .await
                .is_err()
        );
        assert!(
            new.scope(call(
                "ssmcp.worker.accept_handoff",
                json!({"handoff_id":offer.id,"actor":"forged"})
            ))
            .await
            .is_err()
        );
        let receipt = new
            .scope(call("ssmcp.worker.accept_handoff", args.clone()))
            .await
            .unwrap();
        assert_eq!(receipt["manager"]["thread_id"], offer.successor);
        assert_eq!(receipt["handoff"]["control"], offer.control);
        assert_eq!(receipt["handoff"]["workspace"], offer.workspace);
        let retry = new
            .scope(call("ssmcp.worker.accept_handoff", args))
            .await
            .unwrap();
        assert_eq!(retry["duplicate"], true);
        let listed = new
            .scope(call("ssmcp.worker.tasks", json!({})))
            .await
            .unwrap();
        assert_eq!(
            listed["page"]["tasks"][0]["task_id"],
            submitted["task"]["task_id"]
        );
        assert!(
            old.scope(call(
                "ssmcp.worker.submit",
                json!({"request_id":"late","text":"late"})
            ))
            .await
            .is_err()
        );
        let replay = new
            .scope(call(
                "ssmcp.worker.submit",
                json!({"request_id":"existing","text":"verify build"}),
            ))
            .await
            .unwrap();
        assert_eq!(replay["task"]["task_id"], submitted["task"]["task_id"]);
        assert_eq!(pool.queue.snapshot().queued, 1);
        pool.shutdown().await.unwrap();
    })
    .await;
    std::fs::remove_dir_all(root).unwrap();
}

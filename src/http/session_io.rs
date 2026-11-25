//! Request-correlated inputs and outputs at the authenticated response boundary.
use super::{Authorization, Broker, CodeRunReport, EligibleUserResponse, Session};
use crate::code_mode::RunBinding;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_INPUTS: usize = 8;
const HEADER: &str = "THREAD INPUTS — requests from an authenticated account CLI\nThese are addressed to this exact main or bound agent. For each input, use ssmcp.ui.reply with its request_id and input_id and a new output_<32 hex> identifier. progress/running is an update; result/done or result/error completes only that input after accepted:true. This does not grant permission to finish the agent's other work.\n";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeProposal {
    schema_version: u8,
    proposal: Value,
}

/// Called only for the exact native tool result, never the JavaScript return value.
pub(super) fn commit_reply(
    broker: &Broker,
    session: &Arc<Session>,
    user: &str,
    authorization: &Authorization,
    binding: Option<&RunBinding>,
    reply_revision: Option<u64>,
    native: Value,
) -> Result<Value, String> {
    if native["isError"] == true {
        return Ok(native);
    }
    if !session.io_reply_advertised() {
        return Err("this connection has not advertised native ssmcp.ui.reply; discover its tools or update the client".into());
    }
    let Authorization::Identity(actor) = authorization else {
        return Err("thread reply requires an authenticated account".into());
    };
    if !authorization.is_live() || !authorization.permits(session) || !session.is_alive() {
        return Err("thread reply access was revoked before acceptance".into());
    }
    let owner = crate::bridge::owner(user, authorization)?;
    if let Some(binding) = binding {
        if binding.owner != owner {
            return Err("thread reply binding belongs to a different account".into());
        }
        session
            .collaboration
            .bind(&owner, &binding.agent_id, &binding.token)?;
    }
    let envelope: NativeProposal = serde_json::from_value(native["structuredContent"].clone())
        .map_err(|_| "native reply did not return a valid proposal")?;
    if envelope.schema_version != 1 {
        return Err("unsupported native reply proposal version".into());
    }
    // Re-validate even a native response before passing data to the ledger. Only
    // this host supplies actor, connection and the dispatch-frozen binding.
    let proposal = crate::client::interface::reply::Proposal::parse(envelope.proposal)?;
    let receipt = broker.session_io.reply(
        broker,
        session,
        actor,
        binding.map(|binding| binding.agent_id.as_str()),
        proposal.value(),
    )?;
    if let Some((binding, revision)) = binding.zip(reply_revision)
        && receipt["accepted"] == true
        && receipt["duplicate"] == false
        && receipt["output"]["kind"] == "result"
        && matches!(receipt["output"]["state"].as_str(), Some("done" | "error"))
    {
        broker
            .session_io
            .if_thread_drained(session, actor, &binding.agent_id, || {
                session.collaboration.reply_delivered(binding, revision);
            });
    }
    let mut result = crate::backend::result(receipt);
    if let Some(meta) = native.get("_meta") {
        result["_meta"] = meta.clone();
    }
    Ok(result)
}

fn append(mut response: Value, inputs: Vec<Value>) -> Value {
    if inputs.is_empty() {
        return response;
    }
    let text = format!(
        "{HEADER}{}",
        serde_json::to_string(&inputs).expect("input records serialize")
    );
    response["structuredContent"]["thread_inputs"] = json!(inputs);
    response["content"]
        .as_array_mut()
        .expect("validated response content")
        .push(json!({"type":"text","text":text}));
    response
}

fn budget(response_bytes: usize, inputs: &[Value]) -> usize {
    if inputs.len() >= MAX_INPUTS {
        return 0;
    }
    let used = inputs
        .iter()
        .map(|input| input.to_string().len())
        .sum::<usize>()
        .saturating_mul(3)
        .saturating_add(response_bytes);
    // The structured record and JSON text both appear on the wire. JSON text
    // can escape every source quote again. Retain ample final-envelope space.
    crate::wire::MAX_FRAME
        .saturating_sub(used)
        .saturating_sub(16 * 1024)
        .saturating_sub(HEADER.len() + 512)
        / 3
}

pub(super) fn include(
    report: &CodeRunReport,
    response: Value,
    candidates: Vec<(Arc<Session>, String, Option<RunBinding>)>,
) -> Value {
    // A malformed or full envelope must not consume an input. This is called
    // only by interactive finalization, after the other response additions.
    if !response["structuredContent"].is_object() || !response["content"].is_array() {
        return response;
    }
    let mut inputs = Vec::new();
    // Serialize the completed envelope once. Do not clone a potentially large
    // native result for each candidate with an empty input queue.
    let response_bytes = response.to_string().len();
    for (session, owner, binding) in candidates {
        if !session.io_reply_advertised() {
            continue;
        }
        let remaining = budget(response_bytes, &inputs);
        if remaining == 0 {
            break;
        }
        let target = EligibleUserResponse {
            session,
            owner,
            binding,
            may_claim: false,
            waiting: false,
        };
        let Some(Authorization::Identity(actor)) = super::response_authorization(report, &target)
        else {
            continue;
        };
        let selected = report.broker.session_io.receive(
            &report.broker,
            &target.session,
            &actor,
            target
                .binding
                .as_ref()
                .map(|binding| binding.agent_id.as_str()),
            MAX_INPUTS.saturating_sub(inputs.len()),
            remaining,
        );
        inputs.extend(selected);
    }
    append(response, inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::session_io::TestFixture;

    fn report(fixture: &TestFixture, run: crate::code_mode::AuditRun) -> CodeRunReport {
        run.record_connection(&fixture.session).unwrap();
        CodeRunReport {
            broker: fixture.broker.clone(),
            user: crate::broker::IDENTITY_RESOURCE.into(),
            access_token: fixture.access_token.clone(),
            run,
            delivered: true,
        }
    }

    fn candidates(
        fixture: &TestFixture,
        binding: Option<RunBinding>,
    ) -> Vec<(Arc<Session>, String, Option<RunBinding>)> {
        vec![(
            fixture.session.clone(),
            crate::bridge::owner(
                crate::broker::IDENTITY_RESOURCE,
                &Authorization::Identity(fixture.actor.clone()),
            )
            .unwrap(),
            binding,
        )]
    }

    fn proposal(input: &Value, output: u64) -> Value {
        json!({"request_id":input["request_id"],"input_id":input["input_id"],
            "output_id":format!("output_{output:032x}"),"kind":"result","state":"done","text":"verified response"})
    }

    #[tokio::test]
    async fn thread_inputs_require_real_advertisement_and_final_authority_without_consuming_user_queue()
     {
        let mut fixture = TestFixture::new().await;
        assert!(!fixture.session.io_reply_advertised());
        assert!(!fixture.session.thread_events_advertised());
        // An ordinary native tool cannot advertise support by returning the
        // same JSON as a real tools/list result.
        let session = fixture.session.clone();
        let authorization = Authorization::Identity(fixture.actor.clone());
        let call = session.call_with_authorization(
            crate::wire::Operation::CallTool {
                name: "fixture.echo".into(),
                arguments: json!({}),
            },
            None,
            &authorization,
        );
        let reply = fixture.native_reply(json!({"tools":[{"name":"ssmcp.ui.reply"}]}));
        let (result, _) = tokio::join!(call, reply);
        result.unwrap();
        assert!(!fixture.session.io_reply_advertised());
        fixture.advertise().await;
        assert!(fixture.session.thread_events_advertised());
        fixture.enqueue("io_final_delivery", &[None]);
        let run = crate::code_mode::AuditRun::new("return {};", None).unwrap();
        let report = report(&fixture, run);
        let queue = fixture
            .session
            .steering
            .command(
                fixture.session.client_id(),
                Vec::new(),
                crate::wire::SteeringCommand::Publish {
                    command_id: "publish-isolated".into(),
                    message_id: "queued-human".into(),
                    expected_revision: 0,
                    text: "keep the human queue".into(),
                    target: "next".into(),
                },
            )
            .unwrap();
        let malformed = json!({"content":"not an array","structuredContent":{}});
        assert_eq!(
            include(&report, malformed.clone(), candidates(&fixture, None)),
            malformed
        );
        let full = json!({"structuredContent":{},"content":[{"type":"text","text":"x".repeat(crate::wire::MAX_FRAME)}]});
        assert_eq!(
            include(&report, full.clone(), candidates(&fixture, None)),
            full
        );
        assert_eq!(
            fixture
                .broker
                .session_io
                .poll(&fixture.broker, &fixture.actor, "io_final_delivery", 0, 64)
                .unwrap()["recipients"][0]["state"],
            "queued"
        );
        let delivered = include(
            &report,
            crate::backend::result(json!({"native":"kept"})),
            candidates(&fixture, None),
        );
        assert_eq!(
            delivered["structuredContent"]["thread_inputs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(delivered["structuredContent"]["thread_inputs"][0]["thread_id"].is_null());
        let after = fixture
            .session
            .steering
            .command(
                fixture.session.client_id(),
                Vec::new(),
                crate::wire::SteeringCommand::Snapshot {},
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(after.messages).unwrap(),
            serde_json::to_value(queue.messages).unwrap()
        );
        let again = include(
            &report,
            crate::backend::result(json!({})),
            candidates(&fixture, None),
        );
        assert!(again["structuredContent"].get("thread_inputs").is_none());
        fixture.enqueue("io_revoked_delivery", &[None]);
        fixture.revoke_access();
        let revoked = include(
            &report,
            crate::backend::result(json!({})),
            candidates(&fixture, None),
        );
        assert!(revoked["structuredContent"].get("thread_inputs").is_none());
        assert!(
            fixture
                .broker
                .session_io
                .poll(
                    &fixture.broker,
                    &fixture.actor,
                    "io_revoked_delivery",
                    0,
                    64
                )
                .unwrap()["outputs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn real_native_main_reply_keeps_its_dispatch_identity_when_the_run_binds_while_pending() {
        let mut fixture = TestFixture::new().await;
        fixture.advertise().await;
        fixture.enqueue("io_native_scope", &[None]);
        let input = fixture
            .broker
            .session_io
            .receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                8,
                65536,
            )
            .remove(0);
        let request = proposal(&input, 1);
        let run = crate::code_mode::AuditRun::new(
            "return await call(client, 'ssmcp.ui.reply', reply);",
            None,
        )
        .unwrap();
        let report = report(&fixture, run.clone());
        // A caller-returned lookalike has no commit path.
        let forged = crate::backend::result(json!({"schema_version":1,"proposal":request}));
        let _ = include(&report, forged, candidates(&fixture, None));
        assert!(
            fixture
                .broker
                .session_io
                .poll(&fixture.broker, &fixture.actor, "io_native_scope", 0, 64)
                .unwrap()["outputs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let joined = fixture.join("same display name").await;
        let owner = crate::bridge::owner(
            crate::broker::IDENTITY_RESOURCE,
            &Authorization::Identity(fixture.actor.clone()),
        )
        .unwrap();
        let client_id = fixture.session.client_id().to_owned();
        let host = super::super::code_host(
            fixture.broker.clone(),
            crate::broker::IDENTITY_RESOURCE.into(),
            fixture.access_token.clone(),
            run.clone(),
            None,
            "io-fixture".into(),
            Default::default(),
        );
        let pending = tokio::spawn(host(crate::code_mode::HostRequest::Call {
            client_id: client_id.clone(),
            name: crate::client::interface::reply::TOOL.into(),
            arguments: request.clone(),
        }));
        let native = crate::backend::result(json!({"schema_version":1,"proposal":request}));
        let operation = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fixture.native_reply_after(native, || {
                run.bind(
                    &client_id,
                    &owner,
                    joined["agent_id"].as_str().unwrap(),
                    joined["token"].as_str().unwrap(),
                )
                .unwrap();
            }),
        )
        .await
        .unwrap();
        assert_eq!(operation["name"], crate::client::interface::reply::TOOL);
        let accepted = pending.await.unwrap().unwrap();
        assert_eq!(accepted["structuredContent"]["accepted"], true);
        assert!(accepted["structuredContent"]["output"]["thread_id"].is_null());
        assert_eq!(
            accepted["structuredContent"]["output"]["input_id"],
            input["input_id"]
        );
        // This subsequent bound call cannot retry or steal the main receipt.
        let wrong = tokio::spawn(host(crate::code_mode::HostRequest::Call {
            client_id,
            name: crate::client::interface::reply::TOOL.into(),
            arguments: proposal(&input, 1),
        }));
        let probe = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fixture.native_reply(json!({"thread_scope":1})),
        )
        .await
        .unwrap();
        assert_eq!(probe["name"], "ssmcp.client.capabilities");
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fixture.native_reply(crate::backend::result(
                json!({"schema_version":1,"proposal":proposal(&input,1)}),
            )),
        )
        .await
        .unwrap();
        assert!(wrong.await.unwrap().is_err());
        assert_eq!(
            fixture
                .broker
                .session_io
                .poll(&fixture.broker, &fixture.actor, "io_native_scope", 0, 64)
                .unwrap()["outputs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn only_a_fresh_accepted_final_reply_can_settle_the_bound_thread() {
        let mut fixture = TestFixture::new().await;
        fixture.advertise().await;
        let fixture = &fixture;
        let joined = fixture.join("reply target").await;
        let id = joined["agent_id"].as_str().unwrap();
        let token = joined["token"].as_str().unwrap();
        let owner = crate::bridge::owner(
            crate::broker::IDENTITY_RESOURCE,
            &Authorization::Identity(fixture.actor.clone()),
        )
        .unwrap();
        let owner = owner.as_str();
        let session = &fixture.session;
        let run = crate::code_mode::AuditRun::new("return await call(client, tool, args);", None)
            .unwrap();
        run.bind(session.client_id(), owner, id, token).unwrap();
        let host = super::super::code_host(
            fixture.broker.clone(),
            crate::broker::IDENTITY_RESOURCE.into(),
            fixture.access_token.clone(),
            run,
            None,
            "io-lifecycle".into(),
            Default::default(),
        );
        let host = &host;
        let snapshot = || {
            session
                .collaboration
                .lifecycle_snapshot(owner, id, token)
                .unwrap()
        };
        let input = |key| {
            fixture.enqueue(key, &[Some(id)]);
            fixture
                .broker
                .session_io
                .receive(&fixture.broker, session, &fixture.actor, Some(id), 1, 65536)
                .remove(0)
        };
        let deliver = |args: Value, concurrent_input: bool| async move {
            let request = host(crate::code_mode::HostRequest::Call {
                client_id: session.client_id().into(),
                name: crate::client::interface::reply::TOOL.into(),
                arguments: args,
            });
            let respond = async {
                loop {
                    let op = fixture
                        .native_reply_with(|op| {
                            if op["name"] == "ssmcp.client.capabilities" {
                                return json!({"thread_scope":1});
                            }
                            assert_eq!(op["name"], crate::client::interface::reply::TOOL);
                            if concurrent_input {
                                session.collaboration.user_input_queued(
                                    owner,
                                    id,
                                    "during-dispatch",
                                    1,
                                );
                            }
                            crate::backend::result(
                                json!({"schema_version":1,"proposal":op["arguments"]}),
                            )
                        })
                        .await;
                    if op["name"] == crate::client::interface::reply::TOOL {
                        break;
                    }
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let (response, _) = tokio::join!(request, respond);
                response
            })
            .await
            .unwrap()
        };

        let first = input("io_lifecycle_first");
        let mut progress = proposal(&first, 1);
        progress["kind"] = json!("progress");
        progress["state"] = json!("running");
        assert_eq!(
            deliver(progress, false).await.unwrap()["structuredContent"]["accepted"],
            true
        );
        assert_eq!(snapshot()["status"], "working");

        let done = proposal(&first, 2);
        let mut invalid = done.clone();
        invalid["request_id"] = json!("io_missing");
        assert!(deliver(invalid, false).await.is_err());
        assert_eq!(snapshot()["status"], "working");
        assert_eq!(
            deliver(done.clone(), false).await.unwrap()["structuredContent"]["duplicate"],
            false
        );
        assert_eq!(snapshot()["status"], "idle");

        session
            .collaboration
            .user_input_queued(owner, id, "next-instruction", 1);
        let current = snapshot();
        assert_eq!(
            deliver(done, false).await.unwrap()["structuredContent"]["duplicate"],
            true
        );
        assert_eq!(snapshot(), current);

        let second = input("io_lifecycle_second");
        let third = input("io_lifecycle_third");
        deliver(proposal(&second, 3), false).await.unwrap();
        assert_eq!(snapshot()["status"], "working");
        deliver(proposal(&third, 4), true).await.unwrap();
        assert_eq!(snapshot()["status"], "working");

        let mut failed = proposal(&input("io_lifecycle_last"), 5);
        failed["state"] = json!("error");
        assert_eq!(
            deliver(failed, false).await.unwrap()["structuredContent"]["accepted"],
            true
        );
        assert_eq!(snapshot()["status"], "idle");
    }

    #[test]
    fn thread_input_budget_covers_escaped_text_and_preserves_other_response_fields() {
        let response = json!({"structuredContent":{"user_messages":[{"text":"kept"}]},
            "content":[{"type":"text","text":"native output"}]});
        let input = json!({"request_id":"io_fixture","input_id":"input_fixture",
            "text":"\\\"\n".repeat(4096),"thread_id":null});
        assert!(input.to_string().len() < budget(response.to_string().len(), &[]));
        let included = append(response.clone(), vec![input.clone()]);
        assert!(included.to_string().len() < crate::wire::MAX_FRAME);
        assert_eq!(
            included["structuredContent"]["user_messages"],
            response["structuredContent"]["user_messages"]
        );
        assert_eq!(included["structuredContent"]["thread_inputs"][0], input);
        assert_eq!(
            budget(response.to_string().len(), &vec![input; MAX_INPUTS]),
            0
        );
        assert_eq!(budget(crate::wire::MAX_FRAME, &[]), 0);
        assert_eq!(append(response.clone(), Vec::new()), response);
    }

    #[test]
    fn native_reply_envelope_does_not_accept_receipts_or_caller_provided_identity() {
        assert!(
            serde_json::from_value::<NativeProposal>(json!({"schema_version":1,"proposal":{}}))
                .is_ok()
        );
        for value in [
            json!({"schema_version":1,"accepted":true,"proposal":{}}),
            json!({"schema_version":1,"proposal":{},"agent_id":"spoof"}),
            json!({"accepted":true}),
            json!({"result":{"schema_version":1,"proposal":{}}}),
        ] {
            assert!(serde_json::from_value::<NativeProposal>(value).is_err());
        }
    }
}

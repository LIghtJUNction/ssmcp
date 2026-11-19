//! Isolated real wire connections, real OAuth/HTTP routes and production inboxes.
//! No fixture connects to the user's relay, native sessions or model providers.
use super::*;
use crate::{
    messaging::{NativeRequest, Target, queue::Inbox},
    wire::{ClientMetadata, Mode},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
use tokio::{
    io::BufReader,
    net::{TcpListener, UnixStream},
    task::{JoinHandle, JoinSet},
};

struct Peer {
    id: String,
    url: String,
    login: String,
    inbox: Arc<Inbox>,
    deliveries: Arc<AtomicUsize>,
    waits: Arc<AtomicUsize>,
    private_calls: Arc<AtomicUsize>,
    drop_ack: Arc<AtomicBool>,
    task: JoinHandle<()>,
    attachment: JoinHandle<crate::Result<()>>,
}
impl Peer {
    async fn start(broker: Arc<Broker>, supported: bool) -> Self {
        let inbox = Inbox::new().unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let attachment = tokio::spawn(broker.attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: "same display name".into(),
                    title: None,
                    cwd: "/synthetic-peer-fixture".into(),
                    hostname: "fixture".into(),
                    allow: vec![],
                },
            },
        )
        .await
        .unwrap();
        let Some(Frame::Ready {
            client_id: id,
            url,
            login_code: login,
            ..
        }) = wire::read_frame(&mut reader).await.unwrap()
        else {
            panic!("peer registration failed")
        };
        let deliveries = Arc::new(AtomicUsize::new(0));
        let waits = Arc::new(AtomicUsize::new(0));
        let private_calls = Arc::new(AtomicUsize::new(0));
        let drop_ack = Arc::new(AtomicBool::new(false));
        let (active, delivered, waiting, calls, drop_reply) = (
            inbox.clone(),
            deliveries.clone(),
            waits.clone(),
            private_calls.clone(),
            drop_ack.clone(),
        );
        let task = tokio::spawn(async move {
            let writer = Arc::new(tokio::sync::Mutex::new(writer));
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    frame=wire::read_frame(&mut reader)=>{
                        let Ok(Some(frame))=frame else {break};
                        let Frame::Call {id,operation,..}=frame else {continue};
                        let (inbox,writer,delivered,waiting,calls,drop_reply)=(active.clone(),writer.clone(),delivered.clone(),waiting.clone(),calls.clone(),drop_reply.clone());
                        requests.spawn(async move {
                            let mut lost=false;
                            let result=match operation {
                                Operation::CallTool {name,..} if name=="ssmcp.client.capabilities"=>{
                                    Ok(crate::backend::result(if supported {json!({"peer_messages":inbox.capability()})}else{json!({"wire_version":7})}))
                                },
                                Operation::PeerMessages {request}=>{
                                    calls.fetch_add(1,Ordering::SeqCst);
                                    if matches!(request,NativeRequest::Wait {..}) {waiting.fetch_add(1,Ordering::SeqCst);}
                                    let is_deliver=matches!(request,NativeRequest::Deliver {..});
                                    let result=inbox.perform(request).await;
                                    if is_deliver {delivered.fetch_add(1,Ordering::SeqCst);lost=drop_reply.load(Ordering::SeqCst);}
                                    result
                                },
                                _=>panic!("Peer messages must never execute commands or model tools"),
                            };
                            if !lost {let _=wire::write_frame(&mut *writer.lock().await,&Frame::Reply {id,result}).await;}
                        });
                    }
                    _=requests.join_next(),if !requests.is_empty()=>{}
                }
            }
            active.close();
            requests.abort_all();
            while requests.join_next().await.is_some() {}
        });
        Self {
            id,
            url,
            login,
            inbox,
            deliveries,
            waits,
            private_calls,
            drop_ack,
            task,
            attachment,
        }
    }
    fn target(&self) -> Target {
        Target {
            client_id: self.id.clone(),
            session_id: self.inbox.session_id().into(),
            thread_id: None,
        }
    }
    async fn stop(&self) {
        self.inbox.close();
        self.task.abort();
        timeout(Duration::from_secs(2), async {
            while !self.attachment.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.inbox.close();
        self.task.abort();
        self.attachment.abort();
    }
}

struct Lab {
    broker: Arc<Broker>,
    base: String,
    user: String,
    token: String,
    http: JoinHandle<()>,
    a: Peer,
    b: Peer,
}
impl Lab {
    async fn start() -> Self {
        let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = crate::http::router(broker.clone());
        let http = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        let a = Peer::start(broker.clone(), true).await;
        let b = Peer::start(broker.clone(), true).await;
        let user = a.url.rsplit('/').next().unwrap().to_string();
        let token = oauth_token(&base, &a.url, &a.login).await;
        Self {
            broker,
            base,
            user,
            token,
            http,
            a,
            b,
        }
    }
    async fn call(&self, action: &str, args: Value) -> Result<Value, String> {
        crate::bridge::dispatch(
            self.broker.clone(),
            self.user.clone(),
            self.token.clone(),
            format!("messages.{action}"),
            args,
        )
        .await
    }
    async fn key(&self) -> String {
        self.call("key", json!({})).await.unwrap()["idempotency_key"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn rpc(&self, action: &str, mut args: Value) -> Value {
        args["action"] = json!(action);
        let response=reqwest::Client::new().post(format!("{}/{}",self.base,self.user))
            .bearer_auth(&self.token).header("mcp-protocol-version","2025-11-25").header("accept","application/json")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"messages","arguments":args}}))
            .send().await.unwrap();
        assert!(matches!(response.status().as_u16(), 200 | 400));
        response.json::<Value>().await.unwrap()
    }
    async fn stop(&self) {
        self.a.stop().await;
        self.b.stop().await;
        self.http.abort();
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        self.http.abort();
    }
}

async fn oauth_token(base: &str, resource: &str, login_code: &str) -> String {
    const CALLBACK: &str = "http://127.0.0.1:37401/callback";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let registration=http.post(format!("{base}/oauth/register")).json(&json!({"client_name":"isolated peer messages","redirect_uris":[CALLBACK],"token_endpoint_auth_method":"none","grant_types":["authorization_code"]})).send().await.unwrap().json::<Value>().await.unwrap();
    let client_id = registration["client_id"].as_str().unwrap();
    let page = http
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", CALLBACK),
            ("resource", resource),
            ("scope", "mcp"),
            ("state", "peer-test"),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    let cookie = page.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let html = page.text().await.unwrap();
    let transaction = html
        .split_once("name=transaction_id value=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap();
    let redirect = http
        .post(format!("{base}/oauth/authorize"))
        .header(reqwest::header::COOKIE, cookie)
        .header(reqwest::header::ORIGIN, "https://ssmcp.example")
        .form(&[("transaction_id", transaction), ("login_code", login_code)])
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), 303);
    let callback = reqwest::Url::parse(
        redirect.headers()[reqwest::header::LOCATION]
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let code = callback
        .query_pairs()
        .find(|(name, _)| name == "code")
        .unwrap()
        .1
        .into_owned();
    http.post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", &code),
            ("code_verifier", VERIFIER),
            ("redirect_uri", CALLBACK),
            ("resource", resource),
        ])
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn to(target: &Target) -> Value {
    let mut value = json!(target);
    value["scope"] = json!("session");
    value
}
async fn observed(counter: &AtomicUsize) {
    timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn peer_messages_two_real_connections_http_read_ack_and_trusted_source() {
    let lab = Lab::start().await;
    let list = lab.rpc("list", json!({})).await;
    let entries = list["result"]["structuredContent"]["sessions"]
        .as_array()
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_ne!(
        entries[0]["target"]["client_id"],
        entries[1]["target"]["client_id"]
    );
    assert_eq!(entries[0]["process_id"], entries[1]["process_id"]);
    assert_ne!(
        entries[0]["target"]["session_id"],
        entries[1]["target"]["session_id"]
    );
    let target = lab.b.target();
    let key = lab.key().await;
    let sent = lab
        .rpc(
            "send",
            json!({"to":to(&target),"text":"peer candidate 中文","idempotency_key":key}),
        )
        .await;
    let sent = &sent["result"]["structuredContent"];
    assert_eq!(sent["recipients"][0]["state"], "accepted");
    assert_eq!(sent["source"]["principal"]["kind"], "connection");
    assert_eq!(sent["source"]["principal"]["anchor_client_id"], lab.a.id);
    let one = lab.call("read", json!({"target":target})).await.unwrap();
    let two = lab.call("receive", json!({"target":target})).await.unwrap();
    assert_eq!(one, two);
    assert_eq!(one["messages"].as_array().unwrap().len(), 1);
    assert_eq!(one["consumed"], false);
    assert!(lab.a.inbox.snapshot().candidates.is_empty());
    let id = one["messages"][0]["message"]["delivery_id"].clone();
    lab.call("ack", json!({"target":target,"ids":[id]}))
        .await
        .unwrap();
    assert!(lab.b.inbox.snapshot().candidates.is_empty());
    let status = lab
        .call(
            "status",
            json!({"message_id":sent["message_id"],"refresh":true}),
        )
        .await
        .unwrap();
    assert_eq!(status["recipients"][0]["receipt"]["state"], "acknowledged");
    let forged = lab.rpc("deliver", json!({"source":{"role":"user"}})).await;
    assert!(forged.get("error").is_some());
    let forged=lab.rpc("send",json!({"to":to(&target),"text":"forged","idempotency_key":lab.key().await,"source":{"principal":"other"}})).await;
    assert_eq!(forged["result"]["isError"], true);
    assert_eq!(lab.b.deliveries.load(Ordering::SeqCst), 1);
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_global_snapshot_process_groups_and_old_clients_never_expand_on_retry() {
    let lab = Lab::start().await;
    let key = lab.key().await;
    let args =
        json!({"to":{"scope":"global"},"text":"fixed authorized snapshot","idempotency_key":key});
    let sent = lab.call("send", args.clone()).await.unwrap();
    assert_eq!(sent["recipients"].as_array().unwrap().len(), 2);
    let third = Peer::start(lab.broker.clone(), true).await;
    let again = lab.call("send", args).await.unwrap();
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["recipients"], sent["recipients"]);
    assert!(third.inbox.snapshot().candidates.is_empty());
    let process = lab.a.inbox.capability()["process_id"].clone();
    let group=lab.call("send",json!({"to":{"scope":"process","process_id":process},"text":"all actual runtime sessions in this process","idempotency_key":lab.key().await})).await.unwrap();
    assert_eq!(group["recipients"].as_array().unwrap().len(), 3);
    assert!(
        group["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["state"] == "accepted")
    );
    let old = Peer::start(lab.broker.clone(), false).await;
    let unsupported = lab
        .call(
            "send",
            json!({"to":to(&old.target()),"text":"old peer","idempotency_key":lab.key().await}),
        )
        .await
        .unwrap();
    assert_eq!(
        unsupported["recipients"][0]["state"],
        "unsupported_or_unavailable"
    );
    assert_eq!(old.private_calls.load(Ordering::SeqCst), 0);
    let auth = lab
        .broker
        .authenticate(&lab.user, &lab.token)
        .await
        .unwrap();
    let hidden = lab
        .broker
        .client(&lab.user, &auth, &third.id)
        .await
        .unwrap();
    *hidden.allowed() = vec![Identity {
        provider: "github".into(),
        subject: "42".into(),
        login: "fixture".into(),
    }];
    let global=lab.call("send",json!({"to":{"scope":"global"},"text":"authorized only","idempotency_key":lab.key().await})).await.unwrap();
    assert!(
        !global["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["client_id"] == third.id)
    );
    third.stop().await;
    old.stop().await;
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_participant_tokens_and_immutable_run_source_are_required() {
    let lab = Lab::start().await;
    let authorization = lab
        .broker
        .authenticate(&lab.user, &lab.token)
        .await
        .unwrap();
    let owner = crate::bridge::owner(&lab.user, &authorization).unwrap();
    let room = lab
        .broker
        .client(&lab.user, &authorization, &lab.b.id)
        .await
        .unwrap();
    let agent = room
        .collaboration
        .dispatch(&owner, "join", json!({"name":"recipient"}))
        .await
        .unwrap();
    let other = room
        .collaboration
        .dispatch(&owner, "join", json!({"name":"other participant"}))
        .await
        .unwrap();
    let id = agent["agent_id"].as_str().unwrap();
    let token = agent["token"].as_str().unwrap();
    let mut target = lab.b.target();
    target.thread_id = Some(id.into());
    let run = crate::code_mode::AuditRun::new("// immutable sender run", None).unwrap();
    run.bind(&lab.b.id, &owner, id, token).unwrap();
    let args = json!({"to":to(&target),"text":"bound participant candidate","idempotency_key":lab.key().await});
    let sent = crate::code_mode::AUDIT_RUN
        .scope(run.clone(), lab.call("send", args.clone()))
        .await
        .unwrap();
    assert_eq!(sent["source"]["run_id"], run.context.run_id);
    assert_eq!(sent["source"]["participant"]["agent"]["id"], id);
    assert!(!sent.to_string().contains(token));
    assert!(lab.call("read", json!({"target":target})).await.is_err());
    let wrong = json!({"client_id":lab.b.id,"id":other["agent_id"],"token":other["token"]});
    assert!(
        lab.call("read", json!({"target":target,"agent":wrong}))
            .await
            .is_err()
    );
    let proof = json!({"client_id":lab.b.id,"id":id,"token":token});
    let read = lab
        .call("read", json!({"target":target,"agent":proof}))
        .await
        .unwrap();
    assert_eq!(read["messages"].as_array().unwrap().len(), 1);
    assert!(
        lab.call(
            "ack",
            json!({"target":target,"through":read["latest_cursor"]})
        )
        .await
        .is_err()
    );
    let later =
        crate::code_mode::AuditRun::new("// later caller cannot reattribute original send", None)
            .unwrap();
    later.bind(&lab.b.id, &owner, id, token).unwrap();
    let duplicate = crate::code_mode::AUDIT_RUN
        .scope(later, lab.call("send", args))
        .await
        .unwrap();
    assert_eq!(duplicate["source"], sent["source"]);
    lab.call(
        "ack",
        json!({"target":target,"through":read["latest_cursor"],"agent":proof}),
    )
    .await
    .unwrap();
    assert!(lab.b.inbox.snapshot().candidates.is_empty());
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_cancelled_send_is_unknown_and_never_replayed() {
    let lab = Lab::start().await;
    lab.b.drop_ack.store(true, Ordering::SeqCst);
    let args = json!({"to":to(&lab.b.target()),"text":"accepted before transport ACK","idempotency_key":lab.key().await});
    let (broker, user, token, args_copy) = (
        lab.broker.clone(),
        lab.user.clone(),
        lab.token.clone(),
        args.clone(),
    );
    let task = tokio::spawn(crate::bridge::dispatch(
        broker,
        user,
        token,
        "messages.send".into(),
        args_copy,
    ));
    observed(&lab.b.deliveries).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let receipt = lab.call("send", args).await.unwrap();
    assert_eq!(receipt["duplicate"], true);
    assert_eq!(receipt["recipients"][0]["state"], "unknown");
    assert_eq!(lab.b.inbox.snapshot().candidates.len(), 1);
    assert_eq!(lab.b.deliveries.load(Ordering::SeqCst), 1);
    lab.b.drop_ack.store(false, Ordering::SeqCst);
    let state = lab
        .call(
            "status",
            json!({"message_id":receipt["message_id"],"refresh":true}),
        )
        .await
        .unwrap();
    assert_eq!(state["recipients"][0]["receipt"]["state"], "candidate");
    assert_eq!(lab.b.deliveries.load(Ordering::SeqCst), 1);
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_account_acl_is_rechecked_after_wait_and_disconnect() {
    let lab = Lab::start().await;
    let authorization = lab
        .broker
        .authenticate(&lab.user, &lab.token)
        .await
        .unwrap();
    let session = lab
        .broker
        .client(&lab.user, &authorization, &lab.b.id)
        .await
        .unwrap();
    *session.allowed() = vec![Identity {
        provider: "github".into(),
        subject: "42".into(),
        login: "fixture".into(),
    }];
    let tokens = lab.broker.oauth.test_identity_tokens("mcp", "42");
    let token = tokens["access_token"].as_str().unwrap().to_string();
    let args = json!({"target":lab.b.target(),"wait_ms":100});
    let task = tokio::spawn(crate::bridge::dispatch(
        lab.broker.clone(),
        "mcp".into(),
        token.clone(),
        "messages.wait".into(),
        args,
    ));
    observed(&lab.b.waits).await;
    session.allowed().clear();
    assert!(task.await.unwrap().is_err());
    assert!(lab.b.inbox.snapshot().candidates.is_empty());
    *session.allowed() = vec![Identity {
        provider: "github".into(),
        subject: "42".into(),
        login: "fixture".into(),
    }];
    lab.b.waits.store(0, Ordering::SeqCst);
    let task = tokio::spawn(crate::bridge::dispatch(
        lab.broker.clone(),
        "mcp".into(),
        token,
        "messages.wait".into(),
        json!({"target":lab.b.target(),"wait_ms":5000}),
    ));
    observed(&lab.b.waits).await;
    assert!(lab.broker.test_disconnect_client(&lab.b.id).await);
    assert!(
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_cancelled_native_wait_leaves_later_candidates_available() {
    let lab = Lab::start().await;
    let target = lab.b.target();
    let task = tokio::spawn(crate::bridge::dispatch(
        lab.broker.clone(),
        lab.user.clone(),
        lab.token.clone(),
        "messages.wait".into(),
        json!({"target":target,"wait_ms":2000}),
    ));
    observed(&lab.b.waits).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    lab.call("send",json!({"to":to(&target),"text":"arrived after cancelled wait","idempotency_key":lab.key().await})).await.unwrap();
    let page = lab.call("read", json!({"target":target})).await.unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["consumed"], false);
    assert_eq!(lab.b.inbox.snapshot().candidates.len(), 1);
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_real_code_mode_composes_send_wait_read_and_ack() {
    let lab = Lab::start().await;
    let target = lab.b.target();
    let code = format!(
        r#"
        const target = {};
        const key = await messages.key();
        const waiting = messages.wait({{target, after:0, wait_ms:1000}});
        const sent = await messages.send({{to:{{scope:'session',...target}},text:'native composition',idempotency_key:key.idempotency_key}});
        const page = await waiting;
        const beforeAck = await messages.read({{target}});
        const ack = await messages.ack({{target,ids:page.messages.map(c=>c.message.delivery_id)}});
        const status = await messages.status({{message_id:sent.message_id,refresh:true}});
        return {{sent,page,beforeAck,ack,status}};
    "#,
        serde_json::to_string(&target).unwrap()
    );
    let response=reqwest::Client::new().post(format!("{}/{}",lab.base,lab.user))
        .bearer_auth(&lab.token).header("mcp-protocol-version","2025-11-25").header("accept","application/json")
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"code_mode","arguments":{
            "code":code,"timeout_ms":10000,"controller":{"name":"peer fixture","model":"no model invoked"}
        }}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let value = response.json::<Value>().await.unwrap();
    assert_ne!(value["result"]["isError"], true, "{value}");
    let result = &value["result"]["structuredContent"]["result"];
    assert_eq!(
        result["sent"]["recipients"][0]["state"], "accepted",
        "{value}"
    );
    assert_eq!(
        result["sent"]["source"]["run_id"].as_str().unwrap().len(),
        32
    );
    assert_eq!(
        result["sent"]["source"]["controller"]["name"],
        "peer fixture"
    );
    assert_eq!(result["page"]["messages"], result["beforeAck"]["messages"]);
    assert_eq!(
        result["status"]["recipients"][0]["receipt"]["state"],
        "acknowledged"
    );
    assert!(lab.b.inbox.snapshot().candidates.is_empty());
    assert_eq!(lab.b.deliveries.load(Ordering::SeqCst), 1);
    lab.stop().await;
}

#[tokio::test]
async fn peer_messages_full_inbox_is_explicitly_rejected_and_not_retried_after_space_frees() {
    let lab = Lab::start().await;
    let target = lab.b.target();
    let mut rejected = None;
    for _ in 0..32 {
        let args =
            json!({"to":to(&target),"text":"x".repeat(16*1024),"idempotency_key":lab.key().await});
        let result = lab.call("send", args.clone()).await.unwrap();
        if result["recipients"][0]["state"] == "rejected" {
            rejected = Some(args);
            break;
        }
        assert_eq!(result["recipients"][0]["state"], "accepted");
    }
    let args = rejected.expect("native byte bound must reject this batch");
    let count = lab.b.deliveries.load(Ordering::SeqCst);
    let ids = lab
        .b
        .inbox
        .snapshot()
        .candidates
        .iter()
        .map(|c| c.message.delivery_id.clone())
        .collect::<Vec<_>>();
    lab.call("ack", json!({"target":target,"ids":ids}))
        .await
        .unwrap();
    assert!(lab.b.inbox.snapshot().candidates.is_empty());
    let duplicate = lab.call("send", args).await.unwrap();
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["recipients"][0]["state"], "rejected");
    assert_eq!(lab.b.deliveries.load(Ordering::SeqCst), count);
    lab.stop().await;
}

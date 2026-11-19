use super::*;

#[tokio::test]
async fn messaging_helpers_preserve_explicit_selectors_waits_and_ack_without_native_tools() {
    let host: Host = Arc::new(|request| {
        Box::pin(async move {
            let HostRequest::Builtin { name, arguments } = request else {
                panic!("messaging helpers must use the authenticated builtin route")
            };
            Ok(json!({"name":name,"arguments":arguments}))
        })
    });
    let code = r#"
      const target = {client_id:'a'.repeat(64),session_id:'session_'+'b'.repeat(32)};
      const listed = await messages.list({include_threads:false});
      const key = await messages.key();
      const sent = await messages.send({to:{scope:'targets',targets:[target]},text:'peer',idempotency_key:'captured'});
      const read = await messages.receive({target,after:7});
      const wait = await messages.wait({target,after:7,wait_ms:30000});
      const ack = await messages.ack({target,ids:['delivery_'+'c'.repeat(64)]});
      const status = await messages.status({message_id:'peer_'+'d'.repeat(32),refresh:true});
      let rejected=false; try { await messages.send([]); } catch { rejected=true; }
      return {listed,key,sent,read,wait,ack,status,rejected,frozen:Object.isFrozen(messages)};
    "#;
    let result = execute(code.into(), 10_000, json!([]), host).await.unwrap();
    let result = &result["result"];
    assert_eq!(result["listed"]["name"], "messages.list");
    assert_eq!(result["key"]["name"], "messages.key");
    assert_eq!(result["sent"]["arguments"]["to"]["scope"], "targets");
    assert_eq!(result["read"]["name"], "messages.receive");
    assert_eq!(result["wait"]["arguments"]["wait_ms"], 30_000);
    assert_eq!(result["ack"]["name"], "messages.ack");
    assert_eq!(result["status"]["arguments"]["refresh"], true);
    assert_eq!(result["rejected"], true);
    assert_eq!(result["frozen"], true);
}

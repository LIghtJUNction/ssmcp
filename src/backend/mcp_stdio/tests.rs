use super::*;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-mcp-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("server.py"), SERVER).unwrap();
        Self(root)
    }
    async fn start(&self, request: Duration, frame_bytes: usize) -> Arc<McpClient> {
        let mut command = Command::new("python3");
        command.arg("-u").arg(self.0.join("server.py")).arg(&self.0);
        McpClient::start(
            &mut command,
            Limits {
                handshake: Duration::from_secs(4),
                request,
                frame_bytes,
            },
        )
        .await
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn wait_file(path: &Path) {
    timeout(Duration::from_secs(3), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn discovers_actual_pages_and_preserves_native_envelopes() {
    let fixture = Fixture::new();
    let client = fixture.start(Duration::from_secs(3), 65536).await;
    let tools = client.tools();
    assert_eq!(tools.len(), 6);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    let value = client
        .call("echo", json!({"text":"中文 e\u{301}"}))
        .await
        .unwrap();
    assert_eq!(value["structuredContent"]["text"], "中文 e\u{301}");
    assert_eq!(value["content"][0]["text"], "native result");
    assert_eq!(value["_meta"]["server"], "preserved");
    assert_eq!(value["isError"], true);
    assert!(
        client
            .call("missing", json!({}))
            .await
            .unwrap_err()
            .contains("not advertised")
    );
    assert!(
        client
            .call("fail", json!({}))
            .await
            .unwrap_err()
            .contains("-32004")
    );
    wait_file(&fixture.0.join("callback-rejected")).await;
    client.shutdown().await;
}

#[tokio::test]
async fn long_read_does_not_block_an_independent_call_or_misroute_its_reply() {
    let fixture = Fixture::new();
    let client = fixture.start(Duration::from_secs(4), 65536).await;
    let slow_client = client.clone();
    let slow = tokio::spawn(async move { slow_client.call("slow", json!({})).await });
    wait_file(&fixture.0.join("slow-started")).await;
    let fast = timeout(
        Duration::from_secs(2),
        client.call("echo", json!({"request":"fast"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(fast["structuredContent"]["request"], "fast");
    assert_eq!(
        slow.await.unwrap().unwrap()["structuredContent"]["request"],
        "slow"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn request_deadline_reaps_owned_descendants_without_replaying_the_call() {
    let fixture = Fixture::new();
    let client = fixture.start(Duration::from_millis(250), 65536).await;
    let error = client.call("hang", json!({})).await.unwrap_err();
    assert!(error.contains("timed out"));
    wait_file(&fixture.0.join("child-pid")).await;
    let pid = std::fs::read_to_string(fixture.0.join("child-pid")).unwrap();
    timeout(Duration::from_secs(7), client.shutdown())
        .await
        .unwrap();
    assert!(!Path::new("/proc").join(pid.trim()).exists());
    let calls = std::fs::read_to_string(fixture.0.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|line| *line == "hang").count(), 1);
}

#[tokio::test]
async fn oversized_frames_and_wrong_response_ids_fail_closed() {
    for name in ["oversized", "bad_id"] {
        let fixture = Fixture::new();
        let client = fixture.start(Duration::from_secs(3), 4096).await;
        let error = client.call(name, json!({})).await.unwrap_err();
        assert!(
            error.contains("frame limit") || error.contains("Invalid MCP response"),
            "{error}"
        );
        client.shutdown().await;
        let calls = std::fs::read_to_string(fixture.0.join("calls")).unwrap();
        assert_eq!(calls.lines().filter(|line| *line == name).count(), 1);
    }
}

const SERVER: &str = r#"
import json,pathlib,subprocess,sys
root=pathlib.Path(sys.argv[1]); slow=None
def send(v):
    sys.stdout.write(json.dumps(v,ensure_ascii=False)+'\n');sys.stdout.flush()
def result(i,v): send({'jsonrpc':'2.0','id':i,'result':v})
def native(v): return {'content':[{'type':'text','text':'native result'}],'structuredContent':v,'isError':True,'_meta':{'server':'preserved'}}
for line in sys.stdin:
    q=json.loads(line); method=q.get('method'); i=q.get('id')
    if i==900:
        assert q['error']['code']==-32601
        (root/'callback-rejected').write_text('ok')
        continue
    if method=='initialize':
        result(i,{'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'independent-fixture','version':'1'}})
    elif method=='notifications/initialized':
        send({'jsonrpc':'2.0','id':900,'method':'sampling/createMessage','params':{}})
    elif method=='tools/list':
        second=q.get('params',{}).get('cursor')=='page-2'
        names=['hang','oversized','bad_id'] if second else ['echo','slow','fail']
        page={'tools':[{'name':n,'description':'fixture '+n,'inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True}} for n in names]}
        if not second:page['nextCursor']='page-2'
        result(i,page)
    elif method=='tools/call':
        p=q['params'];name=p['name']
        with (root/'calls').open('a') as f:f.write(name+'\n')
        if name=='echo':
            result(i,native(p['arguments']))
            if slow is not None:result(slow,native({'request':'slow'}));slow=None
        elif name=='slow':slow=i;(root/'slow-started').write_text('ok')
        elif name=='fail':send({'jsonrpc':'2.0','id':i,'error':{'code':-32004,'message':'explicit failure'}})
        elif name=='hang':
            child=subprocess.Popen(['sleep','30'])
            (root/'child-pid').write_text(str(child.pid))
        elif name=='oversized':result(i,native({'text':'x'*10000}))
        elif name=='bad_id':result(i+200,native({'wrong':'id'}))
"#;

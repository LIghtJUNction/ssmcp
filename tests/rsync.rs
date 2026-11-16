#![cfg(target_os = "linux")]
//! Exercise the installed rsync protocol between two independently scoped peers.
use serde_json::{Value, json};
use ssmcp::rsync::Rsync;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, timeout};

struct Fixture {
    path: PathBuf,
    left: Arc<Rsync>,
    right: Arc<Rsync>,
}
impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ssmcp-rsync-test-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(path.join("left")).unwrap();
        std::fs::create_dir_all(path.join("right")).unwrap();
        let helper = PathBuf::from(env!("CARGO_BIN_EXE_ssmcp"));
        let left = Arc::new(Rsync::with_helper(path.join("left"), helper.clone()).unwrap());
        let right = Arc::new(Rsync::with_helper(path.join("right"), helper).unwrap());
        Self { path, left, right }
    }
    fn mkdir(&self, path: &str) {
        std::fs::create_dir_all(self.path.join(path)).unwrap();
    }
    fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        std::fs::write(self.path.join(path), content).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn pump(from: &Rsync, from_id: &str, to: &Rsync, to_id: &str) -> Result<(), String> {
    loop {
        let read = from
            .perform(
                "ssmcp.rsync.read",
                json!({"id":from_id,"wait_ms":1000,"max_bytes":32768}),
            )
            .await?;
        let data = read["data_base64"].as_str().unwrap();
        if !data.is_empty() {
            to.perform("ssmcp.rsync.write", json!({"id":to_id,"data_base64":data}))
                .await?;
        }
        if read["eof"] == true {
            // A clean process exit can close stdin before its counterpart reports
            // EOF. No payload remains in this half-close-only operation.
            let _ = to
                .perform("ssmcp.rsync.write", json!({"id":to_id,"close_stdin":true}))
                .await;
            return Ok(());
        }
    }
}

async fn exited(runtime: &Rsync, id: &str) -> Value {
    timeout(Duration::from_secs(4), async {
        loop {
            let status = runtime
                .perform("ssmcp.rsync.status", json!({"id":id}))
                .await
                .unwrap();
            if status["running"] == false {
                return status;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("rsync process remained alive after its protocol ended")
}

async fn transfer(
    from: &Rsync,
    source: &str,
    to: &Rsync,
    destination: &str,
    dry_run: Option<bool>,
    delete: bool,
) -> (Value, Value) {
    let mut args = json!({"role":"receiver","path":destination,"source":source,"delete":delete});
    if let Some(dry_run) = dry_run {
        args["dry_run"] = json!(dry_run);
    }
    let receiver = to.perform("ssmcp.rsync.start", args).await.unwrap();
    let sender = from
        .perform(
            "ssmcp.rsync.start",
            json!({"role":"sender","path":source,"server_args":receiver["server_args"]}),
        )
        .await
        .unwrap();
    let sid = sender["id"].as_str().unwrap();
    let rid = receiver["id"].as_str().unwrap();
    let result = timeout(Duration::from_secs(15), async {
        tokio::try_join!(pump(from, sid, to, rid), pump(to, rid, from, sid))
    })
    .await;
    let sender_status = exited(from, sid).await;
    let receiver_status = exited(to, rid).await;
    from.perform("ssmcp.rsync.close", json!({"id":sid}))
        .await
        .unwrap();
    to.perform("ssmcp.rsync.close", json!({"id":rid}))
        .await
        .unwrap();
    assert!(
        matches!(result, Ok(Ok(_))),
        "pump: {result:?}; sender {sender_status}; receiver {receiver_status}"
    );
    assert_eq!(sender_status["exit_code"], 0, "{sender_status}");
    assert_eq!(receiver_status["exit_code"], 0, "{receiver_status}");
    (sender_status, receiver_status)
}

#[tokio::test]
async fn real_rsync_binary_directories_dry_run_delete_and_reverse() {
    let fixture = Fixture::new();
    fixture.mkdir("left/source space/nested");
    fixture.mkdir("right/target space");
    fixture.mkdir("left/return");
    let bytes: Vec<u8> = (0..180_123usize)
        .map(|index| ((index * 37 + index / 101) % 256) as u8)
        .collect();
    fixture.write("left/source space/nested/binary.dat", &bytes);
    fixture.write("left/source space/.hidden", "hidden contents");
    fixture.write(
        "left/source space/quotes ' + 中文.txt",
        "unicode path survives",
    );
    fixture.write(
        "right/target space/stale.txt",
        "keep until explicitly deleted",
    );

    let (_, preview) = transfer(
        &fixture.left,
        "source space",
        &fixture.right,
        "target space",
        None,
        true,
    )
    .await;
    assert!(!fixture.path.join("right/target space/nested").exists());
    assert!(fixture.path.join("right/target space/stale.txt").exists());
    assert!(
        preview["log"].as_str().unwrap().contains("binary.dat"),
        "{preview}"
    );

    transfer(
        &fixture.left,
        "source space",
        &fixture.right,
        "target space",
        Some(false),
        false,
    )
    .await;
    assert_eq!(
        std::fs::read(fixture.path.join("right/target space/nested/binary.dat")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path.join("right/target space/quotes ' + 中文.txt"))
            .unwrap(),
        "unicode path survives"
    );
    assert!(fixture.path.join("right/target space/stale.txt").exists());
    transfer(
        &fixture.left,
        "source space",
        &fixture.right,
        "target space",
        Some(false),
        true,
    )
    .await;
    assert!(!fixture.path.join("right/target space/stale.txt").exists());

    transfer(
        &fixture.right,
        "target space",
        &fixture.left,
        "return",
        Some(false),
        false,
    )
    .await;
    assert_eq!(
        std::fs::read(fixture.path.join("left/return/nested/binary.dat")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path.join("left/return/.hidden")).unwrap(),
        "hidden contents"
    );
    fixture.left.shutdown().await;
    fixture.right.shutdown().await;
}

#[tokio::test]
async fn receiver_replaces_nested_symlink_without_touching_outside() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.mkdir("left/source/nested");
    fixture.mkdir("right/destination");
    fixture.mkdir("outside");
    fixture.write("left/source/nested/proof.txt", "inside replacement");
    fixture.write("outside/proof.txt", "outside must survive");
    symlink(
        fixture.path.join("outside"),
        fixture.path.join("right/destination/nested"),
    )
    .unwrap();
    symlink(
        fixture.path.join("outside/proof.txt"),
        fixture.path.join("left/source/skipped-link"),
    )
    .unwrap();
    transfer(
        &fixture.left,
        "source",
        &fixture.right,
        "destination",
        Some(false),
        false,
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(fixture.path.join("outside/proof.txt")).unwrap(),
        "outside must survive"
    );
    assert!(
        !std::fs::symlink_metadata(fixture.path.join("right/destination/nested"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path.join("right/destination/nested/proof.txt")).unwrap(),
        "inside replacement"
    );
    assert!(!fixture.path.join("right/destination/skipped-link").exists());
}

#[tokio::test]
async fn paths_handles_and_stream_bounds_are_enforced() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.mkdir("left/source");
    fixture.mkdir("right/destination");
    fixture.mkdir("outside");
    symlink(
        fixture.path.join("outside"),
        fixture.path.join("right/escape"),
    )
    .unwrap();
    for path in ["../outside", "/tmp", "escape", "missing"] {
        let result = fixture
            .right
            .perform(
                "ssmcp.rsync.start",
                json!({"role":"receiver","path":path,"source":"source"}),
            )
            .await;
        assert!(result.is_err(), "accepted {path}");
    }
    let receiver = fixture
        .right
        .perform(
            "ssmcp.rsync.start",
            json!({"role":"receiver","path":"destination","source":"source"}),
        )
        .await
        .unwrap();
    let rid = receiver["id"].as_str().unwrap();
    let mut invalid = receiver["server_args"].clone();
    invalid[2] = json!("--rsync-path=touch /tmp/unsafe");
    assert!(
        fixture
            .left
            .perform(
                "ssmcp.rsync.start",
                json!({"role":"sender","path":"source","server_args":invalid})
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .left
            .perform("ssmcp.rsync.status", json!({"id":rid}))
            .await
            .is_err()
    );
    assert!(
        fixture
            .right
            .perform("ssmcp.rsync.read", json!({"id":rid,"max_bytes":32769}))
            .await
            .is_err()
    );
    assert!(
        fixture
            .right
            .perform(
                "ssmcp.rsync.write",
                json!({"id":rid,"data_base64":"!invalid!"})
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .right
            .perform("ssmcp.rsync.read", json!({"id":rid,"wait_ms":1001}))
            .await
            .is_err()
    );
    fixture
        .right
        .perform("ssmcp.rsync.close", json!({"id":rid}))
        .await
        .unwrap();
    assert!(
        fixture
            .right
            .perform("ssmcp.rsync.status", json!({"id":rid}))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .right
            .perform("ssmcp.rsync.close", json!({"id":rid}))
            .await
            .unwrap()["existed"],
        false
    );
}

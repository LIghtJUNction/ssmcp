#![cfg(unix)]
use serde_json::{Value, json};
use ssmcp::{output::OutputStore, terminals::Terminals, wire::Operation};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-terminals-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("one")).unwrap();
        std::fs::create_dir_all(path.join("two")).unwrap();
        Self(path)
    }
    fn terminals(&self) -> Arc<Terminals> {
        Terminals::new(
            Path::new("/bin/sh"),
            self.0.clone(),
            OutputStore::new_connection().unwrap(),
        )
        .unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn open(terminals: &Terminals, name: &str, cwd: &str) -> String {
    terminals
        .call("terminal.open", json!({"name":name,"cwd":cwd}))
        .await
        .unwrap()["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned()
}
async fn write(terminals: &Terminals, id: &str, data: &str) {
    terminals
        .call("terminal.write", json!({"terminal_id":id,"data":data}))
        .await
        .unwrap();
}
async fn until(terminals: &Terminals, id: &str, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut cursor = 0;
        let mut text = String::new();
        loop {
            let read = terminals
                .call(
                    "terminal.read",
                    json!({"terminal_id":id,"cursor":cursor,"max_bytes":8192,"wait_ms":500}),
                )
                .await
                .unwrap();
            cursor = read["next_cursor"].as_u64().unwrap();
            text.push_str(read["text"].as_str().unwrap());
            if text.contains(needle) {
                return text;
            }
            assert_ne!(
                read["output_eof"], true,
                "shell exited before marker: {text}"
            );
        }
    })
    .await
    .expect("terminal did not produce expected output")
}

#[tokio::test]
async fn independent_shells_keep_cwd_environment_and_output_while_selection_changes() {
    let workspace = Workspace::new();
    let terminals = workspace.terminals();
    let (a, b) = tokio::join!(
        open(&terminals, "构建", "one"),
        open(&terminals, "测试", "two")
    );
    assert_ne!(a, b);
    tokio::join!(
        write(
            &terminals,
            &a,
            "stty -echo\nTOKEN=alpha; pwd; printf '\\nVALUE:%s\\n' \"$TOKEN\"\n"
        ),
        write(
            &terminals,
            &b,
            "stty -echo\nTOKEN=beta; pwd; printf '\\nVALUE:%s\\n' \"$TOKEN\"\n"
        ),
    );
    let (out_a, out_b) = tokio::join!(
        until(&terminals, &a, "\r\nVALUE:alpha\r\n"),
        until(&terminals, &b, "\r\nVALUE:beta\r\n"),
    );
    assert!(!out_a.contains("VALUE:beta"));
    assert!(!out_b.contains("VALUE:alpha"));
    assert!(out_a.contains(&workspace.0.join("one").to_string_lossy().to_string()));
    assert!(out_b.contains(&workspace.0.join("two").to_string_lossy().to_string()));
    let selected = terminals
        .call("terminal.select", json!({"terminal_id":b}))
        .await
        .unwrap();
    assert_eq!(selected["name"], "测试");
    let default = terminals.call("terminal.status", json!({})).await.unwrap();
    assert_eq!(default["terminal_id"], "main");
    assert!(Arc::ptr_eq(
        &terminals.selected(),
        &terminals.get(&b).unwrap()
    ));
    let list = terminals.call("terminal.list", json!({})).await.unwrap();
    assert_eq!(list["terminals"].as_array().unwrap().len(), 3);
    assert_eq!(list["selected"], b);
    terminals.shutdown().await;
}

#[tokio::test]
async fn interactive_wait_in_one_shell_does_not_block_another() {
    let workspace = Workspace::new();
    let terminals = workspace.terminals();
    let a = open(&terminals, "interactive", "one").await;
    let b = open(&terminals, "background", "two").await;
    write(&terminals, &a, "stty -echo\nprintf '\\n%s\\n' waiting; read answer; printf '\\nANSWER:%s\\n' \"$answer\"\n").await;
    until(&terminals, &a, "\r\nwaiting\r\n").await;
    write(
        &terminals,
        &b,
        "stty -echo\nprintf '\\n%s\\n' independent\n",
    )
    .await;
    until(&terminals, &b, "\r\nindependent\r\n").await;
    write(&terminals, &a, "继续\n").await;
    until(&terminals, &a, "\r\nANSWER:继续\r\n").await;
    terminals.shutdown().await;
}

#[tokio::test]
async fn close_is_scoped_idempotent_and_keeps_default_terminal_available() {
    let workspace = Workspace::new();
    let terminals = workspace.terminals();
    let id = open(&terminals, "temporary", ".").await;
    let owned = terminals.get(&id).unwrap();
    terminals
        .call("terminal.select", json!({"terminal_id":id}))
        .await
        .unwrap();
    let closed = terminals
        .call("terminal.close", json!({"terminal_id":id}))
        .await
        .unwrap();
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["selected"], "main");
    assert_eq!(
        owned.perform(Operation::Status {}).await.unwrap()["running"],
        false
    );
    assert_eq!(
        terminals
            .call("terminal.close", json!({"terminal_id":id}))
            .await
            .unwrap()["closed"],
        false
    );
    assert!(
        terminals
            .call(
                "terminal.write",
                json!({"terminal_id":id,"data":"echo should-not-run\n"})
            )
            .await
            .is_err()
    );
    assert!(
        terminals
            .call("terminal.close", json!({"terminal_id":"main"}))
            .await
            .is_err()
    );
    assert_eq!(
        terminals.call("terminal.status", json!({})).await.unwrap()["running"],
        true
    );
    let main = terminals.primary();
    terminals.shutdown().await;
    assert_eq!(
        main.perform(Operation::Status {}).await.unwrap()["running"],
        false
    );
    assert!(terminals.call("terminal.open", json!({})).await.is_err());
}

#[tokio::test]
async fn terminal_capacity_and_arguments_are_checked_before_spawning() {
    let workspace = Workspace::new();
    let terminals = workspace.terminals();
    for args in [
        json!({"cwd":"/"}),
        json!({"name":"bad\nname"}),
        json!({"name":" "}),
        json!({"select":"yes"}),
        json!({"unexpected":true}),
    ] {
        assert!(terminals.call("terminal.open", args).await.is_err());
    }
    assert_eq!(
        terminals.call("terminal.list", json!({})).await.unwrap()["terminals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let limit = terminals.call("terminal.list", json!({})).await.unwrap()["max_terminals"]
        .as_u64()
        .unwrap();
    let mut first = String::new();
    for n in 1..limit {
        let id = open(&terminals, &format!("session-{n}"), ".").await;
        if n == 1 {
            first = id;
        }
    }
    assert!(terminals.call("terminal.open", json!({})).await.is_err());
    terminals
        .call("terminal.close", json!({"terminal_id":first}))
        .await
        .unwrap();
    let next = open(&terminals, "replacement", ".").await;
    assert_ne!(next, first);
    assert!(
        terminals
            .call("terminal.status", json!({"terminal_id":Value::Null}))
            .await
            .is_err()
    );
    terminals.shutdown().await;
}

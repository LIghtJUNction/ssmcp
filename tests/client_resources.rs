#![cfg(target_os = "linux")]
//! Client-owned manifests and resource contents traverse the real native runtime.
use serde_json::{Value, json};
use ssmcp::{backend::Backend, output::OutputStore, runtime::Runtime, wire::Operation};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn native(runtime: &Runtime, name: &str, arguments: Value) -> Value {
    runtime
        .perform(Operation::CallTool {
            name: name.into(),
            arguments,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn client_resources_are_versioned_paged_fresh_and_permission_scoped() {
    let root = Root(std::env::temp_dir().join(format!(
        "ssmcp-client-resources-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::create_dir_all(&root.0).unwrap();
    let document = "Complete client-owned instruction line.\n".repeat(2048);
    std::fs::write(root.0.join("AGENTS.md"), &document).unwrap();
    let backend = Backend::with_output(
        Path::new("/bin/sh"),
        root.0.clone(),
        OutputStore::new_connection().unwrap(),
    )
    .await
    .unwrap();
    let runtime = Runtime::new(backend, root.0.clone()).unwrap();

    let manifest = native(&runtime, "ssmcp.client.capabilities", json!({})).await;
    let manifest = &manifest["structuredContent"];
    assert_eq!(manifest["schema"], "ssmcp.client.capabilities.v1");
    assert_eq!(manifest["client_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["wire_version"], ssmcp::wire::VERSION);
    assert_eq!(manifest["providers"]["client"], 4);
    assert_eq!(manifest["resources"]["read"], "ssmcp.resources.read");

    let mut args = json!({"limit":2});
    let mut uris = std::collections::HashSet::new();
    loop {
        let page = native(&runtime, "ssmcp.resources.list", args).await;
        let page = &page["structuredContent"];
        let entries = page["resources"].as_array().unwrap();
        assert!(entries.len() <= 2);
        for entry in entries {
            assert!(uris.insert(entry["uri"].as_str().unwrap().to_owned()));
        }
        let Some(cursor) = page["nextCursor"].as_str() else {
            break;
        };
        args = json!({"cursor":cursor});
    }
    assert!(uris.contains("ssmcp-client://capabilities"));
    assert!(uris.contains("ssmcp-client://limits"));
    assert!(uris.contains("ssmcp-client://guide"));
    let templates = native(&runtime, "ssmcp.resources.templates", json!({"limit":1})).await;
    assert_eq!(
        templates["structuredContent"]["resourceTemplates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(templates["structuredContent"]["nextCursor"].is_string());

    let uri = "ssmcp-client://agents/read?path=AGENTS.md";
    let first = native(&runtime, "ssmcp.resources.read", json!({"uri":uri})).await;
    assert_eq!(first["structuredContent"]["contents"][0]["text"], document);
    assert_ne!(first["structuredContent"]["spilled"], true);
    std::fs::write(
        root.0.join("AGENTS.md"),
        "Updated while the same client stays online.\n",
    )
    .unwrap();
    let changed = native(&runtime, "ssmcp.resources.read", json!({"uri":uri})).await;
    assert_eq!(
        changed["structuredContent"]["contents"][0]["text"],
        "Updated while the same client stays online.\n"
    );
    for uri in [
        "file:///etc/passwd",
        "ssmcp-client://agents/read?path=..%2Fsecret",
        "ssmcp-client://agents/read?root=not-granted&path=AGENTS.md",
    ] {
        assert!(
            runtime
                .perform(Operation::CallTool {
                    name: "ssmcp.resources.read".into(),
                    arguments: json!({"uri":uri}),
                })
                .await
                .is_err(),
            "{uri}"
        );
    }
    let limits = native(
        &runtime,
        "ssmcp.resources.read",
        json!({"uri":"ssmcp-client://limits"}),
    )
    .await;
    let limits: Value = serde_json::from_str(
        limits["structuredContent"]["contents"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(limits["client_version"], env!("CARGO_PKG_VERSION"));
    assert!(
        limits["tool_catalog"]["max_tools_per_page"]
            .as_u64()
            .unwrap()
            > 0
    );
    runtime.shutdown().await;
    assert!(
        runtime
            .perform(Operation::CallTool {
                name: "ssmcp.client.capabilities".into(),
                arguments: json!({}),
            })
            .await
            .is_err()
    );
}

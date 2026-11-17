#![cfg(target_os = "linux")]
//! Full documents survive the real runtime output path used by resources/read.
use serde_json::json;
use ssmcp::{backend::Backend, output::OutputStore, runtime::Runtime, wire::Operation};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[tokio::test]
async fn bounded_context_documents_remain_inline_and_fresh_in_native_runtime() {
    let root = std::env::temp_dir().join(format!(
        "ssmcp-context-runtime-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("skills/large")).unwrap();
    let instructions = "Keep this complete instruction line.\n".repeat(2048);
    let skill = format!(
        "---\nname: large\ndescription: Complete document\n---\n{}",
        "Keep this skill line.\n".repeat(3072)
    );
    std::fs::write(root.join("AGENTS.md"), &instructions).unwrap();
    std::fs::write(root.join("skills/large/SKILL.md"), &skill).unwrap();
    let backend = Backend::with_output(
        Path::new("/bin/sh"),
        root.clone(),
        OutputStore::new_connection().unwrap(),
    )
    .await
    .unwrap();
    let runtime = Runtime::new(backend, root.clone()).unwrap();

    for (name, path, expected) in [
        ("ssmcp.agents.read", "AGENTS.md", instructions.as_str()),
        ("ssmcp.skills.read", "skills/large/SKILL.md", skill.as_str()),
    ] {
        let value = runtime
            .perform(Operation::CallTool {
                name: name.into(),
                arguments: json!({"path":path}),
            })
            .await
            .unwrap();
        assert_eq!(value["structuredContent"]["content"], expected);
        assert_ne!(value["structuredContent"]["spilled"], true);
    }
    let value = runtime
        .perform(Operation::CallTool {
            name: "ssmcp.agents.resolve".into(),
            arguments: json!({}),
        })
        .await
        .unwrap();
    assert_eq!(
        value["structuredContent"]["files"][0]["content"],
        instructions
    );

    std::fs::write(
        root.join("AGENTS.md"),
        "Updated while the runtime stays connected.\n",
    )
    .unwrap();
    let value = runtime
        .perform(Operation::CallTool {
            name: "ssmcp.agents.read".into(),
            arguments: json!({"path":"AGENTS.md"}),
        })
        .await
        .unwrap();
    assert_eq!(
        value["structuredContent"]["content"],
        "Updated while the runtime stays connected.\n"
    );
    std::fs::write(root.join("AGENTS.md"), vec![b'x'; 262145]).unwrap();
    assert!(
        runtime
            .perform(Operation::CallTool {
                name: "ssmcp.agents.read".into(),
                arguments: json!({"path":"AGENTS.md"})
            })
            .await
            .unwrap_err()
            .contains("no partial")
    );

    runtime.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

//! Local picker discovery must remain an audited read without becoming chat.
use super::*;
use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-skill-picker-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn skill(&self, path: &str, name: &str) {
        let directory = self.0.join(".agents/skills").join(path);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("SKILL.md"), format!(
            "---\nname: {name}\ndescription: fixture {name}\n---\nPrivate instructions for {name}.\n"
        )).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn local_skill_discovery_keeps_security_audit_without_emitting_chat_activity() {
    const CHILD: &str = "SSMCP_TEST_LOCAL_SKILL_PICKER";
    if std::env::var_os(CHILD).is_none() {
        // The process boundary captures the real human transcript and isolates
        // its global renderer from all other unit tests.
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "context::completion_tests::local_skill_discovery_keeps_security_audit_without_emitting_chat_activity", "--nocapture"])
            .env(CHILD, "1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let shown = String::from_utf8_lossy(&output.stderr);
        assert!(shown.contains("chosen/SKILL.md"), "{shown}");
        assert!(shown.contains("forged-picker-marker/SKILL.md"), "{shown}");
        assert!(!shown.contains("unselected/SKILL.md"), "{shown}");
        assert!(!shown.contains(".system/"), "{shown}");
        assert!(
            !shown.contains(".trash/")
                && !shown.contains(".archive/")
                && !shown.contains(".cache/"),
            "{shown}"
        );
        return;
    }
    let fixture = Fixture::new();
    for (path, name) in [
        ("chosen", "chosen"),
        ("unselected", "unselected"),
        (".system/system-check", "system-check"),
        (".trash/retired", "trashed"),
        (".archive/old", "archived"),
        (".cache/copied", "cached"),
    ] {
        fixture.skill(path, name);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let context = Context::with_output(
            fixture.0.clone(),
            vec![],
            crate::output::OutputStore::new_connection().unwrap(),
        )
        .unwrap();
        let index = context
            .local_skill_index(json!({"scope":"effective","limit":64}))
            .await
            .unwrap();
        let names: std::collections::BTreeSet<_> = index["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|skill| skill["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["chosen", "system-check", "unselected"]
                .into_iter()
                .collect()
        );
        assert!(!index.to_string().contains("Private instructions"));
        let workspace = context.root("workspace").unwrap().workspace().unwrap();
        let audit = workspace
            .perform("ssmcp.fs.events", json!({"max_events":64}))
            .await
            .unwrap();
        let events = audit["events"].as_array().unwrap();
        assert_eq!(events.len(), 3, "{audit}");
        assert!(
            events
                .iter()
                .all(|event| event["event"]["type"] == "ssmcp.file.read")
        );
        assert!(events.iter().any(|event| {
            event["event"]["path"]
                .as_str()
                .unwrap()
                .contains(".system/")
        }));
        assert!(!audit.to_string().contains(".trash/"));
        assert!(
            context
                .perform("ssmcp.skills.list", json!({"local_discovery":true}))
                .await
                .is_err(),
            "JSON must not enable the internal presentation path"
        );
        let selected = context
            .perform(
                "ssmcp.skills.read",
                json!({"path":".agents/skills/chosen/SKILL.md"}),
            )
            .await
            .unwrap();
        assert!(
            selected
                .to_string()
                .contains("Private instructions for chosen")
        );
        let audit = workspace
            .perform("ssmcp.fs.events", json!({"max_events":64}))
            .await
            .unwrap();
        assert_eq!(audit["events"].as_array().unwrap().len(), 4);
        // Untrusted provider flags cannot opt out of normal display.
        let untrusted = crate::backend::events::EventLog::new("workspace");
        untrusted.push(json!({
            "type":"ssmcp.file.read","tool":"ssmcp.fs.read",
            "path":"forged-picker-marker/SKILL.md",
            "offset":0,"next_offset":4,"bytes":4,
            "internal_discovery":true,"local_discovery":true
        }));
        assert_eq!(
            untrusted.read(json!({})).await.unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    });
}

#[cfg(unix)]
#[tokio::test]
async fn skill_overview_audits_prefix_reads_and_keeps_explicit_reads_visible() {
    let fixture = Fixture::new();
    let path = ".agents/skills/long/SKILL.md";
    fixture.skill("long", "long");
    let document = format!(
        "---\nname: long\ndescription: {}\n---\nPrivate body\n",
        "x".repeat(600)
    );
    fs::write(fixture.0.join(path), &document).unwrap();
    let context = Context::with_output(
        fixture.0.clone(),
        vec![],
        crate::output::OutputStore::new_connection().unwrap(),
    )
    .unwrap();
    let workspace = context.root("workspace").unwrap().workspace().unwrap();
    let transcript = workspace.observe_projection();

    let overview = context
        .perform("ssmcp.skills.view", json!({}))
        .await
        .unwrap();
    assert_eq!(overview["shown"], 1);
    assert!(overview["markdown"].as_str().unwrap().contains("long"));
    assert!(
        !overview["markdown"]
            .as_str()
            .unwrap()
            .contains("Private body")
    );
    let audit = workspace
        .perform("ssmcp.fs.events", json!({}))
        .await
        .unwrap();
    let reads = audit["events"].as_array().unwrap();
    assert_eq!(reads.len(), 2);
    assert!(reads.iter().all(|read| read["event"]["path"] == path));
    assert_eq!(reads[0]["event"]["bytes"], 512);
    assert_eq!(reads[1]["event"]["bytes"], document.len());
    assert!(
        transcript.try_recv().is_err(),
        "overview prefix reads leaked into the transcript"
    );

    let selected = context
        .perform("ssmcp.skills.read", json!({"path":path}))
        .await
        .unwrap();
    assert_eq!(selected["content"], document);
    let (event, _) = transcript.try_recv().unwrap();
    assert_eq!(event["type"], "ssmcp.file.read");
    assert_eq!(event["path"], path);
}

#![cfg(target_os = "linux")]
//! Real isolated JavaScript helpers against scoped context and file runtimes.
//! The host callback is deterministic; no model, SSH key, or production service is used.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ssmcp::{
    backend,
    code_mode::{self, Host, HostRequest},
    context::Context,
    workspace::Workspace,
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

struct Fixture {
    directory: PathBuf,
    workspace: Arc<Workspace>,
    context: Arc<Context>,
}

impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("ssmcp-context-e2e-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(directory.join("workspace")).unwrap();
        std::fs::create_dir_all(directory.join("external")).unwrap();
        let workspace = Arc::new(Workspace::new(directory.join("workspace")).unwrap());
        let context = Arc::new(
            Context::new(
                directory.join("workspace"),
                vec![directory.join("external")],
            )
            .unwrap(),
        );
        Self {
            directory,
            workspace,
            context,
        }
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.directory.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn host(&self) -> Host {
        let workspace = self.workspace.clone();
        let context = self.context.clone();
        Arc::new(move |request| {
            let workspace = workspace.clone();
            let context = context.clone();
            Box::pin(async move {
                match request {
                    HostRequest::Call {
                        client_id,
                        name,
                        arguments,
                    } => {
                        if client_id != "fixture" {
                            return Err("unknown fixture client".into());
                        }
                        let result = if name.starts_with("ssmcp.fs.") {
                            workspace.perform(&name, arguments).await?
                        } else {
                            context.perform(&name, arguments).await?
                        };
                        Ok(backend::result(result))
                    }
                    HostRequest::Tools { client_id, cursor } => {
                        if client_id != "fixture" || cursor.is_some() {
                            return Err("unknown fixture catalog".into());
                        }
                        let mut tools = Workspace::tools();
                        tools.extend(Context::tools());
                        Ok(json!({"tools":tools}))
                    }
                    HostRequest::Builtin { .. } => {
                        Err("fixture only exposes scoped native calls".into())
                    }
                    HostRequest::WaitUser { .. } => {
                        Err("context helper fixture does not support user waits".into())
                    }
                }
            })
        })
    }

    async fn javascript(&self, code: &str) -> Value {
        code_mode::execute(
            code.into(), 15_000,
            json!([{"id":"fixture","name":"project","type":"shell","cwd":self.directory.join("workspace"),"hostname":"test"}]),
            self.host(),
        ).await.expect("isolated JavaScript context helpers failed")["result"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap and permitted Linux namespaces; run with --ignored"]
async fn skill_discovery_and_instruction_resolution_preserve_scope_and_provenance() {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new();
    let body_one = "---\nname: duplicate\ndescription: >-\n  Parse logs:\n  keep # signs.\n---\n\nBODY_ONE_MUST_NOT_APPEAR_IN_SUMMARY\n";
    let body_two = "---\nname: duplicate\ndescription: 'Second: preserve # text'\n---\n\nBODY_TWO_MUST_NOT_APPEAR_IN_SUMMARY\n";
    fixture.write("workspace/.agents/skills/duplicate/SKILL.md", body_one);
    fixture.write("workspace/skills/duplicate/SKILL.md", body_two);
    fixture.write("external/.agents/skills/global-guide/SKILL.md",
        "---\nname: global-guide\ndescription: Explicitly configured global guide.\n---\nGLOBAL_BODY_NOT_IN_DEFAULT_VIEW\n");
    fixture.write(
        "external/SKILL.md",
        "---\nname: escape\ndescription: Must stay outside discovery.\n---\nOUTSIDE_SECRET\n",
    );
    fixture.write("workspace/AGENTS.md", "Root instructions.\n");
    fixture.write("workspace/packages/AGENTS.md", "Package instructions.\n");
    fixture.write("workspace/packages/app/AGENTS.md", "UPPERCASE_SHADOWED\n");
    fixture.write(
        "workspace/packages/app/AGENTS.override.md",
        "App override.\n",
    );
    fixture.write("workspace/packages/app/src/AGENTS.override.md", "");
    fixture.write(
        "workspace/packages/app/src/agents.md",
        "Fallback instructions.\n",
    );
    fixture.write("workspace/packages/app/src/main.rs", "fn main() {}\n");
    fixture.write("workspace/packages/other/AGENTS.md", "UNRELATED_SIBLING\n");
    fixture.write("external/AGENTS.md", "Explicit global instructions.\n");
    std::os::unix::fs::symlink(
        fixture.directory.join("external"),
        fixture.directory.join("workspace/.agents/skills/escape"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        fixture.directory.join("external"),
        fixture.directory.join("workspace/escape"),
    )
    .unwrap();

    let result = fixture.javascript(r#"
        const id = clients[0].id;
        const roots = await context.roots(id);
        const catalog = await tools(id);
        const index = await skills.list(id);
        const first = await skills.list(id, { limit: 1 });
        const second = await skills.list(id, { limit: 1, cursor: first.next_cursor });
        const view = await skills.view(id);
        const selected = await skills.list(id, { paths: ["skills"] });
        const read = await skills.read(id, { path: ".agents/skills/duplicate/SKILL.md" });
        const global = await skills.list(id, { root: "context-1" });
        const ordered = await agents.resolve(id, { path: "packages/app/src/main.rs" });
        const list = await agents.list(id, { path: "packages/app/src/main.rs" });
        const explicit = await agents.read(id, { root: "context-1", path: "AGENTS.md" });
        let escaped = false, arbitrary = false, foreign = false, unknownRoot = false;
        try { await agents.read(id, { path: "escape/AGENTS.md" }); } catch (_) { escaped = true; }
        try { await skills.read(id, { path: "packages/app/src/main.rs" }); } catch (_) { arbitrary = true; }
        try { await skills.list("someone-else"); } catch (_) { foreign = true; }
        try { await skills.list(id, { root: "context-99" }); } catch (_) { unknownRoot = true; }
        return { roots, names: catalog.tools.map(t => t.name), index, first, second, view,
                 read, global, ordered, list, explicit, selected, escaped, arbitrary, foreign, unknownRoot,
                 frozen: [context, skills, agents, fs].map(Object.isFrozen) };
    "#).await;
    assert_eq!(result["roots"]["roots"][0]["id"], "workspace");
    assert_eq!(result["roots"]["roots"][1]["id"], "context-1");
    assert!(
        result["roots"]["roots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|root| root["read_only"] == true)
    );
    let skills = result["index"]["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 2, "{}", result["index"]);
    assert!(skills.iter().all(|skill| skill["name"] == "duplicate"));
    assert_ne!(skills[0]["path"], skills[1]["path"]);
    assert!(skills.iter().any(|skill| {
        skill["description"]
            .as_str()
            .unwrap()
            .contains("Parse logs: keep # signs.")
    }));
    assert!(!result["index"].to_string().contains("BODY_ONE_MUST"));
    assert!(
        !result["view"]["markdown"]
            .as_str()
            .unwrap()
            .contains("BODY_TWO_MUST")
    );
    assert!(!result["index"].to_string().contains("OUTSIDE_SECRET"));
    assert_eq!(result["read"]["content"], body_one);
    assert_eq!(result["read"]["root"]["id"], "workspace");
    assert_eq!(result["global"]["skills"].as_array().unwrap().len(), 1);
    assert_eq!(result["global"]["skills"][0]["name"], "global-guide");
    assert_eq!(result["global"]["root"]["id"], "context-1");
    assert_eq!(result["view"]["scope"], "effective");
    assert!(
        result["view"]["markdown"]
            .as_str()
            .unwrap()
            .contains("global-guide")
    );
    assert!(
        result["view"]["markdown"]
            .as_str()
            .unwrap()
            .contains("context-1")
    );
    assert!(
        !result["view"]["markdown"]
            .as_str()
            .unwrap()
            .contains("GLOBAL_BODY_NOT")
    );

    assert_eq!(result["selected"]["skills"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["selected"]["skills"][0]["path"],
        "skills/duplicate/SKILL.md"
    );
    assert_eq!(result["first"]["skills"].as_array().unwrap().len(), 1);
    assert_eq!(result["second"]["skills"].as_array().unwrap().len(), 1);
    assert_ne!(
        result["first"]["skills"][0]["path"],
        result["second"]["skills"][0]["path"]
    );
    let instruction_paths: Vec<_> = result["ordered"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        instruction_paths,
        [
            "AGENTS.md",
            "packages/AGENTS.md",
            "packages/app/AGENTS.override.md",
            "packages/app/src/agents.md"
        ]
    );
    assert!(!result["ordered"].to_string().contains("UPPERCASE_SHADOWED"));
    assert!(!result["ordered"].to_string().contains("UNRELATED_SIBLING"));
    assert!(
        !result["ordered"]
            .to_string()
            .contains("Explicit global instructions.")
    );
    assert!(
        result["list"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file.get("content").is_none())
    );
    assert_eq!(
        result["explicit"]["content"],
        "Explicit global instructions.\n"
    );
    assert_eq!(result["escaped"], true);
    assert_eq!(result["arbitrary"], true);
    assert_eq!(result["foreign"], true);
    assert_eq!(result["unknownRoot"], true);
    assert_eq!(result["frozen"], json!([true, true, true, true]));
    assert!(
        result["names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "ssmcp.skills.list")
    );
    assert!(
        result["names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "ssmcp.fs.read")
    );
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap and permitted Linux namespaces; run with --ignored"]
async fn text_helpers_preserve_unicode_binary_compatibility_and_edit_preconditions() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new();
    let original = "\u{feff}你好🙂\r\nkeep\r\n";
    fixture.write("workspace/note.txt", original);
    std::fs::set_permissions(
        fixture.directory.join("workspace/note.txt"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    fixture.write(
        "external/AGENTS.md",
        "External context remains read only.\n",
    );
    fixture.write("workspace/duplicate.txt", "repeat repeat");
    let result = fixture.javascript(r#"
        const id = clients[0].id;
        const raw = await fs.read(id, { path: "note.txt" });
        const full = await fs.readText(id, { path: "note.txt" });
        let offset = 0, assembled = "", pageHashes = [], last;
        do {
            last = await fs.readText(id, { path: "note.txt", offset, max_bytes: 4 });
            assembled += last.text;
            pageHashes.push(last.sha256);
            offset = last.next_offset;
        } while (!last.eof);
        const rewritten = await fs.writeText(id, {
            path: "note.txt", text: full.text + "tail\r\n", overwrite: true,
            expected_sha256: full.sha256
        });
        const edited = await fs.editText(id, {
            path: "note.txt", old_text: "keep", new_text: "kept",
            expected_sha256: rewritten.sha256
        });
        let staleError = "", ambiguousError = "", traversal = false, rootWrite = false;
        try { await fs.editText(id, {path:"note.txt",old_text:"kept",new_text:"wrong",expected_sha256:full.sha256}); }
        catch (e) { staleError = e.message; }
        try { await fs.editText(id, {path:"duplicate.txt",old_text:"repeat",new_text:"changed"}); }
        catch (e) { ambiguousError = e.message; }
        try { await fs.writeText(id, {path:"../external/AGENTS.md",text:"wrong",overwrite:true}); }
        catch (_) { traversal = true; }
        try { await fs.writeText(id, {root:"context-1",path:"AGENTS.md",text:"wrong",overwrite:true}); }
        catch (_) { rootWrite = true; }
        const current = await fs.readText(id, {path:"note.txt"});
        const direct = await call(id, "ssmcp.fs.read_text", {path:"note.txt"});
        const duplicate = await fs.readText(id, {path:"duplicate.txt"});
        const stat = await fs.stat(id, {path:"note.txt"});
        return {raw,full,assembled,pageHashes,rewritten,edited,current,direct,duplicate,stat,
                staleError,ambiguousError,traversal,rootWrite,
                frozen:[fs,skills,agents,context].map(Object.isFrozen),
                processType:typeof process,requireType:typeof require};
    "#).await;
    assert_eq!(
        result["raw"]["data_base64"],
        STANDARD.encode(original.as_bytes())
    );
    assert_eq!(result["full"]["text"], original);
    assert_eq!(
        result["full"]["sha256"],
        format!("{:x}", Sha256::digest(original.as_bytes()))
    );
    assert_eq!(result["assembled"], original);
    assert!(
        result["pageHashes"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_null)
    );
    assert_eq!(
        result["current"]["text"],
        "\u{feff}你好🙂\r\nkept\r\ntail\r\n"
    );
    assert_eq!(
        result["direct"]["structuredContent"]["text"],
        result["current"]["text"]
    );
    assert_eq!(
        result["edited"]["previous_sha256"],
        result["rewritten"]["sha256"]
    );
    assert_eq!(result["edited"]["replacements"], 1);
    assert_eq!(result["stat"]["mode"], 0o640);
    assert_eq!(result["duplicate"]["text"], "repeat repeat");
    assert!(!result["staleError"].as_str().unwrap().is_empty());
    assert!(!result["ambiguousError"].as_str().unwrap().is_empty());
    assert_ne!(
        result["staleError"],
        "workspace file operation failed; inspect the native result"
    );
    assert_eq!(result["traversal"], true);
    assert_eq!(result["rootWrite"], true);
    assert_eq!(
        std::fs::read_to_string(fixture.directory.join("external/AGENTS.md")).unwrap(),
        "External context remains read only.\n"
    );
    assert_eq!(result["frozen"], json!([true, true, true, true]));
    assert_eq!(result["processType"], "undefined");
    assert_eq!(result["requireType"], "undefined");
}

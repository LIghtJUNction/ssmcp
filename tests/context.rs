use serde_json::{Value, json};
use ssmcp::context::Context;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    extra: PathBuf,
    context: Context,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ssmcp-context-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("workspace");
        let extra = base.join("extra");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&extra).unwrap();
        let context = Context::new(root.clone(), vec![extra.clone()]).unwrap();
        Self {
            base,
            root,
            extra,
            context,
        }
    }
    fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    async fn call(&self, name: &str, args: Value) -> Value {
        self.context.perform(name, args).await.unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}
fn codes(value: &Value) -> Vec<&str> {
    value["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["code"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn skill_metadata_preserves_quoted_block_utf8_and_duplicate_paths() {
    let f = Fixture::new();
    let first = "---\nname: duplicate\ndescription: \"Use files: # tags\" # comment\nmetadata:\n  author: fixture\n---\nSECRET_BODY_ONE\n";
    f.write(".agents/skills/z/SKILL.md", first);
    f.write("skills/a/SKILL.md", "---\nname: 'duplicate'\ndescription: >-\n  读取日志：\n  保留 # 标记。\n---\nSECRET_BODY_TWO\n");
    f.write(
        ".codex/skills/literal/SKILL.md",
        "---\nname: literal\ndescription: |\n  first line\n  second line\n---\n",
    );
    f.write(
        ".claude/skills/quoted/SKILL.md",
        "\u{feff}---\r\nname: 'quoted'\r\ndescription: 'it''s <safe> | text'\r\n---\r\nBODY\r\n",
    );
    let page = f.call("ssmcp.skills.list", json!({"limit":2})).await;
    assert_eq!(page["root"]["id"], "workspace");
    assert_eq!(page["total"], 4);
    assert_eq!(page["skills"][0]["path"], ".agents/skills/z/SKILL.md");
    assert_eq!(page["skills"][0]["description"], "Use files: # tags");
    assert_eq!(page["skills"][1]["description"], "it's <safe> | text");
    assert!(codes(&page).contains(&"duplicate_name"));
    let next = f
        .call(
            "ssmcp.skills.list",
            json!({"limit":2,"cursor":page["next_cursor"]}),
        )
        .await;
    assert_eq!(
        next["skills"][0]["description"],
        "first line\nsecond line\n"
    );
    assert_eq!(next["skills"][1]["description"], "读取日志： 保留 # 标记。");
    assert!(next.get("next_cursor").is_none());
    let view = f.call("ssmcp.skills.view", json!({})).await;
    let markdown = view["markdown"].as_str().unwrap();
    assert!(!markdown.contains("SECRET_BODY"));
    assert!(markdown.contains("&lt;safe&gt; \\| text"));
    assert!(view.get("skills").is_none());
    let read = f
        .call(
            "ssmcp.skills.read",
            json!({"path":".agents/skills/z/SKILL.md"}),
        )
        .await;
    assert_eq!(read["content"], first);
    assert_eq!(read["metadata"]["status"], "ok");
    assert_eq!(read["sha256"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn skill_cursors_are_bound_to_root_paths_limits_and_current_metadata() {
    let f = Fixture::new();
    for name in ["a", "b"] {
        f.write(
            &format!("skills/{name}/SKILL.md"),
            format!("---\nname: {name}\ndescription: original\n---\n"),
        );
    }
    let first = f
        .call("ssmcp.skills.list", json!({"paths":["skills"],"limit":1}))
        .await;
    let cursor = first["next_cursor"].clone();
    let next = f
        .call(
            "ssmcp.skills.list",
            json!({"paths":["skills"],"limit":64,"cursor":cursor}),
        )
        .await;
    assert_eq!(next["skills"].as_array().unwrap().len(), 1);
    for args in [
        json!({"root":"context-1","paths":["skills"],"cursor":cursor}),
        json!({"paths":["."],"cursor":cursor}),
        json!({"paths":["skills"],"max_depth":3,"cursor":cursor}),
    ] {
        assert!(
            f.context
                .perform("ssmcp.skills.list", args)
                .await
                .unwrap_err()
                .contains("changed")
        );
    }
    f.write(
        "skills/a/SKILL.md",
        "---\nname: a\ndescription: changed\n---\n",
    );
    assert!(
        f.context
            .perform(
                "ssmcp.skills.list",
                json!({"paths":["skills"],"cursor":cursor})
            )
            .await
            .unwrap_err()
            .contains("changed")
    );
    assert!(
        f.context
            .perform("ssmcp.skills.list", json!({"cursor":"invalid"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unsupported_or_malformed_yaml_has_diagnostics_and_raw_read_remains_available() {
    let f = Fixture::new();
    let fixtures = [
        (
            "array",
            "---\nname: array\ndescription: [one, two]\n---\nRAW\n",
        ),
        (
            "duplicate",
            "---\nname: first\nname: second\ndescription: text\n---\n",
        ),
        (
            "missing",
            "# Skill with no metadata\nDo not infer a summary from this body.\n",
        ),
        ("unterminated", "---\nname: unfinished\ndescription: text\n"),
        (
            "escape",
            "---\nname: escape\ndescription: \"unsupported\\N\"\n---\n",
        ),
    ];
    for (name, content) in fixtures {
        f.write(&format!("skills/{name}/SKILL.md"), content);
    }
    f.write("skills/nonutf8/SKILL.md", [0xff, 0xfe]);
    let index = f
        .call("ssmcp.skills.list", json!({"paths":["skills"]}))
        .await;
    assert_eq!(index["total"], 6);
    let diagnostics = codes(&index);
    for code in [
        "unsupported_yaml",
        "duplicate_metadata_key",
        "missing_frontmatter",
        "unterminated_frontmatter",
        "invalid_utf8",
    ] {
        assert!(diagnostics.contains(&code), "{code}");
    }
    let duplicate = index["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["path"] == "skills/duplicate/SKILL.md")
        .unwrap();
    assert!(duplicate["name"].is_null());
    assert_eq!(duplicate["metadata_status"], "invalid");
    let raw = f
        .call("ssmcp.skills.read", json!({"path":"skills/array/SKILL.md"}))
        .await;
    assert_eq!(raw["content"], fixtures[0].1);
    assert!(
        f.context
            .perform(
                "ssmcp.skills.read",
                json!({"path":"skills/nonutf8/SKILL.md"})
            )
            .await
            .unwrap_err()
            .contains("UTF-8")
    );
}

#[tokio::test]
async fn discovery_is_bounded_and_rejects_invalid_roots_and_limits() {
    let f = Fixture::new();
    f.write(
        "skills/real/SKILL.md",
        "---\nname: real\ndescription: safe\n---\n",
    );
    let index = f
        .call(
            "ssmcp.skills.list",
            json!({"paths":["skills","skills/real"]}),
        )
        .await;
    assert_eq!(index["total"], 1);
    let shallow = f
        .call(
            "ssmcp.skills.list",
            json!({"paths":["skills"],"max_depth":0}),
        )
        .await;
    assert_eq!(shallow["truncated"], true);
    assert!(codes(&shallow).contains(&"depth_limit"));
    let limited = f
        .call(
            "ssmcp.skills.list",
            json!({"paths":["skills"],"max_nodes":1}),
        )
        .await;
    assert!(limited["limits"]["visited_nodes"].as_u64().unwrap() <= 1);
    assert_eq!(limited["truncated"], true);
    let absent = f
        .call("ssmcp.skills.list", json!({"paths":["missing"]}))
        .await;
    assert_eq!(absent["total"], 0);
    assert_eq!(absent["truncated"], false);
    assert_eq!(absent["sources"][0]["status"], "missing");
    for path in ["../extra", "/etc", "skills/../../extra"] {
        assert!(
            f.context
                .perform("ssmcp.skills.list", json!({"paths":[path]}))
                .await
                .is_err()
        );
    }

    for args in [
        json!({"max_nodes":4097}),
        json!({"max_depth":9}),
        json!({"limit":65}),
        json!({"paths":[]}),
        json!({"root":"unknown"}),
        json!({"unrecognized":true}),
    ] {
        assert!(f.context.perform("ssmcp.skills.list", args).await.is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn discovery_never_follows_directory_or_skill_symlinks() {
    let f = Fixture::new();
    f.write(
        "skills/real/SKILL.md",
        "---\nname: real\ndescription: safe\n---\n",
    );
    fs::write(
        f.extra.join("SKILL.md"),
        "---\nname: secret\ndescription: OUTSIDE_SECRET\n---\n",
    )
    .unwrap();
    symlink(&f.extra, f.root.join("skills/linked")).unwrap();
    fs::create_dir_all(f.root.join("skills/link-file")).unwrap();
    symlink(
        f.extra.join("SKILL.md"),
        f.root.join("skills/link-file/SKILL.md"),
    )
    .unwrap();
    let index = f
        .call(
            "ssmcp.skills.list",
            json!({"paths":["skills","skills/real"]}),
        )
        .await;
    assert_eq!(index["total"], 1);
    assert!(codes(&index).contains(&"symlink_skipped"));
    assert!(!index.to_string().contains("OUTSIDE_SECRET"));
    assert!(
        f.context
            .perform(
                "ssmcp.skills.read",
                json!({"path":"skills/link-file/SKILL.md"})
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn agents_resolve_orders_scopes_preserves_exact_text_and_requires_explicit_extra_root() {
    let f = Fixture::new();
    f.write("AGENTS.md", "root\r\n保留原文\r\n");
    f.write("agents.md", "ignored root fallback");
    f.write("pkg/AGENTS.md", "package\n");
    f.write("pkg/app/AGENTS.override.md", "override\n");
    f.write("pkg/app/AGENTS.md", "ignored upper\n");
    f.write("pkg/app/src/AGENTS.override.md", "");
    f.write("pkg/app/src/agents.md", "lowercase fallback\n");
    f.write("pkg/other/AGENTS.md", "SIBLING_SECRET");
    fs::write(f.extra.join("AGENTS.md"), "GLOBAL_EXPLICIT").unwrap();
    let result = f
        .call(
            "ssmcp.agents.resolve",
            json!({"path":"pkg/app/src/new/file.rs"}),
        )
        .await;
    assert_eq!(result["target_exists"], false);
    assert_eq!(result["files"].as_array().unwrap().len(), 4);
    let paths: Vec<_> = result["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec![
            "AGENTS.md",
            "pkg/AGENTS.md",
            "pkg/app/AGENTS.override.md",
            "pkg/app/src/agents.md"
        ]
    );
    assert_eq!(result["files"][0]["content"], "root\r\n保留原文\r\n");
    assert!(codes(&result).contains(&"empty_instruction"));
    assert!(!result.to_string().contains("SIBLING_SECRET"));
    assert!(!result.to_string().contains("GLOBAL_EXPLICIT"));
    let listed = f
        .call("ssmcp.agents.list", json!({"path":"pkg/app/src"}))
        .await;
    assert!(listed["files"][0].get("content").is_none());
    let extra = f
        .call(
            "ssmcp.agents.read",
            json!({"root":"context-1","path":"AGENTS.md"}),
        )
        .await;
    assert_eq!(extra["content"], "GLOBAL_EXPLICIT");
    assert_eq!(extra["root"]["id"], "context-1");
    let roots = f.call("ssmcp.context.roots", json!({})).await;
    assert_eq!(roots["roots"][1]["path"], f.extra.to_str().unwrap());
    assert_eq!(roots["roots"][1]["read_only"], true);
    #[cfg(unix)]
    {
        symlink(&f.extra, f.root.join("outside")).unwrap();
        assert!(
            f.context
                .perform("ssmcp.agents.resolve", json!({"path":"outside/new.rs"}))
                .await
                .is_err()
        );
    }
    for (name, path) in [
        ("ssmcp.agents.read", "secret.txt"),
        ("ssmcp.skills.read", "AGENTS.md"),
        ("ssmcp.agents.read", "../extra/AGENTS.md"),
    ] {
        assert!(f.context.perform(name, json!({"path":path})).await.is_err());
    }
    assert!(Context::new(f.root.clone(), vec![f.extra.clone(); 9]).is_err());
}

#[tokio::test]
async fn document_and_metadata_budgets_fail_explicitly_without_partial_instructions() {
    let f = Fixture::new();
    f.write(
        "skills/utf8/SKILL.md",
        "---\nname: utf8\ndescription: 文本\n---\n中文\n",
    );
    let error = f
        .context
        .perform(
            "ssmcp.skills.read",
            json!({"path":"skills/utf8/SKILL.md","max_bytes":1}),
        )
        .await
        .unwrap_err();
    assert!(error.contains("full-document limit"));
    f.write("AGENTS.md", "x".repeat(128 * 1024));
    f.write("pkg/AGENTS.md", "y".repeat(128 * 1024 + 1));
    assert_eq!(
        f.call("ssmcp.agents.list", json!({"path":"pkg/new.rs"}))
            .await["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        f.context
            .perform("ssmcp.agents.resolve", json!({"path":"pkg/new.rs"}))
            .await
            .unwrap_err()
            .contains("aggregate limit")
    );
    let individual = f
        .call("ssmcp.agents.read", json!({"path":"pkg/AGENTS.md"}))
        .await;
    assert_eq!(individual["bytes"], 128 * 1024 + 1);
    for index in 0..65 {
        f.write(
            &format!("large/{index:02}/SKILL.md"),
            format!(
                "---\nname: huge\ndescription: {}\n---\n",
                "x".repeat(17 * 1024)
            ),
        );
    }
    let bounded = f
        .call("ssmcp.skills.list", json!({"paths":["large"],"limit":64}))
        .await;
    assert_eq!(bounded["truncated"], true);
    assert_eq!(bounded["limits"]["metadata_bytes"], 1024 * 1024);
    assert!(bounded["total"].as_u64().unwrap() <= 64);
    assert!(codes(&bounded).contains(&"frontmatter_limit"));
    assert!(serde_json::to_vec(&bounded).unwrap().len() < 1024 * 1024);
    assert!(
        f.context
            .perform(
                "ssmcp.skills.read",
                json!({"path":"skills/utf8/SKILL.md","max_bytes":262145})
            )
            .await
            .is_err()
    );
    for tool in Context::tools() {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
    }
}

#[tokio::test]
async fn overlapping_scan_paths_retain_their_own_depth_and_invalid_yaml_is_not_silently_used() {
    let f = Fixture::new();
    f.write(
        "skills/one/SKILL.md",
        "---\nname: one\ndescription: text\n---\n",
    );
    let result = f
        .call(
            "ssmcp.skills.list",
            json!({"paths":[".","skills"],"max_depth":1}),
        )
        .await;
    assert_eq!(result["total"], 1);
    assert_eq!(result["skills"][0]["path"], "skills/one/SKILL.md");
    let invalid = [
        (
            "header",
            "---\nname: header\ndescription: > garbage\n  should not become a summary\n---\n",
        ),
        (
            "separator",
            "---\nname: separator\ndescription:missing whitespace\n---\n",
        ),
        (
            "sequence",
            "---\nname: sequence\ndescription: - item\n---\n",
        ),
    ];
    for (name, contents) in invalid {
        f.write(&format!("bad/{name}/SKILL.md"), contents);
    }
    let result = f.call("ssmcp.skills.list", json!({"paths":["bad"]})).await;
    assert_eq!(result["total"], 3);
    for skill in result["skills"].as_array().unwrap() {
        assert_eq!(skill["metadata_status"], "invalid");
        assert!(skill["description"].is_null());
    }
    assert!(codes(&result).contains(&"unsupported_yaml"));
}

#[tokio::test]
async fn effective_skills_overlay_granted_roots_and_reveal_global_after_project_removal() {
    let f = Fixture::new();
    let global = f.extra.join(".agents/skills/shared/SKILL.md");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(
        &global,
        "---\nname: shared\ndescription: Global description\n---\nGLOBAL BODY\n",
    )
    .unwrap();
    let only = f.extra.join("skills/global-only/SKILL.md");
    fs::create_dir_all(only.parent().unwrap()).unwrap();
    fs::write(
        &only,
        "---\nname: global-only\ndescription: Global helper\n---\n",
    )
    .unwrap();
    f.write(
        "skills/shared/SKILL.md",
        "---\nname: shared\ndescription: Project description\n---\nPROJECT BODY\n",
    );
    f.write(
        ".agents/skills/shared/SKILL.md",
        "---\nname: shared\ndescription: Highest priority project\n---\nFIRST BODY\n",
    );

    let page = f
        .call("ssmcp.skills.list", json!({"scope":"effective"}))
        .await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["scope"], "effective");
    let shared = page["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "shared")
        .unwrap();
    assert_eq!(shared["root"]["id"], "workspace");
    assert_eq!(shared["scope"], "project");
    assert_eq!(shared["path"], ".agents/skills/shared/SKILL.md");
    assert_eq!(shared["description"], "Highest priority project");
    assert_eq!(shared["shadowed"].as_array().unwrap().len(), 2);
    assert_eq!(shared["shadowed"][1]["root"]["id"], "context-1");
    assert!(!page.to_string().contains("BODY"));
    let view = f.call("ssmcp.skills.view", json!({})).await;
    assert_eq!(view["scope"], "effective");
    assert_eq!(view["snapshot"], page["snapshot"]);
    assert!(view["markdown"].as_str().unwrap().contains("context-1"));
    assert!(
        view["markdown"]
            .as_str()
            .unwrap()
            .contains("Highest priority project")
    );
    assert!(view.get("skills").is_none());
    assert_eq!(
        f.call("ssmcp.skills.list", json!({})).await["scope"],
        "root"
    );
    assert_eq!(
        f.call("ssmcp.skills.view", json!({"root":"context-1"}))
            .await["scope"],
        "root"
    );

    fs::remove_file(f.root.join(".agents/skills/shared/SKILL.md")).unwrap();
    fs::remove_file(f.root.join("skills/shared/SKILL.md")).unwrap();
    let page = f
        .call("ssmcp.skills.list", json!({"scope":"effective"}))
        .await;
    let shared = page["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "shared")
        .unwrap();
    assert_eq!(shared["root"]["id"], "context-1");
    assert_eq!(shared["scope"], "global");
    assert_eq!(shared["shadowed"], json!([]));
    assert_eq!(
        f.call(
            "ssmcp.skills.read",
            json!({"root":shared["root"]["id"],"path":shared["path"]})
        )
        .await["content"],
        "---\nname: shared\ndescription: Global description\n---\nGLOBAL BODY\n"
    );
    fs::write(
        &global,
        "---\nname: shared\ndescription: Edited global\n---\nNEW BODY\n",
    )
    .unwrap();
    let current = f
        .call("ssmcp.skills.list", json!({"scope":"effective"}))
        .await;
    assert_ne!(current["snapshot"], page["snapshot"]);
    assert_eq!(current["skills"][1]["description"], "Edited global");
}

#[tokio::test]
async fn effective_cursor_rejects_changed_global_sources_and_root_scope() {
    let f = Fixture::new();
    f.write(
        "skills/a/SKILL.md",
        "---\nname: a\ndescription: Project\n---\n",
    );
    let path = f.extra.join("skills/b/SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "---\nname: b\ndescription: Global\n---\n").unwrap();
    let page = f
        .call("ssmcp.skills.list", json!({"scope":"effective","limit":1}))
        .await;
    let cursor = page["next_cursor"].clone();
    let next = f
        .call(
            "ssmcp.skills.list",
            json!({"scope":"effective","limit":1,"cursor":cursor}),
        )
        .await;
    assert_eq!(next["skills"][0]["name"], "b");
    assert_eq!(next["skills"][0]["root"]["id"], "context-1");
    assert!(
        f.context
            .perform("ssmcp.skills.list", json!({"cursor":cursor}))
            .await
            .unwrap_err()
            .contains("changed")
    );
    fs::write(&path, "---\nname: b\ndescription: Updated\n---\n").unwrap();
    assert!(
        f.context
            .perform(
                "ssmcp.skills.list",
                json!({"scope":"effective","cursor":cursor})
            )
            .await
            .unwrap_err()
            .contains("changed")
    );
    for args in [
        json!({"scope":"effective","root":"context-1"}),
        json!({"scope":"unrecognized"}),
        json!({"scope":"effective","paths":["../"]}),
        json!({"scope":"effective","limit":0}),
    ] {
        assert!(f.context.perform("ssmcp.skills.list", args).await.is_err());
    }
}

#[tokio::test]
async fn effective_views_only_use_granted_roots_and_preserve_invalid_metadata_diagnostics() {
    let f = Fixture::new();
    f.write(
        "skills/shared/SKILL.md",
        "---\nname: shared\ndescription: [unsupported]\n---\n",
    );
    let global = f.extra.join("skills/shared/SKILL.md");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(
        &global,
        "---\nname: shared\ndescription: Valid global\n---\n",
    )
    .unwrap();
    let outside = f.base.join("skills/outside/SKILL.md");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(
        &outside,
        "---\nname: outside\ndescription: Not granted\n---\n",
    )
    .unwrap();
    let page = f
        .call("ssmcp.skills.list", json!({"scope":"effective"}))
        .await;
    assert_eq!(page["total"], 2);
    assert!(
        page["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["description"] == "Valid global")
    );
    assert!(!page.to_string().contains("Not granted"));
    assert!(
        page["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["root"]["id"] == "workspace")
    );
    let partial = f
        .call(
            "ssmcp.skills.list",
            json!({"scope":"effective","paths":["skills"],"max_depth":0}),
        )
        .await;
    assert_eq!(partial["truncated"], true);
}

#[tokio::test]
async fn effective_global_roots_respect_configuration_order_and_custom_search_paths() {
    let f = Fixture::new();
    let later = f.base.join("later");
    fs::create_dir_all(later.join("custom/shared")).unwrap();
    fs::create_dir_all(f.extra.join("custom/shared")).unwrap();
    fs::write(
        f.extra.join("custom/shared/SKILL.md"),
        "---\nname: shared\ndescription: First configured global\n---\n",
    )
    .unwrap();
    fs::write(
        later.join("custom/shared/SKILL.md"),
        "---\nname: shared\ndescription: Later configured global\n---\n",
    )
    .unwrap();
    let context = Context::new(f.root.clone(), vec![f.extra.clone(), later]).unwrap();
    let page = context
        .perform(
            "ssmcp.skills.list",
            json!({"scope":"effective","paths":["custom"]}),
        )
        .await
        .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["skills"][0]["root"]["id"], "context-1");
    assert_eq!(page["skills"][0]["shadowed"][0]["root"]["id"], "context-2");
}

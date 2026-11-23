use super::*;
use std::{
    cell::Cell,
    ffi::OsStr,
    fs::{self, FileTimes, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const PROJECT_MODE: &str = "project_rules = true\n";
const OP: &str = "ssmcp.fs.write_text";

fn rule(action: &str) -> String {
    format!("[operations]\n\"{OP}\" = \"{action}\"\n")
}

fn put(path: &Path, source: impl AsRef<[u8]>) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    baseline: PathBuf,
    project: PathBuf,
    workspace: super::super::Workspace,
    next: Cell<Instant>,
}

impl Fixture {
    fn new() -> Self {
        let mut token = [0u8; 8];
        crate::platform::random_bytes(&mut token).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-safety-rules-{:016x}",
            u64::from_ne_bytes(token)
        ));
        crate::platform::private_directory(&root).unwrap();
        for name in ["home", "workspace"] {
            let base = root.join(name);
            for dir in [&base, &base.join(".ssmcp"), &base.join(".ssmcp/safety")] {
                crate::platform::private_directory(dir).unwrap();
            }
        }
        Self {
            home: root.join("home"),
            baseline: root.join("home/.ssmcp/safety/baseline.toml"),
            project: root.join("workspace/.ssmcp/safety/rules.toml"),
            workspace: super::super::Workspace::new(root.join("workspace")).unwrap(),
            root,
            // Native effects in these tests use the injected snapshot instead
            // of reading the test runner's HOME. No process environment changes.
            next: Cell::new(Instant::now() + Duration::from_secs(3600)),
        }
    }

    fn load(&self) -> Result<Option<rules::Rules>, rules::LoadError> {
        linux::load_at(Some(&self.home), &self.workspace.root.directory)
    }

    fn refresh_with(
        &self,
        load: impl FnOnce() -> Result<Option<rules::Rules>, rules::LoadError>,
    ) -> Snapshot {
        let now = self.next.get();
        self.next.set(now + Duration::from_secs(2));
        self.workspace.root.safety_rules.refresh(now, load)
    }

    fn refresh(&self) -> Snapshot {
        self.refresh_with(|| self.load())
    }

    fn is_allowed(&self, snapshot: Snapshot) -> bool {
        authorize(OP, &json!({"path":"fixture.txt"}), snapshot).is_allowed()
    }

    async fn write(&self, path: &str) -> Result<Value, String> {
        self.workspace
            .perform(
                OP,
                json!({
                    "path":path, "text":"synthetic-private-value", "create_parents":true
                }),
            )
            .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn actual_guard_matches_independent_decision_matrix() {
    use Decision::{Allow, Deny, RequireApproval};
    let names = ["allow", "require_approval", "deny"];
    let reference = [
        [Allow, RequireApproval, Deny],
        [RequireApproval, RequireApproval, Deny],
        [Deny, Deny, Deny],
    ];
    for (i, baseline) in names.into_iter().enumerate() {
        for (j, project) in names.into_iter().enumerate() {
            let baseline = format!("{PROJECT_MODE}{}", rule(baseline));
            let rules = rules::parse(&baseline, || Ok(rule(project))).unwrap();
            let snapshot = Snapshot {
                rules,
                ..Snapshot::default()
            };
            assert_eq!(
                authorize(OP, &json!({"path":"fixture"}), snapshot).decision,
                reference[i][j],
                "{i}, {j}"
            );
            assert_eq!(
                authorize(OP, &json!({"path":"../fixture"}), snapshot).decision,
                Deny
            );
        }
    }
}

#[tokio::test]
async fn absent_baseline_preserves_native_paths_with_missing_or_writable_home() {
    let fixture = Fixture::new();
    put(&fixture.project, rule("deny"));
    for home in [
        None,
        Some(fixture.root.join("missing-home")),
        Some(fixture.home.clone()),
    ] {
        fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o770)).unwrap();
        let snapshot = fixture
            .refresh_with(|| linux::load_at(home.as_deref(), &fixture.workspace.root.directory));
        assert_eq!(snapshot.state, "builtin");
        assert!(!snapshot.deny_all);
        for operation in OPERATIONS {
            assert!(
                authorize(
                    operation,
                    &json!({"path":".ssmcp/safety/ordinary.toml"}),
                    snapshot
                )
                .is_allowed()
            );
        }
    }
    fixture.write(".ssmcp/safety/ordinary.toml").await.unwrap();
    fixture.write(".SSMCP/SAFETY/ordinary.toml").await.unwrap();
    assert!(
        fixture
            .root
            .join("workspace/.ssmcp/safety/ordinary.toml")
            .is_file()
    );
    assert!(
        fixture
            .root
            .join("workspace/.SSMCP/SAFETY/ordinary.toml")
            .is_file()
    );
}

const UNDISCOVERABLE_HOME: [&str; 9] = [
    "unset",
    "missing",
    "home_symlink",
    "ancestor_symlink",
    "dangling_home_symlink",
    "config_symlink",
    "safety_symlink",
    "home_file",
    "config_file",
];

fn undiscoverable_home(fixture: &Fixture, shape: &str) -> Option<PathBuf> {
    let linked = fixture.root.join("linked");
    match shape {
        "unset" => return None,
        "missing" => return Some(fixture.root.join("missing")),
        "home_symlink" => {
            symlink(&fixture.home, &linked).unwrap();
            return Some(linked);
        }
        "ancestor_symlink" => {
            symlink(&fixture.root, &linked).unwrap();
            return Some(linked.join("home"));
        }
        "dangling_home_symlink" => {
            symlink(fixture.root.join("missing"), &linked).unwrap();
            return Some(linked);
        }
        "home_file" => {
            put(&linked, "");
            return Some(linked);
        }
        "config_symlink" | "config_file" | "safety_symlink" => {
            let path = if shape == "safety_symlink" {
                fixture.home.join(".ssmcp/safety")
            } else {
                fixture.home.join(".ssmcp")
            };
            let displaced = fixture.root.join("displaced");
            fs::rename(&path, &displaced).unwrap();
            if shape == "config_file" {
                put(&path, "");
            } else {
                symlink(displaced, &path).unwrap();
            }
        }
        _ => panic!("unknown discovery fixture: {shape}"),
    }
    Some(fixture.home.clone())
}

async fn assert_native_mutations(fixture: &Fixture, allowed: bool) {
    let root = fixture.root.join("workspace");
    put(&root.join("preexisting.txt"), "before");
    let requests = [
        (
            "ssmcp.fs.write",
            json!({"path":".ssmcp/safety/discovery/binary","data_base64":"eA==","create_parents":true}),
        ),
        (
            OP,
            json!({"path":".SSMCP/SAFETY/discovery/text","text":"after","create_parents":true}),
        ),
        (
            "ssmcp.fs.edit_text",
            json!({"path":"preexisting.txt","old_text":"before","new_text":"after"}),
        ),
        (
            "ssmcp.fs.mkdir",
            json!({"path":"created/directory","parents":true}),
        ),
    ];
    for (operation, args) in requests {
        let result = fixture.workspace.perform(operation, args).await;
        if allowed {
            assert!(result.is_ok(), "{operation}: {result:?}");
        } else {
            assert_eq!(
                result.unwrap_err(),
                "native file mutation blocked by safety policy",
                "{operation}"
            );
        }
    }
    assert_eq!(
        fs::read_to_string(root.join("preexisting.txt")).unwrap(),
        if allowed { "after" } else { "before" }
    );
    if allowed {
        assert_eq!(
            fs::read(root.join(".ssmcp/safety/discovery/binary")).unwrap(),
            b"x"
        );
        assert_eq!(
            fs::read_to_string(root.join(".SSMCP/SAFETY/discovery/text")).unwrap(),
            "after"
        );
        assert!(root.join("created/directory").is_dir());
    } else {
        for path in [".ssmcp/safety/discovery", ".SSMCP", "created"] {
            assert!(!root.join(path).exists(), "{path}");
        }
    }
}

#[tokio::test]
async fn cold_discovery_failures_preserve_all_native_mutations() {
    for shape in UNDISCOVERABLE_HOME {
        for hidden_baseline in [false, true] {
            let fixture = Fixture::new();
            put(&fixture.project, rule("deny"));
            if hidden_baseline {
                put(&fixture.baseline, rule("deny"));
            }
            let home = undiscoverable_home(&fixture, shape);
            let snapshot = fixture.refresh_with(|| {
                linux::load_at(home.as_deref(), &fixture.workspace.root.directory)
            });
            assert_eq!(snapshot.state, "builtin", "{shape}: {hidden_baseline}");
            assert!(!snapshot.deny_all, "{shape}: {hidden_baseline}");
            assert_native_mutations(&fixture, true).await;
        }
    }
}

#[tokio::test]
async fn active_policy_stays_closed_when_discovery_becomes_unavailable() {
    for shape in UNDISCOVERABLE_HOME {
        let fixture = Fixture::new();
        put(&fixture.baseline, rule("allow"));
        let active = fixture.refresh();
        assert_eq!(active.state, "active");
        let home = undiscoverable_home(&fixture, shape);
        let unavailable = fixture
            .refresh_with(|| linux::load_at(home.as_deref(), &fixture.workspace.root.directory));
        assert_eq!(unavailable.state, "active_unavailable", "{shape}");
        assert!(unavailable.deny_all, "{shape}");
        assert!(unavailable.revision > active.revision, "{shape}");
        assert_native_mutations(&fixture, false).await;
    }
}

#[tokio::test]
async fn non_utf8_home_can_select_its_real_baseline() {
    let mut fixture = Fixture::new();
    let home = fixture.root.join(OsStr::from_bytes(b"home-\xff"));
    fs::rename(&fixture.home, &home).unwrap();
    fixture.home = home;
    fixture.baseline = fixture.home.join(".ssmcp/safety/baseline.toml");
    assert!(fixture.home.to_str().is_none());
    let absent = fixture.refresh();
    assert_eq!(absent.state, "builtin");
    assert!(!absent.deny_all);
    assert_native_mutations(&fixture, true).await;

    put(&fixture.baseline, rule("deny"));
    let active = fixture.refresh();
    assert_eq!(active.state, "active");
    assert_eq!(active.rules.decisions(OP), [Some(Decision::Deny), None]);
    assert!(!fixture.is_allowed(active));
    assert!(fixture.write("denied/data").await.is_err());
    assert!(!fixture.root.join("workspace/denied").exists());
}

#[tokio::test]
async fn selected_baseline_disappearance_does_not_become_absence_in_the_same_load() {
    let fixture = Fixture::new();
    put(&fixture.baseline, "");
    let mut discovered = false;
    let failed = fixture.refresh_with(|| {
        linux::load_at_with_hooks(
            Some(&fixture.home),
            &fixture.workspace.root.directory,
            &mut || {
                // Keep the selected inode linked so its metadata remains valid;
                // opening the original name must now fail after selection.
                fs::rename(&fixture.baseline, fixture.root.join("selected-away.toml")).unwrap();
                discovered = true;
            },
            &mut |_| {},
        )
    });
    assert!(discovered);
    assert_eq!(failed.state, "configured_unavailable");
    assert!(failed.deny_all);
    assert_native_mutations(&fixture, false).await;

    let absent = fixture.refresh();
    assert_eq!(absent.state, "builtin");
    assert!(!absent.deny_all);
    assert_native_mutations(&fixture, true).await;
}

#[tokio::test]
async fn explicit_approval_blocks_all_native_mutations() {
    let fixture = Fixture::new();
    let source = format!(
        "[operations]\n{}",
        OPERATIONS
            .map(|operation| format!("\"{operation}\" = \"require_approval\"\n"))
            .join("")
    );
    put(&fixture.baseline, source);
    let snapshot = fixture.refresh();
    assert_eq!(snapshot.state, "active");
    assert!(!snapshot.deny_all);
    for operation in OPERATIONS {
        let verdict = authorize(operation, &json!({"path":"ordinary/file"}), snapshot);
        assert_eq!(verdict.decision, Decision::RequireApproval);
        assert!(!verdict.is_allowed(), "{operation}");
    }
    assert_native_mutations(&fixture, false).await;
}

#[tokio::test]
async fn selected_baseline_replacement_cannot_change_the_pinned_candidate() {
    let fixture = Fixture::new();
    put(&fixture.baseline, rule("deny"));
    let mut replaced = false;
    let snapshot = fixture.refresh_with(|| {
        linux::load_at_with_hooks(
            Some(&fixture.home),
            &fixture.workspace.root.directory,
            &mut || {
                // Preserve the selected inode, then put an independently valid
                // permissive file at its old name. The read fd must not replace
                // the candidate's identity even when both files are trusted.
                fs::rename(&fixture.baseline, fixture.root.join("selected-away.toml")).unwrap();
                put(&fixture.baseline, "");
                replaced = true;
            },
            &mut |_| {},
        )
    });
    assert!(replaced);
    assert_eq!(snapshot.state, "configured_unavailable");
    assert!(snapshot.deny_all);
    assert_native_mutations(&fixture, false).await;

    let replacement = fixture.refresh();
    assert_eq!(replacement.state, "active");
    assert!(!replacement.deny_all);
    assert_native_mutations(&fixture, true).await;
}

#[test]
fn searchable_unreadable_directories_still_discover_the_baseline() {
    for directory in ["home", "home/.ssmcp", "home/.ssmcp/safety"] {
        let fixture = Fixture::new();
        put(&fixture.baseline, rule("deny"));
        let path = fixture.root.join(directory);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o100)).unwrap();
        let snapshot = fixture.refresh();
        // Restore traversal/read permissions before any assertion or cleanup.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(snapshot.state, "active", "{directory}");
        assert!(!fixture.is_allowed(snapshot), "{directory}");
    }
}

#[test]
fn discovered_baseline_keeps_directory_trust_failures_closed() {
    for directory in ["home", "home/.ssmcp", "home/.ssmcp/safety"] {
        let fixture = Fixture::new();
        put(&fixture.baseline, "");
        fs::set_permissions(
            fixture.root.join(directory),
            fs::Permissions::from_mode(0o770),
        )
        .unwrap();
        let snapshot = fixture.refresh();
        assert_eq!(snapshot.state, "configured_unavailable", "{directory}");
        assert!(snapshot.deny_all, "{directory}");
    }
}

#[test]
fn project_cannot_activate_itself_and_unselected_project_is_not_read() {
    let fixture = Fixture::new();
    put(&fixture.project, rule("deny"));
    assert!(fixture.load().unwrap().is_none());
    put(&fixture.baseline, "");
    put(&fixture.project, "invalid project");
    let snapshot = fixture.refresh();
    assert_eq!(snapshot.state, "active");
    assert!(fixture.is_allowed(snapshot));
    fs::remove_file(&fixture.project).unwrap();
    symlink(&fixture.baseline, &fixture.project).unwrap();
    assert!(fixture.is_allowed(fixture.refresh()));
}

#[tokio::test]
async fn cold_error_then_absence_recovers_without_a_sticky_denial() {
    let fixture = Fixture::new();
    put(&fixture.baseline, "not valid toml!");
    let failed = fixture.refresh();
    assert_eq!(failed.state, "configured_unavailable");
    assert!(!fixture.is_allowed(failed));
    assert!(fixture.write("blocked/file").await.is_err());
    assert!(!fixture.root.join("workspace/blocked").exists());

    fs::remove_file(&fixture.baseline).unwrap();
    let absent = fixture.refresh();
    assert_eq!(absent.state, "builtin");
    assert!(absent.revision > failed.revision);
    assert!(fixture.is_allowed(absent));
    fixture.write("restored/file").await.unwrap();
}

#[tokio::test]
async fn stricter_user_baseline_cannot_be_masked_by_a_broken_project() {
    for failure in ["missing", "malformed", "symlink"] {
        let fixture = Fixture::new();
        put(
            &fixture.baseline,
            format!("{PROJECT_MODE}{}", rule("allow")),
        );
        put(&fixture.project, rule("allow"));
        let active = fixture.refresh();
        assert!(fixture.is_allowed(active));
        fixture.write("before.txt").await.unwrap();

        put(&fixture.baseline, format!("{PROJECT_MODE}{}", rule("deny")));
        fs::remove_file(&fixture.project).unwrap();
        match failure {
            "malformed" => put(&fixture.project, "invalid project!"),
            "symlink" => symlink(&fixture.baseline, &fixture.project).unwrap(),
            _ => (),
        }
        let unavailable = fixture.refresh();
        assert_eq!(unavailable.state, "active_unavailable", "{failure}");
        assert!(unavailable.revision > active.revision);
        assert_eq!(
            unavailable.rules.decisions(OP),
            [Some(Decision::Deny), None]
        );
        for operation in OPERATIONS {
            assert!(
                !authorize(operation, &json!({"path":"blocked/data"}), unavailable).is_allowed()
            );
        }
        assert!(fixture.write("blocked/data").await.is_err());
        assert!(!fixture.root.join("workspace/blocked").exists());

        if failure != "missing" {
            fs::remove_file(&fixture.project).unwrap();
        }
        put(&fixture.project, rule("allow"));
        let repaired = fixture.refresh();
        assert_eq!(repaired.state, "active");
        assert!(!repaired.deny_all);
        assert!(!fixture.is_allowed(repaired));
    }
}

#[test]
fn active_policy_disappearance_or_lost_trust_requires_a_valid_replacement() {
    for action in ["allow", "deny"] {
        let fixture = Fixture::new();
        put(&fixture.baseline, rule(action));
        let active = fixture.refresh();
        assert_eq!(active.state, "active");

        let no_home =
            fixture.refresh_with(|| linux::load_at(None, &fixture.workspace.root.directory));
        assert_eq!(no_home.state, "active_unavailable");
        assert!(!fixture.is_allowed(no_home));
        assert_eq!(fixture.refresh().state, "active");

        fs::remove_file(&fixture.baseline).unwrap();
        let missing = fixture.refresh();
        assert_eq!(missing.state, "active_unavailable");
        assert!(!fixture.is_allowed(missing));
        assert!(missing.revision > active.revision);

        put(&fixture.baseline, "");
        let empty = fixture.refresh();
        assert_eq!(empty.state, "active");
        assert!(fixture.is_allowed(empty));
        assert!(empty.revision > missing.revision);

        fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o770)).unwrap();
        let untrusted = fixture.refresh();
        assert_eq!(untrusted.state, "active_unavailable");
        assert!(!fixture.is_allowed(untrusted));
        fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(fixture.is_allowed(fixture.refresh()));

        put(&fixture.baseline, "invalid baseline!");
        assert!(!fixture.is_allowed(fixture.refresh()));
        put(&fixture.baseline, "");
        assert!(fixture.is_allowed(fixture.refresh()));
    }
}

fn replace_in_place_with_deny(path: &Path) {
    let original = fs::read_to_string(path).unwrap();
    let replacement = original.replace("\"allow\"", "\"deny\" ");
    assert_eq!(original.len(), replacement.len());
    assert_ne!(original, replacement);
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(replacement.as_bytes()).unwrap();
    // Force a different mtime so the test does not depend on clock resolution.
    file.set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)))
        .unwrap();
}

#[test]
fn in_place_policy_content_and_metadata_changes_are_rejected_after_read() {
    for layer in ["baseline.toml", "rules.toml"] {
        for change in ["permissions", "same_length_content"] {
            let fixture = Fixture::new();
            put(
                &fixture.baseline,
                format!("{PROJECT_MODE}{}", rule("allow")),
            );
            put(&fixture.project, rule("allow"));
            assert!(fixture.is_allowed(fixture.refresh()));
            let path = if layer == "baseline.toml" {
                &fixture.baseline
            } else {
                &fixture.project
            };
            let mut changed = false;
            let unavailable = fixture.refresh_with(|| {
                linux::load_at_with(
                    Some(&fixture.home),
                    &fixture.workspace.root.directory,
                    &mut |name| {
                        if name == layer {
                            if change == "permissions" {
                                fs::set_permissions(path, fs::Permissions::from_mode(0o660))
                                    .unwrap();
                            } else {
                                replace_in_place_with_deny(path);
                            }
                            changed = true;
                        }
                    },
                )
            });
            assert!(changed);
            assert_eq!(unavailable.state, "active_unavailable", "{layer}: {change}");
            assert!(!fixture.is_allowed(unavailable), "{layer}: {change}");
        }
    }
}

#[test]
fn baseline_is_revalidated_after_the_subordinate_project_read() {
    let fixture = Fixture::new();
    put(
        &fixture.baseline,
        format!("{PROJECT_MODE}{}", rule("allow")),
    );
    put(&fixture.project, rule("allow"));
    let mut changed = false;
    let result = linux::load_at_with(
        Some(&fixture.home),
        &fixture.workspace.root.directory,
        &mut |name| {
            if name == "rules.toml" {
                replace_in_place_with_deny(&fixture.baseline);
                changed = true;
            }
        },
    );
    assert!(changed);
    assert!(
        result.unwrap_err().baseline.is_none(),
        "changed baseline is not trusted evidence"
    );
}

#[test]
fn directory_trust_is_rechecked_after_the_policy_read() {
    for directory in ["home", "home/.ssmcp", "workspace/.ssmcp/safety"] {
        let fixture = Fixture::new();
        put(
            &fixture.baseline,
            format!("{PROJECT_MODE}{}", rule("allow")),
        );
        put(&fixture.project, rule("allow"));
        let layer = if directory.starts_with("home") {
            "baseline.toml"
        } else {
            "rules.toml"
        };
        let result = linux::load_at_with(
            Some(&fixture.home),
            &fixture.workspace.root.directory,
            &mut |name| {
                if name == layer {
                    fs::set_permissions(
                        fixture.root.join(directory),
                        fs::Permissions::from_mode(0o770),
                    )
                    .unwrap();
                }
            },
        );
        assert!(result.is_err(), "{directory}");
    }
}

#[tokio::test]
async fn rule_sources_must_be_regular_files() {
    let fixture = Fixture::new();
    let name = std::ffi::CString::new(fixture.baseline.as_os_str().as_bytes()).unwrap();
    // SAFETY: the fixture path is a valid C string in a private directory.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let rejected = fixture.refresh();
    assert_eq!(rejected.state, "configured_unavailable");
    assert!(rejected.deny_all);
    assert_native_mutations(&fixture, false).await;

    fs::remove_file(&fixture.baseline).unwrap();
    put(&fixture.baseline, rule("deny"));
    let regular = fixture.refresh();
    assert_eq!(regular.state, "active");
    assert!(!regular.deny_all);
    assert!(!fixture.is_allowed(regular));
}

#[test]
fn policy_files_remain_bounded_regular_single_link_and_no_follow() {
    for invalid in [
        "symlink",
        "dangling_symlink",
        "permissions",
        "hardlink",
        "directory",
        "oversize",
        "invalid_utf8",
    ] {
        let fixture = Fixture::new();
        match invalid {
            "symlink" => {
                put(&fixture.project, "");
                symlink(&fixture.project, &fixture.baseline).unwrap();
            }
            "dangling_symlink" => {
                symlink(fixture.root.join("missing"), &fixture.baseline).unwrap();
            }
            "permissions" => {
                put(&fixture.baseline, "");
                fs::set_permissions(&fixture.baseline, fs::Permissions::from_mode(0o660)).unwrap();
            }
            "hardlink" => {
                put(&fixture.baseline, "");
                fs::hard_link(&fixture.baseline, fixture.home.join("other-link")).unwrap();
            }
            "directory" => {
                fs::create_dir(&fixture.baseline).unwrap();
            }
            "oversize" => put(&fixture.baseline, format!("#{}\n", "a".repeat(16 * 1024))),
            "invalid_utf8" => put(&fixture.baseline, [0xff]),
            _ => unreachable!(),
        }
        let snapshot = fixture.refresh();
        assert_eq!(snapshot.state, "configured_unavailable", "{invalid}");
        assert!(snapshot.deny_all, "{invalid}");
    }
    let fixture = Fixture::new();
    put(
        &fixture.baseline,
        format!("#{}\n", "a".repeat(16 * 1024 - 2)),
    );
    assert!(fixture.is_allowed(fixture.refresh()));
}

#[tokio::test]
async fn safety_decisions_and_successful_file_audit_keep_separate_cursors() {
    let fixture = Fixture::new();
    fixture.refresh();
    assert!(fixture.write("../synthetic-private-path").await.is_err());
    fixture.write(".ssmcp/safety/ordinary.toml").await.unwrap();
    let safety = fixture
        .workspace
        .perform("ssmcp.fs.events", json!({"stream":"safety"}))
        .await
        .unwrap();
    let rows = safety["events"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["event"]["decision"], "deny");
    assert_eq!(rows[0]["event"]["outcome"], "blocked");
    assert_eq!(rows[1]["event"]["decision"], "allow");
    assert_eq!(rows[1]["event"]["outcome"], "succeeded");
    for omitted in ["synthetic-private", "ordinary.toml", "../"] {
        assert!(!safety.to_string().contains(omitted));
    }
    let files = fixture
        .workspace
        .perform("ssmcp.fs.events", json!({}))
        .await
        .unwrap();
    let file_rows = files["events"].as_array().unwrap();
    assert!(!file_rows.is_empty());
    assert!(
        file_rows
            .iter()
            .all(|row| row["event"]["type"] != "ssmcp.safety.decision")
    );
    assert!(
        file_rows
            .iter()
            .any(|row| row["event"]["path"] == ".ssmcp/safety/ordinary.toml")
    );
}

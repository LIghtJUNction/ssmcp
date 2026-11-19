//! Opt-in skill sources. Global directories grant only skills, never the user's home.
use super::*;
use std::{collections::BTreeSet, fs};

pub(crate) const BUILTIN_SSMCP_SKILL: &str = include_str!("../../skills/ssmcp/SKILL.md");
const MAX_ROOTS: usize = 256;
const MAX_DISCOVERY_NODES: usize = 4096;
const MAX_GROUP_DEPTH: usize = 8;

pub(super) struct Sources {
    pub roots: Vec<ContextRoot>,
    pub diagnostics: Vec<Value>,
    pub truncated: bool,
}

pub(super) fn default_paths() -> (Vec<PathBuf>, Option<PathBuf>) {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let mut paths = Vec::new();
    if let Some(home) = &home {
        paths.push(home.join(".agents/skills"));
    }
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".codex")));
    if let Some(path) = codex.filter(|path| path.is_absolute()) {
        paths.push(path.join("skills"));
    }
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".claude")));
    if let Some(path) = claude.filter(|path| path.is_absolute()) {
        paths.push(path.join("skills"));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    if let Some(path) = config.filter(|path| path.is_absolute()) {
        paths.push(path.join("ssmcp/skills"));
    }
    if let Some(home) = &home {
        paths.push(home.join(".ssmcp/skills"));
    }
    (paths, home)
}

impl Sources {
    pub fn discover(
        paths: Vec<PathBuf>,
        home: Option<PathBuf>,
        output: Arc<crate::output::OutputStore>,
    ) -> Result<Self, String> {
        if paths.len() > 16 {
            return Err("at most 16 global skill directories are allowed".into());
        }
        let home = home.and_then(|path| fs::canonicalize(path).ok());
        let mut sources = Self {
            roots: Vec::new(),
            diagnostics: Vec::new(),
            truncated: false,
        };
        let mut granted = BTreeMap::new();
        for path in paths {
            if !path.is_absolute() {
                sources.note(
                    &path,
                    "invalid_global_skill_root",
                    "Global skill directories must be absolute",
                    false,
                );
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    sources.note(
                        &path,
                        "unreadable_global_skill_root",
                        "Cannot inspect this global skill directory",
                        true,
                    );
                    continue;
                }
            };
            if !canonical.is_dir() || broad_root(&canonical, home.as_deref()) {
                sources.note(
                    &path,
                    "invalid_global_skill_root",
                    "A skill source must be a directory below the home and filesystem roots",
                    false,
                );
                continue;
            }
            if !sources.grant(&canonical, &path, "global_skill_dir", &output, &mut granted) {
                continue;
            }
            // Installed skill aliases may point outside their manager's directory.
            // Grant only the exact target package containing a regular SKILL.md.
            let mut pending = vec![(canonical, 0usize)];
            let mut visited = BTreeSet::new();
            let mut nodes = 0;
            while let Some((directory, depth)) = pending.pop() {
                if !visited.insert(directory.clone()) || regular_skill(&directory) {
                    continue;
                }
                let entries = match fs::read_dir(&directory) {
                    Ok(entries) => entries.collect::<Result<Vec<_>, _>>(),
                    Err(error) => Err(error),
                };
                let mut entries = match entries {
                    Ok(entries) => entries,
                    Err(_) => {
                        sources.note(
                            &directory,
                            "unreadable_skill_group",
                            "Cannot enumerate a skill group",
                            true,
                        );
                        continue;
                    }
                };
                entries.sort_by_key(|entry| entry.file_name());
                let mut children = Vec::new();
                for entry in entries {
                    nodes += 1;
                    if nodes > MAX_DISCOVERY_NODES {
                        sources.note(
                            &directory,
                            "skill_source_node_limit",
                            "Global skill alias discovery reached its bounded node limit",
                            true,
                        );
                        pending.clear();
                        break;
                    }
                    let path = entry.path();
                    let Ok(kind) = entry.file_type() else {
                        sources.note(
                            &path,
                            "unreadable_skill_entry",
                            "Cannot inspect a skill directory entry",
                            true,
                        );
                        continue;
                    };
                    if kind.is_symlink() {
                        let target = match fs::canonicalize(&path) {
                            Ok(target) => target,
                            Err(_) => {
                                sources.note(
                                    &path,
                                    "unreadable_skill_alias",
                                    "Skill alias target is unavailable",
                                    false,
                                );
                                continue;
                            }
                        };
                        if !broad_root(&target, home.as_deref()) && regular_skill(&target) {
                            sources.grant(
                                &target,
                                &path,
                                "global_skill_package",
                                &output,
                                &mut granted,
                            );
                        } else {
                            sources.note(
                                &path,
                                "skill_alias_not_package",
                                "A skill alias must name an exact package with a regular SKILL.md",
                                false,
                            );
                        }
                    } else if kind.is_dir() {
                        if regular_skill(&path) {
                            continue;
                        }
                        if depth < MAX_GROUP_DEPTH {
                            children.push((path, depth + 1));
                        } else {
                            sources.note(
                                &path,
                                "skill_source_depth_limit",
                                "Global skill groups exceeded the bounded depth",
                                true,
                            );
                        }
                    }
                }
                if nodes > MAX_DISCOVERY_NODES {
                    break;
                }
                pending.extend(children.into_iter().rev());
            }
        }
        sources.roots.push(ContextRoot {
            id: "builtin".into(),
            workspace: None,
            kind: "builtin",
            scope: "builtin",
            skill_only: true,
            aliases: Vec::new(),
        });
        Ok(sources)
    }

    fn grant(
        &mut self,
        canonical: &Path,
        original: &Path,
        kind: &'static str,
        output: &Arc<crate::output::OutputStore>,
        granted: &mut BTreeMap<PathBuf, usize>,
    ) -> bool {
        if let Some(index) = granted.get(canonical).copied() {
            if canonical != original
                && !self.roots[index]
                    .aliases
                    .iter()
                    .any(|path| path == original)
            {
                self.roots[index].aliases.push(original.to_path_buf());
            }
            return true;
        }
        if self.roots.len() >= MAX_ROOTS || canonical.to_str().is_none() {
            self.note(
                original,
                "skill_source_limit",
                "Skill source count or path encoding exceeded its limit",
                true,
            );
            return false;
        }
        let workspace = match Workspace::with_output(canonical.to_path_buf(), output.clone()) {
            Ok(workspace) => workspace,
            Err(_) => {
                self.note(
                    original,
                    "unreadable_skill_source",
                    "Cannot bind this exact read-only skill source",
                    true,
                );
                return false;
            }
        };
        let fingerprint = digest(canonical.to_string_lossy().as_bytes());
        granted.insert(canonical.to_path_buf(), self.roots.len());
        self.roots.push(ContextRoot {
            id: format!("global-{}", &fingerprint[..16]),
            workspace: Some(workspace),
            kind,
            scope: "global",
            skill_only: true,
            aliases: if canonical == original {
                Vec::new()
            } else {
                vec![original.to_path_buf()]
            },
        });
        true
    }

    fn note(&mut self, path: &Path, code: &str, message: &str, truncated: bool) {
        self.truncated |= truncated;
        diagnostic(
            &mut self.diagnostics,
            &path.to_string_lossy(),
            code,
            message,
        );
    }
}

fn broad_root(path: &Path, home: Option<&Path>) -> bool {
    path.parent().is_none() || home.is_some_and(|home| home.starts_with(path))
}

fn regular_skill(directory: &Path) -> bool {
    fs::symlink_metadata(directory.join("SKILL.md"))
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

pub(super) fn read_builtin(path: &str, max_bytes: usize) -> Result<String, String> {
    if path != "ssmcp/SKILL.md" {
        return Err("Unknown embedded skill path; list the builtin root first".into());
    }
    if max_bytes == 0 || max_bytes > MAX_DOCUMENT_BYTES {
        return Err(format!("max_bytes must be 1..={MAX_DOCUMENT_BYTES}"));
    }
    if BUILTIN_SSMCP_SKILL.len() > max_bytes {
        return Err(
            "Embedded skill exceeds the full-document limit; no partial instructions were returned"
                .into(),
        );
    }
    Ok(BUILTIN_SSMCP_SKILL.into())
}

pub(super) fn scan_builtin(
    root: &ContextRoot,
    args: &ScanArgs,
) -> Result<(Vec<Value>, Value), String> {
    let paths = args.paths.clone().unwrap_or_else(|| vec![".".into()]);
    if paths.is_empty() || paths.len() > 16 {
        return Err("paths must contain 1..=16 relative directories".into());
    }
    let paths = paths
        .iter()
        .map(|path| normalize(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut diagnostics = Vec::new();
    let meta = metadata(
        BUILTIN_SSMCP_SKILL,
        true,
        "ssmcp/SKILL.md",
        &mut diagnostics,
    );
    let skills = if paths
        .iter()
        .any(|path| matches!(path.as_str(), "." | "ssmcp"))
    {
        vec![
            json!({"root":origin(root),"scope":"builtin","source":"embedded",
            "origin_path":document_source(root,"ssmcp/SKILL.md"),"path":"ssmcp/SKILL.md","directory":"ssmcp",
            "name":meta["name"],"description":meta["description"],"metadata_status":meta["status"],
            "size":BUILTIN_SSMCP_SKILL.len(),"metadata_sha256":digest(BUILTIN_SSMCP_SKILL.as_bytes())}),
        ]
    } else {
        Vec::new()
    };
    let snapshot = digest(
        &serde_json::to_vec(&json!({"paths":paths,"skills":skills}))
            .map_err(|error| error.to_string())?,
    );
    let result = json!({"root":origin(root),"scope":"root","sources":[{"path":"ssmcp/SKILL.md","status":"embedded"}],
        "diagnostics":diagnostics,"total":skills.len(),"truncated":false,"snapshot":snapshot,"boundary":boundary(),
        "limits":{"visited_nodes":skills.len(),"metadata_bytes":BUILTIN_SSMCP_SKILL.len(),"page_size":args.limit},
        "metadata_parser":"yaml-frontmatter-subset"});
    Ok((skills, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        context: Context,
        project: PathBuf,
        global: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let mut random = [0; 16];
            crate::platform::random_bytes(&mut random).unwrap();
            let root = std::env::temp_dir().join(format!(
                "ssmcp-skill-sources-{}",
                u128::from_le_bytes(random)
            ));
            fs::create_dir_all(&root).unwrap();
            let root = fs::canonicalize(root).unwrap();
            let project = root.join("project");
            let global = root.join("global-skills");
            fs::create_dir_all(&project).unwrap();
            fs::create_dir_all(&global).unwrap();
            let output = crate::output::OutputStore::at_for_test(root.join("outputs")).unwrap();
            let context = Context::with_output(project.clone(), Vec::new(), output).unwrap();
            Self {
                root,
                context,
                project,
                global,
            }
        }
        fn write(path: &Path, name: &str, description: &str) {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("SKILL.md"), format!("---\nname: {name}\ndescription: {description}\n---\nComplete instructions for {name}.\n")).unwrap();
        }
        async fn list(&self) -> Value {
            self.context
                .perform("ssmcp.skills.list", json!({"scope":"effective","limit":64}))
                .await
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn explicit_context_stays_hermetic_until_sources_are_enabled() {
        let fixture = Fixture::new();
        Fixture::write(
            &fixture.global.join("available"),
            "global-only",
            "Global fixture",
        );
        assert!(
            fixture.list().await["skills"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        fixture
            .context
            .enable_skill_sources(vec![fixture.global.clone()], None)
            .unwrap();
        let page = fixture.list().await;
        assert_eq!(page["truncated"], false);
        let skills = page["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert!(
            skills
                .iter()
                .any(|skill| skill["name"] == "global-only" && skill["scope"] == "global")
        );
        assert!(
            skills
                .iter()
                .any(|skill| skill["name"] == "ssmcp" && skill["scope"] == "builtin")
        );
        for skill in skills {
            let read = fixture
                .context
                .perform(
                    "ssmcp.skills.read",
                    json!({"root":skill["root"]["id"],"path":skill["path"]}),
                )
                .await
                .unwrap();
            assert_eq!(read["metadata"]["name"], skill["name"]);
            assert_eq!(read["scope"], skill["scope"]);
            assert!(!read["content"].as_str().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn project_overrides_global_and_builtin_with_all_shadowed_origins() {
        let fixture = Fixture::new();
        Fixture::write(
            &fixture.project.join(".agents/skills/local"),
            "ssmcp",
            "Project override",
        );
        Fixture::write(&fixture.global.join("managed"), "ssmcp", "Global override");
        fixture
            .context
            .enable_skill_sources(vec![fixture.global.clone(), fixture.global.clone()], None)
            .unwrap();
        let page = fixture.list().await;
        let skills = page["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["scope"], "project");
        assert_eq!(skills[0]["description"], "Project override");
        let shadowed = skills[0]["shadowed"].as_array().unwrap();
        assert_eq!(shadowed.len(), 2);
        assert_eq!(shadowed[0]["scope"], "global");
        assert_eq!(shadowed[1]["scope"], "builtin");
        for origin in shadowed {
            let read = fixture
                .context
                .perform(
                    "ssmcp.skills.read",
                    json!({"root":origin["root"]["id"],"path":origin["path"]}),
                )
                .await
                .unwrap();
            assert_eq!(read["metadata"]["name"], "ssmcp");
        }
    }

    #[tokio::test]
    async fn skill_assets_do_not_make_metadata_discovery_incomplete() {
        let fixture = Fixture::new();
        let package = fixture.global.join("compact");
        Fixture::write(&package, "compact", "Metadata fixture");
        fs::create_dir_all(package.join("assets/a/b/c/d/e/f/g/h/i/j")).unwrap();
        Fixture::write(
            &package.join("scripts/not-an-installed-skill"),
            "asset-fake",
            "Not a package root",
        );
        fixture
            .context
            .enable_skill_sources(vec![fixture.global.clone()], None)
            .unwrap();
        let page = fixture.list().await;
        assert_eq!(page["truncated"], false);
        assert!(
            !page["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|skill| skill["name"] == "asset-fake")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_packages_are_readable_deduplicated_and_do_not_grant_parent_directories() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let target = fixture.root.join("installed/only-package");
        Fixture::write(&target, "linked", "Installed package");
        fs::write(fixture.root.join("installed/AGENTS.md"), "Outside package").unwrap();
        symlink(&target, fixture.global.join("first-alias")).unwrap();
        symlink(&target, fixture.global.join("second-alias")).unwrap();
        symlink(&fixture.root, fixture.global.join("not-a-package")).unwrap();
        fixture
            .context
            .enable_skill_sources(vec![fixture.global.clone()], Some(fixture.root.clone()))
            .unwrap();
        let page = fixture.list().await;
        assert_eq!(page["truncated"], false);
        let linked: Vec<_> = page["skills"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|skill| skill["name"] == "linked")
            .collect();
        assert_eq!(linked.len(), 1);
        let linked = linked[0];
        assert_eq!(linked["root"]["path"], target.to_string_lossy().as_ref());
        assert_eq!(linked["root"]["aliases"].as_array().unwrap().len(), 2);
        let id = linked["root"]["id"].as_str().unwrap();
        let read = fixture
            .context
            .perform("ssmcp.skills.read", json!({"root":id,"path":"SKILL.md"}))
            .await
            .unwrap();
        assert!(
            read["content"]
                .as_str()
                .unwrap()
                .contains("Complete instructions for linked")
        );
        for (tool, path) in [
            ("ssmcp.skills.read", "../SKILL.md"),
            ("ssmcp.agents.read", "AGENTS.md"),
            ("ssmcp.agents.resolve", "."),
        ] {
            assert!(
                fixture
                    .context
                    .perform(tool, json!({"root":id,"path":path}))
                    .await
                    .is_err()
            );
        }
        let workspace =
            Workspace::with_output(fixture.project.clone(), fixture.context.output.clone())
                .unwrap();
        assert!(
            workspace
                .perform(
                    "ssmcp.fs.read",
                    json!({"path":"../installed/only-package/SKILL.md"})
                )
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aliases_to_an_existing_global_package_retain_the_duplicate_origin() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let package = fixture.global.join("real-package");
        Fixture::write(&package, "one-package", "Physical dedup fixture");
        symlink(&package, fixture.global.join("installed-alias")).unwrap();
        fixture
            .context
            .enable_skill_sources(vec![fixture.global.clone()], None)
            .unwrap();
        let page = fixture.list().await;
        let matches: Vec<_> = page["skills"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|skill| skill["name"] == "one-package")
            .collect();
        assert_eq!(matches.len(), 1);
        let duplicate = &matches[0]["shadowed"][0];
        assert_eq!(duplicate["same_document"], true);
        assert_eq!(
            duplicate["root"]["aliases"][0],
            fixture
                .global
                .join("installed-alias")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[tokio::test]
    async fn large_skill_bodies_do_not_exhaust_the_frontmatter_scan_budget() {
        let fixture = Fixture::new();
        for index in 0..80 {
            let package = fixture.global.join(format!("package-{index:03}"));
            Fixture::write(&package, &format!("package-{index:03}"), "Small metadata");
            let mut body = fs::read_to_string(package.join("SKILL.md")).unwrap();
            body.push_str(&"x".repeat(20_000));
            fs::write(package.join("SKILL.md"), body).unwrap();
        }
        fixture
            .context
            .enable_skill_sources(vec![fixture.global.clone()], None)
            .unwrap();
        let page = fixture.list().await;
        assert_eq!(page["truncated"], false);
        assert_eq!(page["total"], 81);
        assert!(page["next_cursor"].is_string());
    }

    #[tokio::test]
    async fn builtin_reads_are_exact_bounded_and_reject_other_paths() {
        let fixture = Fixture::new();
        fixture
            .context
            .enable_skill_sources(Vec::new(), None)
            .unwrap();
        let read = fixture
            .context
            .perform(
                "ssmcp.skills.read",
                json!({"root":"builtin","path":"ssmcp/SKILL.md"}),
            )
            .await
            .unwrap();
        assert_eq!(read["content"], BUILTIN_SSMCP_SKILL);
        assert_eq!(read["sha256"], digest(BUILTIN_SSMCP_SKILL.as_bytes()));
        assert!(
            fixture
                .context
                .perform(
                    "ssmcp.skills.read",
                    json!({"root":"builtin","path":"ssmcp/SKILL.md","max_bytes":1})
                )
                .await
                .is_err()
        );
        assert!(
            fixture
                .context
                .perform(
                    "ssmcp.skills.read",
                    json!({"root":"builtin","path":"other/SKILL.md"})
                )
                .await
                .is_err()
        );
        assert!(
            fixture
                .context
                .perform(
                    "ssmcp.agents.read",
                    json!({"root":"builtin","path":"AGENTS.md"})
                )
                .await
                .is_err()
        );
    }
}

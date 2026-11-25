use super::*;

#[test]
fn rc_loads_explicit_workspace_overrides_and_rejects_unknown_or_invalid_inputs() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temporary.0.canonicalize().unwrap();
    let file = root.join("local.js");
    write_new(&file, b"core => ({example: () => 1})\n").unwrap();
    let paths = Paths {
        workspace: root.clone(),
        rc: root.join("ssmcprc.toml"),
        directory: root.join("data"),
        global: root.join("home/.ssmcp/tools"),
    };
    assert!(paths.config().unwrap().0.is_none());
    write_new(
        &paths.rc,
        b"version = 1\n[tools]\noverrides = [\"local.js\"]\n",
    )
    .unwrap();
    let (identity, sources) = paths.config().unwrap();
    assert_eq!(
        sources,
        [Selection {
            name: "override:0".into(),
            path: file
        }]
    );
    assert_eq!(
        identity.unwrap().sha256,
        digest(&std::fs::read(&paths.rc).unwrap())
    );
    for invalid in [
        "version = 2\n",
        "version = 1\nunknown = true\n",
        "version = 1\n[tools]\noverrides = [\"missing.js\"]\n",
        "version = 1\n[tools]\noverrides = [\"\"]\n",
    ] {
        std::fs::write(&paths.rc, invalid).unwrap();
        assert!(paths.config().is_err(), "{invalid}");
    }
}

#[test]
fn bundle_checks_loaded_bytes_and_manifest_cannot_escape_its_registered_root() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temporary.0.canonicalize().unwrap();
    write_new(
        &root.join("manifest.json"),
        br#"{"schema":"ssmcp-tools-v1","entry":"../secret.js"}"#,
    )
    .unwrap();
    assert!(entry(&root).unwrap_err().contains("traversal"));
    let code = "core => ({example: () => 1})";
    let source = Source {
        name: "one".into(),
        path: "/fixture/source.js".into(),
        code: code.into(),
        sha256: digest(code.as_bytes()),
        repository: None,
    };
    let mut bundle = Bundle {
        schema: SCHEMA.into(),
        rc: None,
        sources: vec![source],
    };
    bundle.clone().validate().unwrap();
    bundle.sources[0].code.push(' ');
    assert!(bundle.clone().validate().is_err());
    bundle.sources[0].sha256 = digest(bundle.sources[0].code.as_bytes());
    bundle.sources.push(bundle.sources[0].clone());
    assert!(bundle.validate().is_err());
}

#[tokio::test]
async fn offline_seed_is_an_editable_git_checkout_and_dirty_work_is_retained() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temporary.0.clone();
    git::owned(async move {
        let official = ensure_official(&root.join("tools")).await.unwrap();
        let original = source("official".into(), &official).await.unwrap();
        assert_eq!(original.sha256, SEED_SOURCE_SHA256);
        assert_eq!(
            original.repository.as_ref().unwrap().head.as_deref(),
            Some(SEED_COMMIT)
        );
        assert!(!original.repository.as_ref().unwrap().dirty);
        assert_eq!(
            git::text(&official, &["remote", "get-url", "origin"])
                .await
                .unwrap(),
            ORIGIN
        );
        let edited = format!("{}\n// local iteration remains local\n", original.code);
        std::fs::write(official.join("tools.js"), &edited).unwrap();
        assert_eq!(
            ensure_official(&root.join("tools")).await.unwrap(),
            official
        );
        let changed = source("official".into(), &official).await.unwrap();
        assert!(changed.repository.unwrap().dirty);
        assert_eq!(changed.code, edited);
        assert_ne!(changed.sha256, original.sha256);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_repository_caller_keeps_the_lease_until_owned_cleanup_finishes() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temporary.0.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (cleaning, cleanup_started) = tokio::sync::oneshot::channel();
    let (release, cleanup) = tokio::sync::oneshot::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    let operation = tokio::spawn(git::owned(async move {
        let lock = Lock::take(&root).unwrap();
        started.send(()).unwrap();
        git::cancelled().await;
        cleaning.send(()).unwrap();
        // A real Git capture awaits Managed::shutdown here. This handshake
        // deterministically proves the surrounding owner cannot release early.
        cleanup.await.unwrap();
        drop(lock);
        finished.send(()).unwrap();
        Ok(())
    }));
    ready.await.unwrap();
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    cleanup_started.await.unwrap();
    assert!(Lock::take(&temporary.0).err().unwrap().contains("busy"));
    release.send(()).unwrap();
    done.await.unwrap();
    drop(Lock::take(&temporary.0).unwrap());
}

#[tokio::test]
async fn git_filter_included_configuration_is_rejected_before_status() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temporary.0.clone();
    git::owned(async move {
        git::run(&root, &["init".into(), ".".into()], false)
            .await
            .unwrap();
        let included = root.join("filter-config");
        write_new(
            &included,
            b"[filter \"fixture\"]\n\tclean = touch filter-ran\n",
        )
        .unwrap();
        git::run(
            &root,
            &[
                "config".into(),
                "--local".into(),
                "include.path".into(),
                included.into_os_string(),
            ],
            false,
        )
        .await
        .unwrap();
        assert!(
            git::identity(&root)
                .await
                .unwrap_err()
                .contains("configured Git filters")
        );
        assert!(!root.join("filter-ran").exists());
        Ok(())
    })
    .await
    .unwrap();
}

fn local_paths(root: &Path) -> Paths {
    let root = root.canonicalize().unwrap();
    let workspace = root.join("project");
    std::fs::create_dir(&workspace).unwrap();
    Paths {
        workspace: workspace.canonicalize().unwrap(),
        rc: root.join("config/ssmcprc.toml"),
        directory: root.join("data/tools"),
        global: root.join("home/.ssmcp/tools"),
    }
}

fn local_factory(path: &Path, code: &str) {
    std::fs::create_dir_all(path).unwrap();
    write_new(
        &path.join("manifest.json"),
        br#"{"schema":"ssmcp-tools-v1","entry":"tools.js"}"#,
    )
    .unwrap();
    write_new(&path.join("tools.js"), code.as_bytes()).unwrap();
}

fn save_test_rc(paths: &Paths, config: &Rc) {
    std::fs::create_dir_all(paths.rc.parent().unwrap()).unwrap();
    std::fs::write(&paths.rc, toml::to_string_pretty(config).unwrap()).unwrap();
}

#[tokio::test]
async fn local_layers_are_inert_until_trusted_rc_registration_and_keep_explicit_order() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    local_factory(&paths.global, "core => ({example: () => 'global'})");
    local_factory(&paths.project(), "core => ({example: () => 'project'})");
    let legacy = paths.workspace.join("registered.js");
    write_new(&legacy, b"core => ({example: () => 'legacy'})").unwrap();
    git::owned(async move {
        let first = load_configured(&paths).await?;
        assert_eq!(first.sources.len(), 1, "unregistered files never activate");
        assert_eq!(first.sources[0].name, "official");
        let config = Rc {
            env: Default::default(),
            version: 1,
            tools: Tools {
                overrides: vec![PathBuf::from("registered.js")],
                global: true,
                projects: vec![paths.workspace.clone()],
            },
        };
        save_test_rc(&paths, &config);
        let bundle = load_configured(&paths).await?;
        assert_eq!(
            bundle
                .sources
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["official", "override:0", "global", "project"]
        );
        assert_eq!(
            bundle.sources[2].path,
            paths.global.join("tools.js").display().to_string()
        );
        assert_eq!(
            bundle.sources[3].path,
            paths.project().join("tools.js").display().to_string()
        );
        let identity = bundle.provenance();
        assert_eq!(
            identity["rc"]["sha256"],
            digest(&std::fs::read(&paths.rc).unwrap())
        );
        for (source, receipt) in bundle
            .sources
            .iter()
            .zip(identity["sources"].as_array().unwrap())
        {
            assert_eq!(receipt["sha256"], digest(source.code.as_bytes()));
            assert!(receipt.get("code").is_none());
        }
        std::fs::write(
            paths.project().join("tools.js"),
            b"core => ({example: () => 'edited'})",
        )
        .unwrap();
        assert_ne!(
            load_configured(&paths).await?.sources[3].sha256,
            bundle.sources[3].sha256
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[test]
fn project_registration_matches_only_the_exact_workspace_and_duplicates_are_errors() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    local_factory(&paths.project(), "core => ({})");
    let other = temporary.0.join("other");
    std::fs::create_dir(&other).unwrap();
    save_test_rc(
        &paths,
        &Rc {
            env: Default::default(),
            version: 1,
            tools: Tools {
                projects: vec![other],
                ..Tools::default()
            },
        },
    );
    assert!(paths.config().unwrap().1.is_empty());
    for tools in [
        Tools {
            projects: vec![paths.workspace.clone(), paths.workspace.clone()],
            ..Tools::default()
        },
        Tools {
            projects: vec![PathBuf::from("relative")],
            ..Tools::default()
        },
        Tools {
            overrides: vec![paths.project(), paths.project()],
            ..Tools::default()
        },
    ] {
        save_test_rc(
            &paths,
            &Rc {
                env: Default::default(),
                version: 1,
                tools,
            },
        );
        assert!(paths.config().is_err());
    }
}

#[tokio::test]
async fn missing_or_invalid_enabled_layer_fails_instead_of_using_official_helpers() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    save_test_rc(
        &paths,
        &Rc {
            env: Default::default(),
            version: 1,
            tools: Tools {
                global: true,
                ..Tools::default()
            },
        },
    );
    git::owned(async move {
        assert!(
            load_configured(&paths)
                .await
                .unwrap_err()
                .contains("selected global")
        );
        local_factory(&paths.global, "");
        assert!(
            load_configured(&paths)
                .await
                .unwrap_err()
                .contains("invalid")
        );
        std::fs::write(paths.global.join("tools.js"), b"core => ({})").unwrap();
        std::fs::write(
            paths.global.join("manifest.json"),
            br#"{"schema":"ssmcp-tools-v1","entry":"../outside.js"}"#,
        )
        .unwrap();
        assert!(
            load_configured(&paths)
                .await
                .unwrap_err()
                .contains("traversal")
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn init_project_creates_an_independent_git_repository_and_registration_is_idempotent() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    git::owned(async move {
        let result = local::manage_owned(
            &paths,
            local::Operation::Init {
                scope: local::Scope::Project,
                git: true,
            },
        )
        .await?;
        assert_eq!(result["created"], true);
        assert_eq!(result["initialized_git"], true);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["path"], paths.project().display().to_string());
        assert!(paths.project().join(".git").is_dir());
        let repository = git::identity(&paths.project()).await?.unwrap();
        assert_eq!(repository.root, paths.project().display().to_string());
        assert!(
            repository.head.is_none(),
            "init must not fabricate a commit"
        );
        let before = read_file(&paths.rc, MAX_RC)?;
        let enabled =
            local::manage_owned(&paths, local::Operation::Enable(local::Scope::Project)).await?;
        assert_eq!(enabled["rc_changed"], false);
        assert_eq!(read_file(&paths.rc, MAX_RC)?, before);
        let sources = load_configured(&paths).await?;
        assert_eq!(sources.sources.last().unwrap().name, "project");
        assert!(
            local::manage_owned(
                &paths,
                local::Operation::Init {
                    scope: local::Scope::Project,
                    git: false,
                }
            )
            .await
            .unwrap_err()
            .contains("already exists")
        );
        assert_eq!(read_file(&paths.rc, MAX_RC)?, before);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn global_init_uses_plural_tools_and_disable_preserves_broken_source_and_git() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    git::owned(async move {
        let result = local::manage_owned(
            &paths,
            local::Operation::Init {
                scope: local::Scope::Global,
                git: true,
            },
        )
        .await?;
        assert_eq!(result["path"], paths.global.display().to_string());
        assert!(paths.global.ends_with(".ssmcp/tools"));
        assert!(!paths.global.with_file_name("tool").exists());
        std::fs::remove_file(paths.global.join("tools.js")).unwrap();
        assert!(load_configured(&paths).await.is_err());
        let disabled =
            local::manage_owned(&paths, local::Operation::Disable(local::Scope::Global)).await?;
        assert_eq!(disabled["enabled"], false);
        assert!(paths.global.join(".git").exists());
        assert!(paths.global.join("manifest.json").exists());
        assert_eq!(load_configured(&paths).await?.sources.len(), 1);
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn enabling_a_project_factory_accepts_dot_relative_entry_paths() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    let project = paths.project();
    let entry_path = project.join("dist/tools.js");
    let manifest_path = project.join("manifest.json");
    let factory = "core => ({ projectVersion: () => '1.0' })";
    std::fs::create_dir_all(entry_path.parent().unwrap()).unwrap();
    write_new(&entry_path, factory.as_bytes()).unwrap();
    std::fs::write(
        &manifest_path,
        br#"{"schema":"ssmcp-tools-v1","entry":"./dist/tools.js"}"#,
    )
    .unwrap();
    git::owned(async move {
        local::manage_owned(&paths, local::Operation::Enable(local::Scope::Project)).await?;
        let dotted = load_configured(&paths).await?;
        let extension = dotted.sources.last().unwrap();
        assert_eq!(extension.name, "project");
        assert_eq!(extension.code, factory);
        assert_eq!(extension.path, entry_path.display().to_string());
        std::fs::write(
            &manifest_path,
            br#"{"schema":"ssmcp-tools-v1","entry":"dist/tools.js"}"#,
        )
        .unwrap();
        assert_eq!(
            dotted.provenance(),
            load_configured(&paths).await?.provenance()
        );
        for invalid in [".", "./", "./../tools.js"] {
            std::fs::write(
                &manifest_path,
                json!({"schema":"ssmcp-tools-v1","entry":invalid}).to_string(),
            )
            .unwrap();
            assert!(entry(&project).is_err(), "{invalid}");
        }
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn enabling_existing_sources_preserves_files_and_migrates_their_old_rc_registration() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    local_factory(&paths.project(), "core => ({workflow: () => 'owned'})");
    save_test_rc(
        &paths,
        &Rc {
            env: Default::default(),
            version: 1,
            tools: Tools {
                overrides: vec![paths.project()],
                ..Tools::default()
            },
        },
    );
    git::owned(async move {
        let before = read_file(&paths.project().join("tools.js"), MAX_SOURCE)?;
        let result =
            local::manage_owned(&paths, local::Operation::Enable(local::Scope::Project)).await?;
        assert_eq!(result["created"], false);
        assert_eq!(result["migrated_overrides"], 1);
        let (_, selected) = paths.config()?;
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "project");
        assert_eq!(
            read_file(&paths.project().join("tools.js"), MAX_SOURCE)?,
            before
        );
        assert!(
            !paths.project().join(".git").exists(),
            "enable must not initialize Git"
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_existing_source_cannot_be_enabled_and_init_never_overwrites_it() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    local_factory(&paths.project(), "");
    git::owned(async move {
        assert!(
            local::manage_owned(&paths, local::Operation::Enable(local::Scope::Project))
                .await
                .is_err()
        );
        assert!(!paths.rc.exists());
        assert!(
            local::manage_owned(
                &paths,
                local::Operation::Init {
                    scope: local::Scope::Project,
                    git: false,
                }
            )
            .await
            .unwrap_err()
            .contains("already exists")
        );
        assert_eq!(
            read_file(&paths.project().join("tools.js"), MAX_SOURCE)?,
            b""
        );
        assert!(!paths.rc.exists());
        Ok(())
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn local_layer_symlinks_cannot_redirect_initialization_or_selection() {
    use std::os::unix::fs::symlink;
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    let external = temporary.0.join("outside");
    std::fs::create_dir(&external).unwrap();
    symlink(&external, paths.workspace.join(".ssmcp")).unwrap();
    git::owned(async move {
        assert!(
            local::manage_owned(
                &paths,
                local::Operation::Init {
                    scope: local::Scope::Project,
                    git: false,
                }
            )
            .await
            .is_err()
        );
        assert!(!external.join("tools").exists());
        assert!(!paths.rc.exists());
        local_factory(&external.join("tools"), "core => ({})");
        save_test_rc(
            &paths,
            &Rc {
                env: Default::default(),
                version: 1,
                tools: Tools {
                    projects: vec![paths.workspace.clone()],
                    ..Tools::default()
                },
            },
        );
        assert!(
            load_configured(&paths)
                .await
                .unwrap_err()
                .contains("symlinks")
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[test]
fn local_config_save_rejects_a_changed_snapshot_without_overwriting_it() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    let changed = "version = 1\n[tools]\nprojects = []\nglobal = false\n";
    std::fs::create_dir_all(paths.rc.parent().unwrap()).unwrap();
    std::fs::write(&paths.rc, changed).unwrap();
    let desired = Rc {
        env: Default::default(),
        version: 1,
        tools: Tools {
            global: true,
            ..Tools::default()
        },
    };
    assert!(
        local::save_config(&paths, &None, &desired)
            .unwrap_err()
            .contains("changed")
    );
    assert_eq!(std::fs::read_to_string(&paths.rc).unwrap(), changed);
}

#[test]
fn source_counts_allow_sixteen_overrides_and_both_local_layers_without_relaxing_byte_limits() {
    let source = |index: usize, size: usize| {
        let mut code = "core => ({})\n".to_owned();
        code.extend(std::iter::repeat_n(' ', size.saturating_sub(code.len())));
        Source {
            name: format!("layer:{index}"),
            path: format!("/fixture/{index}.js"),
            sha256: digest(code.as_bytes()),
            code,
            repository: None,
        }
    };
    let make = |sources| Bundle {
        schema: SCHEMA.into(),
        rc: None,
        sources,
    };
    assert_eq!(MAX_SOURCES, 19);
    make((0..19).map(|i| source(i, 20)).collect())
        .validate()
        .unwrap();
    assert!(
        make((0..20).map(|i| source(i, 20)).collect())
            .validate()
            .is_err()
    );
    make(vec![
        source(0, MAX_SOURCE),
        source(1, MAX_TOTAL - MAX_SOURCE),
    ])
    .validate()
    .unwrap();
    assert!(make(vec![source(0, MAX_SOURCE + 1)]).validate().is_err());
    assert!(
        make(vec![
            source(0, MAX_SOURCE),
            source(1, MAX_TOTAL - MAX_SOURCE + 1)
        ])
        .validate()
        .is_err()
    );
    let mut duplicate = source(1, 20);
    duplicate.path = "/fixture/0.js".into();
    assert!(make(vec![source(0, 20), duplicate]).validate().is_err());
}

#[test]
fn local_command_parser_and_planned_paths_do_not_activate_sources() {
    let parse = |args: &[&str]| local::parse(&args.iter().map(OsString::from).collect::<Vec<_>>());
    assert_eq!(
        parse(&["init", "--git"]).unwrap(),
        Some(local::Operation::Init {
            scope: local::Scope::Project,
            git: true
        })
    );
    assert_eq!(
        parse(&["init", "global", "--git"]).unwrap(),
        Some(local::Operation::Init {
            scope: local::Scope::Global,
            git: true
        })
    );
    assert_eq!(
        parse(&["disable", "global"]).unwrap(),
        Some(local::Operation::Disable(local::Scope::Global))
    );
    for invalid in [
        &["enable", "--git"][..],
        &["init", "global", "project"],
        &["init", "--git", "--git"],
        &["init", "arbitrary/path"],
    ] {
        assert!(parse(invalid).is_err());
    }
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    let before = std::fs::read_dir(&temporary.0).unwrap().count();
    let plan = local::locations(&paths);
    assert_eq!(plan["global"], paths.global.display().to_string());
    assert_eq!(plan["project"], paths.project().display().to_string());
    assert_eq!(std::fs::read_dir(&temporary.0).unwrap().count(), before);
    assert!(!paths.rc.exists() && !paths.directory.exists() && !paths.global.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn initialization_rejects_symlinks_above_an_existing_local_parent() {
    use std::os::unix::fs::symlink;
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let mut paths = local_paths(&temporary.0);
    let external = temporary.0.join("external");
    std::fs::create_dir_all(external.join("home/.ssmcp")).unwrap();
    let alias = temporary.0.join("alias");
    symlink(&external, &alias).unwrap();
    paths.global = alias.join("home/.ssmcp/tools");
    git::owned(async move {
        assert!(
            local::manage_owned(
                &paths,
                local::Operation::Init {
                    scope: local::Scope::Global,
                    git: false,
                }
            )
            .await
            .unwrap_err()
            .contains("symlinks")
        );
        assert!(!external.join("home/.ssmcp/tools").exists());
        assert!(!paths.rc.exists());
        assert_eq!(
            std::fs::read_dir(external.join("home/.ssmcp"))
                .unwrap()
                .count(),
            0
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[test]
fn selected_source_limit_keeps_sixteen_legacy_overrides_and_both_local_layers() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let paths = local_paths(&temporary.0);
    local_factory(&paths.global, "core => ({})");
    local_factory(&paths.project(), "core => ({})");
    let overrides = (0..16)
        .map(|index| {
            let name = PathBuf::from(format!("registered-{index}.js"));
            write_new(&paths.workspace.join(&name), b"core => ({})").unwrap();
            name
        })
        .collect::<Vec<_>>();
    let mut config = Rc {
        env: Default::default(),
        version: 1,
        tools: Tools {
            overrides,
            global: true,
            projects: vec![paths.workspace.clone()],
        },
    };
    save_test_rc(&paths, &config);
    let selected = paths.config().unwrap().1;
    assert_eq!(selected.len(), 18);
    assert_eq!(selected[15].name, "override:15");
    assert_eq!(selected[16].name, "global");
    assert_eq!(selected[17].name, "project");
    config.tools.overrides.push(PathBuf::from("excess.js"));
    save_test_rc(&paths, &config);
    assert!(paths.config().unwrap_err().contains("at most 16 overrides"));
    config.tools.overrides.clear();
    config.tools.projects = (0..129)
        .map(|index| temporary.0.join(format!("project-{index}")))
        .collect();
    save_test_rc(&paths, &config);
    assert!(
        paths
            .config()
            .unwrap_err()
            .contains("128 trusted project roots")
    );
}

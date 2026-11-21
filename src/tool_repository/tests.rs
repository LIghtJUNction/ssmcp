use super::*;

#[test]
fn rc_loads_explicit_workspace_overrides_and_rejects_unknown_or_invalid_inputs() {
    let temporary = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temporary.0.canonicalize().unwrap();
    let file = root.join("local.js");
    write_new(&file, b"core => ({example: () => 1})\n").unwrap();
    let paths = Paths { workspace:root.clone(), rc:root.join("ssmcprc.toml"), directory:root.join("data") };
    assert!(paths.config().unwrap().0.is_none());
    write_new(&paths.rc, b"version = 1\n[tools]\noverrides = [\"local.js\"]\n").unwrap();
    let (identity, sources) = paths.config().unwrap();
    assert_eq!(sources, [file]);
    assert_eq!(identity.unwrap().sha256, digest(&std::fs::read(&paths.rc).unwrap()));
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
    write_new(&root.join("manifest.json"), br#"{"schema":"ssmcp-tools-v1","entry":"../secret.js"}"#).unwrap();
    assert!(entry(&root).unwrap_err().contains("traversal"));
    let code = "core => ({example: () => 1})";
    let source = Source { name:"one".into(), path:"/fixture/source.js".into(),
        code:code.into(), sha256:digest(code.as_bytes()), repository:None };
    let mut bundle = Bundle { schema:SCHEMA.into(), rc:None, sources:vec![source] };
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
        assert_eq!(original.repository.as_ref().unwrap().head.as_deref(), Some(SEED_COMMIT));
        assert!(!original.repository.as_ref().unwrap().dirty);
        assert_eq!(git::text(&official, &["remote", "get-url", "origin"]).await.unwrap(), ORIGIN);
        let edited = format!("{}\n// local iteration remains local\n", original.code);
        std::fs::write(official.join("tools.js"), &edited).unwrap();
        assert_eq!(ensure_official(&root.join("tools")).await.unwrap(), official);
        let changed = source("official".into(), &official).await.unwrap();
        assert!(changed.repository.unwrap().dirty);
        assert_eq!(changed.code, edited);
        assert_ne!(changed.sha256, original.sha256);
        Ok(())
    }).await.unwrap();
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
        git::run(&root, &["init".into(), ".".into()], false).await.unwrap();
        let included = root.join("filter-config");
        write_new(&included, b"[filter \"fixture\"]\n\tclean = touch filter-ran\n").unwrap();
        git::run(&root, &[
            "config".into(), "--local".into(), "include.path".into(), included.into_os_string(),
        ], false).await.unwrap();
        assert!(git::identity(&root).await.unwrap_err().contains("configured Git filters"));
        assert!(!root.join("filter-ran").exists());
        Ok(())
    }).await.unwrap();
}

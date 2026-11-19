use super::*;

fn seed_completed(fixture: &Fixture, count: u64) {
    // Seed real private files without invoking capture admission. This models
    // the retained logs a newly connected client encounters from earlier runs.
    for number in 1..=count {
        let id = format!("{number:032x}");
        let mut file = fixture
            .store
            .open(&artifact_name(&id), Access::CreateNewWrite)
            .unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        fixture
            .store
            .write_manifest(&ArtifactMetadata {
                schema: SCHEMA.into(),
                id: id.clone(),
                path: fixture
                    .path
                    .join(artifact_name(&id))
                    .to_string_lossy()
                    .into_owned(),
                scope: "retained-fixture".into(),
                sha256: format!("{:x}", Sha256::digest(b"x")),
                sha256_scope: "first_bytes_snapshot".into(),
                bytes: 1,
                lines: 1,
                characters: Some(1),
                observed_bytes: 1,
                complete: true,
                expires_at_unix: now().saturating_add(RETENTION_SECS),
                retention_secs: RETENTION_SECS,
                max_bytes: ARTIFACT_BYTES,
                error: None,
            })
            .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn cleanup_has_bounded_descriptor_use_and_preserves_an_active_writer() {
    const CHILD: &str = "SSMCP_RETENTION_FD_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "output::tests::retention::cleanup_has_bounded_descriptor_use_and_preserves_an_active_writer", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let limit = libc::rlimit {
        rlim_cur: 64,
        rlim_max: 64,
    };
    // Only this dedicated child test process receives the reduced descriptor cap.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let fixture = Fixture::new(Policy {
        inline_bytes: 0,
        retained_bytes: 0,
        ..Policy::default()
    });
    let mut active = fixture.store.capture("must-survive");
    active.push(b"running").unwrap();
    let active_metadata = active.metadata().unwrap();
    seed_completed(&fixture, 384);
    std::fs::write(fixture.path.join("unrelated.txt"), b"keep").unwrap();
    let summary = fixture.store.cleanup().unwrap();
    assert_eq!(summary["removed_files"], 384);
    assert_eq!(summary["active_artifacts"], 1);
    assert_eq!(std::fs::read(&active_metadata.path).unwrap(), b"running");
    assert_eq!(
        std::fs::read(fixture.path.join("unrelated.txt")).unwrap(),
        b"keep"
    );
    active.push(b" still live").unwrap();
    active.finish().unwrap();
    assert_eq!(fixture.store.cleanup().unwrap()["removed_files"], 1);
}

#[test]
fn small_quota_is_rechecked_before_the_next_capture() {
    let fixture = Fixture::new(Policy {
        inline_bytes: 0,
        retained_bytes: 8,
        ..Policy::default()
    });
    let mut first = fixture.store.capture("first");
    first.push(b"1234567").unwrap();
    first.finish().unwrap();
    let first_path = first.metadata().unwrap().path;
    let mut second = fixture.store.capture("second");
    second.push(b"890").unwrap();
    second.finish().unwrap();
    let second_path = second.metadata().unwrap().path;
    let mut third = fixture.store.capture("next-admission");
    third.push(b"x").unwrap();
    third.finish().unwrap();
    let survivors = [&first_path, &second_path]
        .into_iter()
        .filter(|path| Path::new(path).exists())
        .count();
    // Same-second ties use the resource ID, so either old completed log can be
    // evicted; the retained byte quota and the new capture must still hold.
    assert_eq!(survivors, 1);
    assert!(
        fixture.store.cleanup().unwrap()["retained_bytes"]
            .as_u64()
            .unwrap()
            <= 8
    );
    assert_eq!(std::fs::read(third.metadata().unwrap().path).unwrap(), b"x");
}

#[test]
fn failed_automatic_sweep_preserves_its_work_for_retry() {
    let fixture = Fixture::new(Policy {
        inline_bytes: 0,
        retained_bytes: 1,
        ..Policy::default()
    });
    let mut first = fixture.store.capture("first");
    first.push(b"larger than quota").unwrap();
    first.finish().unwrap();
    let first_path = first.metadata().unwrap().path;
    let lock_path = fixture.path.join(".cleanup.lock");
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::create_dir(&lock_path).unwrap();
    let mut blocked = fixture.store.capture("cannot-sweep");
    assert!(blocked.spill().is_err());
    assert!(Path::new(&first_path).exists());
    std::fs::remove_dir(&lock_path).unwrap();
    let mut retry = fixture.store.capture("retry");
    retry.push(b"x").unwrap();
    retry.finish().unwrap();
    assert!(!Path::new(&first_path).exists());
    assert_eq!(std::fs::read(retry.metadata().unwrap().path).unwrap(), b"x");
}

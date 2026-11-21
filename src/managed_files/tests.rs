use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

struct TestRoot(PathBuf);

fn private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        crate::platform::windows::create_private_directory(path)
    }
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock is after the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-managed-files-{label}-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        private_directory(&path).expect("create an exclusively owned private test root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.0.join(relative)
    }

    fn create(&self, relative: &str, contents: &[u8]) -> File {
        let path = self.join(relative);
        let mut parent = self.0.clone();
        for component in Path::new(relative)
            .parent()
            .expect("a fixture has a parent")
            .components()
        {
            parent.push(component.as_os_str());
            match private_directory(&parent) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create private fixture parent: {error}"),
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).expect("create a new fixture file");
        file.write_all(contents).expect("write fixture contents");
        file.sync_all().expect("finish fixture bytes before registration");
        // Closing the writer finalizes Windows last-write metadata as well.
        drop(file);
        File::open(path).expect("reopen the fully published fixture for registration")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        // Each test owns this entire tree; no real workspace is ever passed in.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn register(
    manager: &ManagedFiles,
    root: &TestRoot,
    relative: &str,
    contents: &[u8],
    category: Category,
) -> u64 {
    let file = root.create(relative, contents);
    let registration = manager
        .register_created(Path::new(relative), &file, category, "unit fixture", "unit owner")
        .expect("register the exact newly created file");
    assert_eq!(registration.record().relative_path, relative);
    let created = registration.record().created_at_unix
        .max(registration.record().identity.modified_at_unix);
    drop(registration);
    drop(file);
    created
}

fn expire_immediately(manager: &ManagedFiles) {
    let mut config = manager.config().expect("read cleanup configuration");
    config.enabled = true;
    config.max_age_seconds = 0;
    manager.set_config(&config).expect("set a deterministic immediate expiry");
}

#[test]
fn registered_temporary_files_expire_at_the_age_boundary_with_exact_byte_counts() {
    let root = TestRoot::new("age");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let relative = ".ssmcp/temporary/age.bin";
    let bytes = "temporary payload 界".as_bytes();
    let created = register(&manager, &root, relative, bytes, Category::Temporary);
    let mut config = manager.config().unwrap();
    config.max_age_seconds = 60;
    manager.set_config(&config).unwrap();

    let before = manager.cleanup_at(false, "unit-age-before", created + 59).unwrap();
    assert_eq!(before.removed_files, 0);
    assert_eq!(fs::read(root.join(relative)).unwrap(), bytes);

    let expired = manager.cleanup_at(false, "unit-age-at", created + 60).unwrap();
    assert_eq!(expired.removed_files, 1);
    assert_eq!(expired.removed_bytes, bytes.len() as u64);
    assert!(expired.errors.is_empty());
    assert!(!root.join(relative).exists());
    let repeated = manager.cleanup_at(false, "unit-age-repeat", created + 61).unwrap();
    assert_eq!(repeated.removed_files, 0);
    assert_eq!(repeated.removed_bytes, 0);
}

#[test]
fn capacity_cleanup_counts_only_the_registered_temporary_bytes_it_actually_removes() {
    let root = TestRoot::new("capacity");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let fixtures: [(&str, &[u8]); 3] = [
        (".ssmcp/temporary/a.bin", b"aaaa"),
        (".ssmcp/temporary/b.bin", b"bbbbb"),
        (".ssmcp/temporary/c.bin", b"ccc"),
    ];
    let now = fixtures.iter().map(|(relative, bytes)| {
        register(&manager, &root, relative, bytes, Category::Temporary)
    }).max().unwrap();
    drop(root.create(".ssmcp/temporary/unregistered.bin", b"unregistered bytes stay put"));
    let mut config = manager.config().unwrap();
    config.max_age_seconds = 86_400;
    config.max_total_bytes = 8;
    manager.set_config(&config).unwrap();

    let summary = manager.cleanup_at(false, "unit-capacity", now).unwrap();
    let mut deleted_files = 0;
    let mut deleted_bytes = 0_u64;
    let mut retained_bytes = 0_u64;
    for (relative, bytes) in fixtures {
        if root.join(relative).exists() {
            assert_eq!(fs::read(root.join(relative)).unwrap(), bytes);
            retained_bytes += bytes.len() as u64;
        } else {
            deleted_files += 1;
            deleted_bytes += bytes.len() as u64;
        }
    }
    assert!(deleted_files > 0);
    assert!(retained_bytes <= 8);
    assert_eq!(summary.removed_files, deleted_files);
    assert_eq!(summary.removed_bytes, deleted_bytes);
    assert!(summary.errors.is_empty());
    assert_eq!(fs::read(root.join(".ssmcp/temporary/unregistered.bin")).unwrap(),
               b"unregistered bytes stay put");
}

#[test]
fn registration_and_reader_leases_protect_a_file_from_another_instance() {
    let root = TestRoot::new("leases");
    let first = ManagedFiles::open(root.path()).unwrap();
    let second = ManagedFiles::open(root.path()).unwrap();
    expire_immediately(&first);
    let relative = Path::new(".ssmcp/temporary/active.bin");
    let file = root.create(relative.to_str().unwrap(), b"leased bytes");
    let registration = first
        .register_created(relative, &file, Category::Temporary, "unit fixture", "original owner")
        .unwrap();
    let now = registration.record().created_at_unix
        .max(registration.record().identity.modified_at_unix) + 1;
    drop(file);
    let held = second.cleanup_at(false, "unit-registration", now).unwrap();
    assert_eq!(held.removed_files, 0);
    assert_eq!(held.kept_active, 1);
    assert_eq!(fs::read(root.join(relative)).unwrap(), b"leased bytes");

    drop(registration);
    let lease = second.lease(relative).unwrap();
    let held = first.cleanup_at(false, "unit-reader", now).unwrap();
    assert_eq!(held.removed_files, 0);
    assert_eq!(held.kept_active, 1);
    drop(lease);
    let released = first.cleanup_at(false, "unit-released", now).unwrap();
    assert_eq!(released.removed_files, 1);
    assert_eq!(released.removed_bytes, b"leased bytes".len() as u64);
    assert!(!root.join(relative).exists());
}

#[test]
fn persistent_categories_and_unknown_files_are_protected_even_under_zero_age_policy() {
    let root = TestRoot::new("protected");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let protected = [
        (".ssmcp/inputs/original.txt", Category::PersistentInput),
        (".ssmcp/settings/source.toml", Category::Configuration),
        (".ssmcp/history/conversation.jsonl", Category::History),
        (".ssmcp/credentials/token", Category::Credential),
        (".ssmcp/artifacts/result.bin", Category::PersistentArtifact),
        ("user-notes.txt", Category::UserFile),
    ];
    let mut now = 0_u64;
    for (relative, category) in &protected {
        now = now.max(register(&manager, &root, relative, b"keep registered", *category));
    }
    now = now.max(register(&manager, &root, ".ssmcp/temporary/disposable.bin",
                           b"delete temporary", Category::Temporary));
    drop(root.create(".ssmcp/temporary/unknown.bin", b"keep unknown"));
    expire_immediately(&manager);
    let summary = manager.cleanup_at(false, "unit-protected", now + 1).unwrap();
    assert_eq!(summary.removed_files, 1);
    assert_eq!(summary.removed_bytes, b"delete temporary".len() as u64);
    assert_eq!(summary.kept_protected, protected.len() as u64);
    for (relative, _) in protected {
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"keep registered");
    }
    assert_eq!(fs::read(root.join(".ssmcp/temporary/unknown.bin")).unwrap(), b"keep unknown");
}

#[test]
fn dry_run_disabled_policy_and_component_rules_cannot_delete_an_unselected_file() {
    let root = TestRoot::new("rules");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let selected = ".ssmcp/temporary/selected/item.bin";
    let sibling = ".ssmcp/temporary/selected-other/item.bin";
    let now = register(&manager, &root, selected, b"selected", Category::Temporary)
        .max(register(&manager, &root, sibling, b"sibling", Category::Temporary)) + 1;
    let mut config = manager.config().unwrap();
    config.max_age_seconds = 86_400;
    config.rules = vec![CleanupRule {
        relative_prefix: ".ssmcp/temporary/selected".into(),
        category: Category::Temporary,
        max_age_seconds: Some(0),
    }];
    manager.set_config(&config).unwrap();
    let preview = manager.cleanup_at(true, "unit-preview", now).unwrap();
    assert_eq!(preview.removed_files, 0);
    assert_eq!(preview.removed_bytes, 0);
    assert_eq!(preview.would_remove_files, 1);
    assert_eq!(preview.would_remove_bytes, b"selected".len() as u64);
    assert_eq!(fs::read(root.join(selected)).unwrap(), b"selected");
    assert_eq!(fs::read(root.join(sibling)).unwrap(), b"sibling");

    config.enabled = false;
    manager.set_config(&config).unwrap();
    assert_eq!(manager.cleanup_at(false, "unit-disabled", now).unwrap().removed_files, 0);
    assert_eq!(fs::read(root.join(selected)).unwrap(), b"selected");
    config.enabled = true;
    manager.set_config(&config).unwrap();
    let removed = manager.cleanup_at(false, "unit-selected", now).unwrap();
    assert_eq!(removed.removed_files, 1);
    assert_eq!(removed.removed_bytes, b"selected".len() as u64);
    assert!(!root.join(selected).exists());
    assert_eq!(fs::read(root.join(sibling)).unwrap(), b"sibling");

    let previous = manager.config().unwrap();
    for prefix in ["../escape", ".ssmcp/temporary/../../escape"] {
        let mut invalid = manager.config().unwrap();
        invalid.rules[0].relative_prefix = prefix.into();
        assert!(manager.set_config(&invalid).is_err());
        assert_eq!(manager.config().unwrap(), previous);
    }
    fs::write(root.join(".ssmcp").join(CONFIG), b"{ malformed configuration").unwrap();
    assert!(manager.config().is_err());
    assert!(manager.cleanup_at(false, "unit-malformed-config", now + 86_400).is_err());
    assert_eq!(fs::read(root.join(sibling)).unwrap(), b"sibling");
}

#[test]
fn replaced_files_parent_directories_and_added_hard_links_are_never_deleted() {
    let root = TestRoot::new("identities");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let replaced = ".ssmcp/temporary/replaced.bin";
    let nested = ".ssmcp/temporary/parent/nested.bin";
    let linked = ".ssmcp/temporary/linked.bin";
    let now = [replaced, nested, linked].into_iter().map(|relative| {
        register(&manager, &root, relative, b"original bytes", Category::Temporary)
    }).max().unwrap() + 1;
    let nested_identity = FileIdentity::from_file(&File::open(root.join(nested)).unwrap()).unwrap();
    fs::rename(root.join(replaced), root.join(".ssmcp/temporary/replaced-original.bin")).unwrap();
    drop(root.create(replaced, b"original bytes"));
    fs::rename(root.join(".ssmcp/temporary/parent"),
               root.join(".ssmcp/temporary/parent-original")).unwrap();
    private_directory(&root.join(".ssmcp/temporary/parent")).unwrap();
    fs::rename(root.join(".ssmcp/temporary/parent-original/nested.bin"), root.join(nested)).unwrap();
    assert_eq!(FileIdentity::from_file(&File::open(root.join(nested)).unwrap()).unwrap(),
               nested_identity, "this case must change the parent, not the recorded file identity");
    fs::hard_link(root.join(linked), root.join(".ssmcp/temporary/second-link.bin")).unwrap();
    expire_immediately(&manager);
    let summary = manager.cleanup_at(false, "unit-identity", now).unwrap();
    assert_eq!(summary.removed_files, 0);
    assert_eq!(summary.removed_bytes, 0);
    assert!(summary.kept_changed >= 3);
    for relative in [replaced, linked, ".ssmcp/temporary/replaced-original.bin",
                     ".ssmcp/temporary/second-link.bin"] {
        assert_eq!(fs::read(root.join(relative)).unwrap(), b"original bytes");
    }
    assert_eq!(fs::read(root.join(nested)).unwrap(), b"original bytes");
    assert!(root.join(".ssmcp/temporary/parent-original").is_dir());
}

#[cfg(unix)]
#[test]
fn symlink_replacements_of_a_file_or_parent_never_reach_their_targets() {
    use std::os::unix::fs::symlink;

    let root = TestRoot::new("symlinks");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let leaf = ".ssmcp/temporary/leaf.bin";
    let nested = ".ssmcp/temporary/parent/nested.bin";
    let now = register(&manager, &root, leaf, b"original leaf", Category::Temporary)
        .max(register(&manager, &root, nested, b"original nested", Category::Temporary)) + 1;
    fs::rename(root.join(leaf), root.join(".ssmcp/temporary/leaf-original.bin")).unwrap();
    drop(root.create("external/target.bin", b"external leaf"));
    symlink(root.join("external/target.bin"), root.join(leaf)).unwrap();
    fs::rename(root.join(".ssmcp/temporary/parent"),
               root.join(".ssmcp/temporary/parent-original")).unwrap();
    drop(root.create("external/nested.bin", b"external nested"));
    symlink(root.join("external"), root.join(".ssmcp/temporary/parent")).unwrap();
    expire_immediately(&manager);
    let summary = manager.cleanup_at(false, "unit-symlink", now).unwrap();
    assert_eq!(summary.removed_files, 0);
    assert_eq!(summary.removed_bytes, 0);
    assert!(summary.kept_changed >= 2);
    assert!(fs::symlink_metadata(root.join(leaf)).unwrap().file_type().is_symlink());
    assert!(fs::symlink_metadata(root.join(".ssmcp/temporary/parent")).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(root.join("external/target.bin")).unwrap(), b"external leaf");
    assert_eq!(fs::read(root.join("external/nested.bin")).unwrap(), b"external nested");
    assert_eq!(fs::read(root.join(".ssmcp/temporary/leaf-original.bin")).unwrap(), b"original leaf");
    assert_eq!(fs::read(root.join(".ssmcp/temporary/parent-original/nested.bin")).unwrap(), b"original nested");
}

#[test]
fn concurrent_cleanup_instances_remove_and_count_each_file_only_once() {
    let root = TestRoot::new("competing");
    let manager = ManagedFiles::open(root.path()).unwrap();
    let relative = ".ssmcp/temporary/once.bin";
    let now = register(&manager, &root, relative, b"one removal", Category::Temporary) + 1;
    expire_immediately(&manager);
    let competitors = [
        ManagedFiles::open(root.path()).unwrap(),
        ManagedFiles::open(root.path()).unwrap(),
    ];
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = competitors.into_iter().map(|manager| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            manager.cleanup_at(false, "unit-competing", now)
        })
    }).collect();
    barrier.wait();
    let mut removed_files = 0_u64;
    let mut removed_bytes = 0_u64;
    for worker in workers {
        let summary = worker.join().expect("cleanup thread must not panic")
            .expect("a contending instance returns a busy summary, not an error");
        assert!(summary.errors.is_empty());
        if summary.busy {
            assert_eq!(summary.removed_files, 0);
            assert_eq!(summary.removed_bytes, 0);
        }
        removed_files += summary.removed_files;
        removed_bytes += summary.removed_bytes;
    }
    assert_eq!(removed_files, 1);
    assert_eq!(removed_bytes, b"one removal".len() as u64);
    assert!(!root.join(relative).exists());
}

#[test]
fn creation_hook_rejects_a_replaced_path_even_when_its_bytes_are_unchanged() {
    let root = TestRoot::new("creation-hook");
    let relative = ".ssmcp/temporary/hook.bin";
    let file = root.create(relative, b"same bytes");
    let identity = FileIdentity::from_file(&file).unwrap();
    drop(file);
    fs::rename(root.join(relative), root.join(".ssmcp/temporary/hook-original.bin")).unwrap();
    drop(root.create(relative, b"same bytes"));
    assert!(record_created(root.path(), Path::new(relative), Category::Temporary,
                           "creation-hook fixture", &identity).is_err());
    let summary = cleanup(root.path(), false, "unit-creation-hook").unwrap();
    assert_eq!(summary.removed_files, 0);
    assert_eq!(fs::read(root.join(relative)).unwrap(), b"same bytes");
    assert_eq!(fs::read(root.join(".ssmcp/temporary/hook-original.bin")).unwrap(), b"same bytes");
}

#[test]
fn cleanup_without_a_registry_preserves_unknown_files_and_does_not_create_metadata() {
    let root = TestRoot::new("no-registry");
    drop(root.create("notes.txt", b"user-owned notes"));
    let summary = cleanup(root.path(), false, "unit-no-registry").unwrap();
    assert_eq!(summary.removed_files, 0);
    assert_eq!(summary.removed_bytes, 0);
    assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"user-owned notes");
    assert!(!root.join(".ssmcp").exists());
}

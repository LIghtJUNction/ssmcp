use super::*;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Barrier},
};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: std::env::temp_dir().join(format!(
                "ssmcp-legacy-switch-test-{}-{}",
                std::process::id(),
                generation_text(generation().unwrap())
            )),
        }
    }

    fn store(&self) -> Store {
        Store::at(self.root.clone())
    }

    fn file(&self, name: &str, bytes: &[u8]) -> File {
        if !self.root.exists() {
            crate::platform::private_directory(&self.root).unwrap();
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.root.join(name)).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        file
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn missing_status_never_initializes_legacy_state() {
    let fixture = Fixture::new();
    let status = read_status(&fixture.store());
    assert_eq!(status["mode"], "unset");
    assert_eq!(status["available"], false);
    assert_eq!(status["generation"], Value::Null);
    assert_eq!(status["compatibility_only"], true);
    assert_eq!(status["controls_this_version"], false);
    assert_eq!(status["standalone_zuse_controlled"], false);
    assert!(!fixture.root.exists());
    assert!(Store::shared().is_err(), "tests cannot select the operator's state");
}

#[test]
fn old_schema_noop_is_byte_stable_and_off_on_uses_new_generations() {
    let fixture = Fixture::new();
    // Exactly the three-field schema written by the older mutation providers.
    let original = serde_json::to_vec(&json!({
        "schema_version": 1, "enabled": true, "generation": vec![17_u8; 16]
    }))
    .unwrap();
    fixture.file("state.json", &original);
    let store = fixture.store();
    let before = store.read().unwrap().unwrap();
    assert!(before.enabled);
    assert_eq!(before.generation, [17; 16]);
    assert_eq!(store.update(Some(true)).unwrap(), before);
    assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), original);

    let off = store.update(Some(false)).unwrap();
    let on = store.update(Some(true)).unwrap();
    assert!(!off.enabled);
    assert!(on.enabled);
    assert_ne!(off.generation, before.generation);
    assert_ne!(on.generation, before.generation);
    assert_ne!(on.generation, off.generation);
    let wire: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("state.json")).unwrap()).unwrap();
    assert_eq!(wire.as_object().unwrap().len(), 3);
    assert_eq!(wire["schema_version"], 1);
    assert_eq!(wire["generation"].as_array().unwrap().len(), 16);
    assert_eq!(store.read().unwrap(), Some(on));
}

#[test]
fn explicit_off_repairs_corruption_but_on_and_toggle_refuse() {
    let fixture = Fixture::new();
    let broken = b"{interrupted legacy state";
    fixture.file("state.json", broken);
    let store = fixture.store();
    assert_eq!(read_status(&store)["mode"], "unavailable");
    assert!(store.update(Some(true)).is_err());
    assert!(store.update(None).is_err());
    assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), broken.to_vec());
    let repaired = store.update(Some(false)).unwrap();
    assert!(!repaired.enabled);
    assert_eq!(store.read().unwrap(), Some(repaired));
}

#[test]
fn simultaneous_compatibility_toggles_do_not_lose_updates() {
    let fixture = Fixture::new();
    fixture.store().update(Some(false)).unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let root = fixture.root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Store::at(root).update(None).unwrap()
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|state| state.enabled).count(), 2);
    assert!(!fixture.store().read().unwrap().unwrap().enabled);
    let generations: std::collections::BTreeSet<_> =
        results.iter().map(|state| state.generation).collect();
    assert_eq!(generations.len(), 4);
}

#[test]
fn busy_state_status_is_bounded_and_preserves_the_record() {
    let fixture = Fixture::new();
    fixture.store().update(Some(true)).unwrap();
    let path = fixture.root.join("state.json");
    let original = fs::read(&path).unwrap();
    let held = OpenOptions::new().read(true).write(true).open(path).unwrap();
    crate::platform::lock_file(&held, true).unwrap();
    let status = read_status(&fixture.store());
    assert_eq!(status["mode"], "unavailable");
    assert!(status["error"].as_str().unwrap().contains("timed out"));
    assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), original);
    drop(held);
}

#[cfg(unix)]
#[test]
fn off_preserves_state_and_input_lock_inodes_without_taking_input_lease() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    fixture.store().update(Some(true)).unwrap();
    let state_path = fixture.root.join("state.json");
    let input_path = fixture.root.join("input.lock");
    let input = fixture.file("input.lock", b"existing legacy lease");
    crate::platform::lock_file(&input, true).unwrap();
    let state_before = fs::metadata(&state_path).unwrap();
    let input_before = fs::metadata(&input_path).unwrap();

    let stopped = fixture.store().update(Some(false)).unwrap();
    assert!(!stopped.enabled);
    let state_after = fs::metadata(&state_path).unwrap();
    let input_after = fs::metadata(&input_path).unwrap();
    assert_eq!(
        (state_before.dev(), state_before.ino()),
        (state_after.dev(), state_after.ino())
    );
    assert_eq!(
        (input_before.dev(), input_before.ino()),
        (input_after.dev(), input_after.ino())
    );
    assert_eq!(fs::read(input_path).unwrap(), b"existing legacy lease".to_vec());
    drop(input);
}

#[cfg(unix)]
#[test]
fn compatibility_store_rejects_links_and_non_private_storage_even_for_off() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let fixture = Fixture::new();
    fixture.store().update(Some(true)).unwrap();
    let path = fixture.root.join("state.json");

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.store().read().is_err());
    assert!(fixture.store().update(Some(false)).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let alias = fixture.root.join("linked.json");
    fs::hard_link(&path, &alias).unwrap();
    assert!(fixture.store().read().is_err());
    assert!(fixture.store().update(Some(false)).is_err());
    fs::remove_file(alias).unwrap();

    let saved = fixture.root.join("saved.json");
    fs::rename(&path, &saved).unwrap();
    symlink(&saved, &path).unwrap();
    assert!(fixture.store().read().is_err());
    assert!(fixture.store().update(Some(false)).is_err());
    fs::remove_file(&path).unwrap();
    fs::rename(saved, &path).unwrap();

    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(fixture.store().read().is_err());
    assert!(fixture.store().update(Some(false)).is_err());
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn oversized_unknown_schema_and_zero_on_generation_fail_closed() {
    for bytes in [
        vec![b'x'; 1025],
        serde_json::to_vec(&json!({
            "schema_version": 2, "enabled": false, "generation": vec![0_u8; 16]
        }))
        .unwrap(),
        serde_json::to_vec(&json!({
            "schema_version": 1, "enabled": true, "generation": vec![0_u8; 16]
        }))
        .unwrap(),
    ] {
        let fixture = Fixture::new();
        fixture.file("state.json", &bytes);
        let store = fixture.store();
        assert!(store.read().is_err());
        assert!(store.update(Some(true)).is_err());
        assert!(store.update(None).is_err());
        let status = read_status(&store);
        assert_eq!(status["mode"], "unavailable");
        assert_eq!(status["enabled"], false);
        assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), bytes);
    }
}

use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) const MAX_ENTRIES: usize = 256;
const LIVE: &str = "live";
const MANIFEST: &str = "overview.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Live {
    format: u32,
    instance: String,
    pid: u32,
    process_start_ticks: Option<u64>,
    metadata: crate::wire::ClientMetadata,
}

pub(crate) struct Registration {
    directory: PathBuf,
    instance: String,
    handle: Option<attach::Handle>,
}

impl Registration {
    pub async fn open(metadata: crate::wire::ClientMetadata) -> crate::Result<Self> {
        metadata.validate()?;
        let parent = super::super::session_root(true)?.join(LIVE);
        super::super::create_private_tree(&parent)?;
        super::super::verify_directory(&parent)?;
        let instance = super::super::random_suffix()?;
        let directory = parent.join(&instance);
        crate::platform::private_directory(&directory)?;
        super::super::verify_directory(&directory)?;
        let mut registration = Self {
            directory,
            instance,
            handle: None,
        };
        // Publish only after the private endpoint has been created successfully.
        registration.handle = Some(attach::serve(registration.directory.clone(), None).await?);
        let pid = std::process::id();
        super::super::write_json(
            &registration.directory.join(MANIFEST),
            &Live {
                format: 1,
                instance: registration.instance.clone(),
                pid,
                process_start_ticks: metrics::sample(pid).map(|sample| sample.start_ticks),
                metadata,
            },
        )?;
        Ok(registration)
    }

    pub fn handle(&self) -> &attach::Handle {
        self.handle.as_ref().expect("published attach handle")
    }

    pub async fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().await;
        }
        self.remove_manifest();
        // These are our private endpoint's lock and directory, never a user tree.
        let _ = super::super::remove_file(&self.directory.join("attach.endpoint-lock"));
        let _ = fs::remove_dir(&self.directory);
    }

    fn remove_manifest(&self) {
        if read_live(&self.directory).is_ok_and(|value| value.instance == self.instance) {
            let _ = super::super::remove_file(&self.directory.join(MANIFEST));
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        drop(self.handle.take());
        self.remove_manifest();
        let _ = fs::remove_dir(&self.directory);
    }
}

#[derive(Clone)]
pub(super) struct Entry {
    pub target: String,
    pub kind: &'static str,
    pub name: String,
    pub directory: PathBuf,
    pub pid: Option<u32>,
    pub process_start_ticks: Option<u64>,
    pub stopped: bool,
}

impl Entry {
    pub fn empty(&self, state: &str) -> Value {
        json!({"target":self.target,"kind":self.kind,"name":self.name,
            "state":state,"pid":self.pid,"client_id":null,"generation":null,
            "registered_start_ticks":self.process_start_ticks.map(|value|value.to_string()),
            "metrics":null,"agents":null,"workers":null,"executions":null,
            "commands":[],"logs_omitted":true,"stop_requested":self.stopped})
    }
}

pub(super) struct Inventory {
    pub entries: Vec<Entry>,
    pub skipped: usize,
    pub truncated: bool,
}

fn read_live(directory: &Path) -> crate::Result<Live> {
    super::super::verify_directory(directory)?;
    let value: Live = serde_json::from_slice(&super::super::read_private(
        &directory.join(MANIFEST),
        16 * 1024,
    )?)?;
    if value.format != 1
        || value.pid == 0
        || !valid_instance(&value.instance)
        || directory.file_name().and_then(|v| v.to_str()) != Some(value.instance.as_str())
    {
        return Err("live session registration does not match its private directory".into());
    }
    value.metadata.validate()?;
    Ok(value)
}

fn valid_instance(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn scan() -> crate::Result<Inventory> {
    match super::super::session_root(false) {
        Ok(root) => scan_in(&root),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(Inventory {
                entries: vec![],
                skipped: 0,
                truncated: false,
            })
        }
        Err(error) => Err(error),
    }
}

fn scan_in(root: &Path) -> crate::Result<Inventory> {
    super::super::verify_directory(root)?;
    let mut inventory = Inventory {
        entries: vec![],
        skipped: 0,
        truncated: false,
    };
    let mut scanned = 0;
    for entry in fs::read_dir(root)? {
        scanned += 1;
        if scanned > 4096 {
            inventory.truncated = true;
            break;
        }
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("session-") {
            continue;
        }
        let found = (|| -> crate::Result<Entry> {
            super::super::verify_directory(&entry.path())?;
            let manifest: super::super::Manifest =
                super::super::read_json(&entry.path().join("manifest.json"))?;
            let (directory, manifest) = super::super::load(root, &manifest.spec.name)?;
            if directory != entry.path() {
                return Err("saved session entry does not match its manifest".into());
            }
            let stopped = match fs::symlink_metadata(directory.join("stop.requested")) {
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true,
            };
            Ok(Entry {
                target: format!("saved:{}", manifest.spec.name),
                kind: "saved",
                name: manifest.spec.name,
                directory,
                pid: None,
                process_start_ticks: None,
                stopped,
            })
        })();
        add(&mut inventory, found);
        if inventory.entries.len() >= MAX_ENTRIES {
            inventory.truncated = true;
            break;
        }
    }
    let live = root.join(LIVE);
    match fs::symlink_metadata(&live) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => {
            super::super::verify_directory(&live)?;
            for entry in fs::read_dir(&live)? {
                scanned += 1;
                if scanned > 4096 || inventory.entries.len() >= MAX_ENTRIES {
                    inventory.truncated = true;
                    break;
                }
                let entry = entry?;
                let found = read_live(&entry.path()).map(|value| Entry {
                    target: format!("live:{}", value.instance),
                    kind: "foreground",
                    name: value.metadata.name,
                    directory: entry.path(),
                    pid: Some(value.pid),
                    process_start_ticks: value.process_start_ticks,
                    stopped: false,
                });
                add(&mut inventory, found);
            }
        }
    }
    inventory.entries.sort_by(|a, b| a.target.cmp(&b.target));
    Ok(inventory)
}

fn add(inventory: &mut Inventory, entry: crate::Result<Entry>) {
    match entry {
        Ok(entry) => inventory.entries.push(entry),
        Err(_) => inventory.skipped += 1,
    }
}

pub(super) fn select(target: &str) -> crate::Result<Entry> {
    let root = super::super::session_root(false)?;
    if let Some(name) = target.strip_prefix("saved:") {
        let (directory, manifest) = super::super::load(&root, name)?;
        let stopped = fs::symlink_metadata(directory.join("stop.requested")).is_ok();
        return Ok(Entry {
            target: target.into(),
            kind: "saved",
            name: manifest.spec.name,
            directory,
            pid: None,
            process_start_ticks: None,
            stopped,
        });
    }
    if let Some(instance) = target
        .strip_prefix("live:")
        .filter(|value| valid_instance(value))
    {
        let parent = root.join(LIVE);
        super::super::verify_directory(&parent)?;
        let directory = parent.join(instance);
        let value = read_live(&directory)?;
        return Ok(Entry {
            target: target.into(),
            kind: "foreground",
            name: value.metadata.name,
            directory,
            pid: Some(value.pid),
            process_start_ticks: value.process_start_ticks,
            stopped: false,
        });
    }
    Err("select an exact saved:NAME or live:INSTANCE target from ssmcp overview".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_selectors_cannot_be_paths_or_names() {
        assert!(valid_instance(&"a".repeat(32)));
        for value in [
            "",
            "../work",
            "same-name",
            "a/b",
            "/tmp/client",
            "*",
            "a%2fb",
        ] {
            assert!(!valid_instance(value));
        }
    }
}

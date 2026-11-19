//! Explicit replacement of the recovery image, independent of saved sessions.
use super::*;
use sha2::{Digest, Sha256};
use tokio::time::sleep;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn private_image(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.permissions().mode() & 0o100 == 0
    {
        return Err("007 recovery image must be a private, user-owned executable file".into());
    }
    Ok(file)
}

fn copy_image(mut source: File, destination: &Path) -> Result<String> {
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?;
    let result = (|| {
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 32 * 1024];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            target.write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
        }
        target.sync_all()?;
        Ok(format!("{:x}", digest.finalize()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

struct Prepared {
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    original: Identity,
    candidate: Identity,
    previous_sha256: String,
    candidate_sha256: String,
    activated: bool,
}

impl Prepared {
    fn new(root: &Path, source: &Path) -> Result<Self> {
        let target = root.join("ssmcp");
        let original_file = private_image(&target)?;
        let original = Identity::of(&original_file.metadata()?);
        let mut random = [0u8; 8];
        crate::platform::random_bytes(&mut random)?;
        let suffix = format!(
            "{}-{:016x}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            u64::from_ne_bytes(random)
        );
        let backup = root.join(format!("ssmcp.backup-{suffix}"));
        let staged = root.join(format!("ssmcp.upgrade-{suffix}"));
        // A backup is a separate file, not a rename that consumes the old image.
        let previous_sha256 = copy_image(original_file, &backup)?;
        let candidate_sha256 = match copy_image(File::open(source)?, &staged) {
            Ok(digest) => digest,
            Err(error) => {
                let _ = fs::remove_file(&staged);
                return Err(error);
            }
        };
        let candidate = Identity::of(&fs::metadata(&staged)?);
        File::open(root)?.sync_all()?;
        Ok(Self {
            target,
            staged,
            backup,
            original,
            candidate,
            previous_sha256,
            candidate_sha256,
            activated: false,
        })
    }

    fn activate(&mut self) -> Result<()> {
        if Identity::of(&private_image(&self.target)?.metadata()?) != self.original {
            return Err(
                "007 recovery image changed before replacement; inspect it before retrying".into(),
            );
        }
        fs::rename(&self.staged, &self.target)?;
        self.activated = true;
        File::open(self.target.parent().ok_or("missing recovery directory")?)?.sync_all()?;
        Ok(())
    }

    fn restore(&mut self) -> Result<Identity> {
        if !self.activated {
            if Identity::of(&private_image(&self.target)?.metadata()?) != self.original {
                return Err("007 recovery image changed before activation; backup retained without overwriting it".into());
            }
            return Ok(self.original);
        }
        if Identity::of(&private_image(&self.target)?.metadata()?) != self.candidate {
            return Err("007 recovery image changed after replacement; backup retained without overwriting it".into());
        }
        let digest = copy_image(private_image(&self.backup)?, &self.staged)?;
        if digest != self.previous_sha256 {
            return Err("007 recovery backup checksum changed; inspect the retained backup".into());
        }
        let restored = Identity::of(&fs::metadata(&self.staged)?);
        fs::rename(&self.staged, &self.target)?;
        self.activated = false;
        File::open(self.target.parent().ok_or("missing recovery directory")?)?.sync_all()?;
        Ok(restored)
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // Retain every backup, including after rollback. Only our staging file is temporary.
        let _ = fs::remove_file(&self.staged);
    }
}

async fn service() -> Result<Value> {
    let output = systemctl(&[
        "show",
        UNIT,
        "--property=LoadState,ActiveState,SubState,MainPID,ExecMainStatus",
    ])
    .await?;
    let mut value = serde_json::Map::new();
    for line in output.stdout.lines() {
        if let Some((key, text)) = line.split_once('=') {
            value.insert(key.into(), json!(text));
        }
    }
    Ok(Value::Object(value))
}

fn pid(service: &Value) -> u64 {
    service["MainPID"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn connected_observation(
    service: &Value,
    observation: &Value,
    previous_pid: u64,
    since: u64,
) -> bool {
    let current_pid = pid(service);
    service["ActiveState"] == "active"
        && current_pid != 0
        && current_pid != previous_pid
        && observation["pid"].as_u64() == Some(current_pid)
        && observation["observed_at"]
            .as_u64()
            .is_some_and(|time| time >= since)
        && observation["state"] == "connected"
        && observation["client_id"]
            .as_str()
            .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

async fn connected(root: &Path, image: Identity, previous_pid: u64, since: u64) -> Result<Value> {
    timeout(HEALTH_TIMEOUT, async {
        loop {
            let service = service().await?;
            if let Ok(observation) = crate::session::read_json::<Value>(&root.join("state.json")) {
                let current_pid = pid(&service);
                let actual_image = fs::metadata(format!("/proc/{current_pid}/exe"))
                    .ok().map(|metadata| Identity::of(&metadata));
                if actual_image == Some(image)
                    && connected_observation(&service, &observation, previous_pid, since)
                {
                    return Ok(json!({"service":service,"connection_observation":observation}));
                }
                if current_pid != 0 && current_pid != previous_pid
                    && observation["pid"].as_u64() == Some(current_pid)
                    && observation["observed_at"].as_u64().is_some_and(|time| time >= since)
                    && observation["last_error"].as_str().is_some_and(|error| error.starts_with("007 registration rejected:"))
                {
                    return Err("007 replacement registration was rejected; inspect its authorization and protocol".into());
                }
            }
            if service["ActiveState"] == "failed" {
                return Err("007 replacement service failed before connecting".into());
            }
            sleep(Duration::from_millis(500)).await;
        }
    }).await.map_err(|_| "007 replacement did not confirm a fresh connected process within 65 seconds")?
}

pub(super) async fn run(root: &Path, unit: &Path, dry_run: bool) -> Result<Value> {
    let _lock = crate::session::lock(&root.join("install.lock"), true)?;
    let definition = read_unit(unit, root)?.ok_or("007 user service is not installed")?;
    let expected = format!(
        "ExecStart={} 007 run",
        systemd_quote(text(&root.join("ssmcp"))?, true)?
    );
    if definition
        .lines()
        .filter(|line| line.starts_with("ExecStart="))
        .collect::<Vec<_>>()
        != [expected.as_str()]
    {
        return Err(
            "007 upgrade requires the owned recovery executable in its service definition".into(),
        );
    }
    let _config = load_config(root)?;
    let before = service().await?;
    if before["LoadState"] != "loaded" {
        return Err("007 user service is not loaded".into());
    }
    let previous_pid = pid(&before);
    let was_active = matches!(
        before["ActiveState"].as_str(),
        Some("active" | "activating")
    );
    let previous_version = program_version(&root.join("ssmcp")).await;
    let mut report = json!({
        "unit":UNIT,"dry_run":dry_run,"source":"current executable",
        "source_version":env!("CARGO_PKG_VERSION"),"source_wire_version":crate::wire::VERSION,
        "previous_version":previous_version,"previous_pid":previous_pid,
        "sessions_preserved":true,"configuration_preserved":true,"unit_definition_preserved":true,
    });
    if dry_run {
        report["success"] = json!(true);
        return Ok(report);
    }

    // Open the running CLI image, even when its installation path was replaced meanwhile.
    let mut prepared = Prepared::new(root, Path::new("/proc/self/exe"))?;
    report["backup"] = json!({"path":prepared.backup,"sha256":prepared.previous_sha256});
    report["candidate_sha256"] = json!(prepared.candidate_sha256);
    let mut interrupted = crate::platform::interrupt_signal()?;
    let mut terminated = crate::platform::terminate_signal()?;
    let since = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let activation = async {
        if read_unit(unit, root)?.as_deref() != Some(definition.as_str()) {
            return Err("007 service definition changed before upgrade".into());
        }
        systemctl(&["stop", UNIT]).await?;
        prepared.activate()?;
        systemctl(&["start", UNIT]).await?;
        connected(root, prepared.candidate, previous_pid, since).await
    };
    let result: Result<Value> = tokio::select! {
        result = activation => result,
        _ = interrupted.recv() => Err("007 upgrade interrupted; restoring the previous image".into()),
        _ = terminated.recv() => Err("007 upgrade terminated; restoring the previous image".into()),
    };
    match result {
        Ok(health) => {
            report["success"] = json!(true);
            report["new_pid"] = json!(pid(&health["service"]));
            report["connection_state"] = json!("connected");
            report["connection_observation"] = health["connection_observation"].clone();
        }
        Err(error) => {
            report["success"] = json!(false);
            report["error"] = json!(super::super::safe_error(&error.to_string()));
            let mut rollback =
                json!({"binary_restored":false,"service_started":false,"connected":false});
            let restored: Result<()> = async {
                // Do not turn a concurrent, externally changed unit into an unreviewed restart.
                if read_unit(unit, root)?.as_deref() != Some(definition.as_str()) {
                    return Err(
                        "007 service definition changed; backup retained for manual recovery"
                            .into(),
                    );
                }
                let stop = systemctl(&["stop", UNIT]).await;
                let image = prepared.restore()?;
                rollback["binary_restored"] = json!(true);
                stop?;
                if was_active {
                    let since = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    systemctl(&["start", UNIT]).await?;
                    rollback["service_started"] = json!(true);
                    let health = connected(root, image, previous_pid, since).await?;
                    rollback["connected"] = json!(true);
                    rollback["connection_observation"] = health["connection_observation"].clone();
                }
                Ok(())
            }
            .await;
            if let Err(error) = restored {
                rollback["error"] = json!(super::super::safe_error(&error.to_string()));
            }
            report["rollback"] = rollback;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            crate::platform::random_bytes(&mut bytes).unwrap();
            let path = std::env::temp_dir().join(format!(
                "ssmcp-007-upgrade-{}-{:x}",
                std::process::id(),
                u64::from_ne_bytes(bytes)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn executable(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&path)
                .unwrap();
            file.write_all(contents).unwrap();
            path
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn upgrade_image_retains_backup_and_restores_without_touching_session_files() {
        let directory = Directory::new();
        let old = directory.executable("ssmcp", b"previous recovery image");
        let source = directory.executable("candidate", b"new recovery image");
        fs::write(directory.0.join("config.json"), b"unchanged config").unwrap();
        fs::create_dir(directory.0.join("sessions")).unwrap();
        fs::write(directory.0.join("sessions/sentinel"), b"saved session").unwrap();
        let mut prepared = Prepared::new(&directory.0, &source).unwrap();
        prepared.activate().unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"new recovery image");
        assert_eq!(
            fs::read(&prepared.backup).unwrap(),
            b"previous recovery image"
        );
        prepared.restore().unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"previous recovery image");
        assert_eq!(
            fs::read(&prepared.backup).unwrap(),
            b"previous recovery image"
        );
        assert_eq!(
            fs::read(directory.0.join("config.json")).unwrap(),
            b"unchanged config"
        );
        assert_eq!(
            fs::read(directory.0.join("sessions/sentinel")).unwrap(),
            b"saved session"
        );
    }

    #[test]
    fn upgrade_refuses_a_changed_target_and_never_overwrites_it_on_rollback() {
        let directory = Directory::new();
        directory.executable("ssmcp", b"old");
        let source = directory.executable("candidate", b"new");
        let mut prepared = Prepared::new(&directory.0, &source).unwrap();
        prepared.activate().unwrap();
        let external = directory.executable("external", b"another operator's image");
        fs::rename(external, directory.0.join("ssmcp")).unwrap();
        assert!(prepared.restore().is_err());
        assert_eq!(
            fs::read(directory.0.join("ssmcp")).unwrap(),
            b"another operator's image"
        );
        assert_eq!(fs::read(&prepared.backup).unwrap(), b"old");
    }

    #[test]
    fn upgrade_health_rejects_old_process_old_observations_and_active_only() {
        let service = json!({"ActiveState":"active","MainPID":"202"});
        let observation =
            json!({"state":"connected","pid":202,"observed_at":50,"client_id":"a".repeat(64)});
        assert!(connected_observation(&service, &observation, 101, 50));
        assert!(!connected_observation(&service, &observation, 202, 50));
        assert!(!connected_observation(&service, &observation, 101, 51));
        let mut stale = observation.clone();
        stale["pid"] = json!(101);
        assert!(!connected_observation(&service, &stale, 101, 50));
        stale["pid"] = json!(202);
        stale["state"] = json!("connecting");
        assert!(!connected_observation(&service, &stale, 101, 50));
    }
}

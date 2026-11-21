use super::*;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

pub(super) struct Temporary(pub(super) PathBuf);

impl Temporary {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("ssmcp-zuse-cli-{}", storage::id().unwrap()));
        crate::platform::private_directory(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn binary(target: &str) -> Vec<u8> {
    let mut bytes = vec![0; 128];
    if target.ends_with("-unknown-linux-gnu") {
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        let machine = if target.starts_with("aarch64-") {
            183u16
        } else {
            62u16
        };
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    } else if target.ends_with("-apple-darwin") {
        bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        let cpu = if target.starts_with("aarch64-") {
            0x0100000cu32
        } else {
            0x01000007u32
        };
        bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
    } else {
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..70].copy_from_slice(b"PE\0\0\x64\x86");
    }
    bytes
}

fn release(target: &str, bytes: &[u8]) -> release::Release {
    let suffix = if target.ends_with("-windows-msvc") {
        ".zip"
    } else {
        ".tar.gz"
    };
    let name = format!("zuse-v0.3.0-{target}{suffix}");
    release::Release {
        version: "0.3.0".into(),
        tag: "v0.3.0".into(),
        target: target.into(),
        release_id: 1,
        asset_id: 2,
        asset_name: name.clone(),
        size: bytes.len() as u64,
        sha256: release::digest(bytes),
        url: format!("https://github.com/LIghtJUNction/zuse/releases/download/v0.3.0/{name}"),
    }
}

fn tar_members(target: &str, entries: &[(&str, &[u8], u32, tar::EntryType)]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for (name, bytes, mode, kind) in entries {
        let name = format!("zuse-v0.3.0-{target}/{name}");
        let mut header = tar::Header::new_ustar();
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_entry_type(*kind);
        header.set_mtime(0);
        header.set_path(&name).unwrap();
        header.set_cksum();
        archive.append(&header, Cursor::new(bytes)).unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}

fn zip_members(target: &str, entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes, mode) in entries {
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(*mode);
        archive
            .start_file(format!("zuse-v0.3.0-{target}/{name}"), options)
            .unwrap();
        archive.write_all(bytes).unwrap();
    }
    archive.finish().unwrap().into_inner()
}

fn package(target: &str) -> (release::Release, Vec<u8>, Vec<u8>) {
    let executable = binary(target);
    let bytes = if target.ends_with("-windows-msvc") {
        zip_members(
            target,
            &[
                ("zuse.exe", &executable, 0o755),
                ("README.md", b"release documentation", 0o644),
                ("examples/observe.zuse", b"schema()", 0o644),
            ],
        )
    } else {
        tar_members(
            target,
            &[
                ("zuse", &executable, 0o755, tar::EntryType::Regular),
                (
                    "README.md",
                    b"release documentation",
                    0o644,
                    tar::EntryType::Regular,
                ),
                (
                    "docs/language.md",
                    b"language",
                    0o644,
                    tar::EntryType::Regular,
                ),
            ],
        )
    };
    (release(target, &bytes), bytes, executable)
}

#[test]
fn official_asset_requires_exact_digest_size_tag_target_and_single_match() {
    let (receipt, bytes, _) = package("x86_64-unknown-linux-gnu");
    let asset = json!({
        "id": receipt.asset_id, "name": receipt.asset_name, "state":"uploaded",
        "size":receipt.size, "digest":format!("sha256:{}", receipt.sha256),
        "browser_download_url":receipt.url
    });
    let metadata = json!({
        "id":receipt.release_id, "tag_name":receipt.tag,
        "draft":false, "prerelease":false, "assets":[asset.clone()]
    });
    let select = |value: &Value| {
        release::select(
            &serde_json::to_vec(value).unwrap(),
            Some("0.3.0"),
            &receipt.target,
        )
    };
    assert_eq!(select(&metadata).unwrap(), receipt);
    let mut missing_digest = metadata.clone();
    missing_digest["assets"][0]["digest"] = Value::Null;
    assert!(select(&missing_digest).unwrap_err().contains("SHA256"));
    let mut duplicate = metadata.clone();
    duplicate["assets"] = json!([asset.clone(), asset]);
    assert!(select(&duplicate).is_err());
    let mut draft = metadata.clone();
    draft["draft"] = json!(true);
    assert!(select(&draft).is_err());
    let mut foreign_url = metadata;
    foreign_url["assets"][0]["browser_download_url"] = json!("https://example.com/zuse.tar.gz");
    assert!(select(&foreign_url).is_err());
    let mut changed = bytes.clone();
    changed[0] ^= 1;
    assert!(receipt.verify(&changed, &receipt.target).is_err());
    assert!(
        receipt
            .verify(&bytes[..bytes.len() - 1], &receipt.target)
            .is_err()
    );
    assert!(receipt.verify(&bytes, "aarch64-unknown-linux-gnu").is_err());
    assert!(release::version("../latest").is_err());
    assert!(release::version("0.3.0-beta").is_err());
}

#[test]
fn archives_extract_only_the_expected_regular_platform_executable() {
    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ] {
        let (receipt, bytes, executable) = package(target);
        assert_eq!(
            archive::executable(&bytes, &receipt, target).unwrap(),
            executable
        );
    }
}

#[test]
fn windows_zip_accepts_the_official_dos_creator_without_posix_execute_bits() {
    let target = "x86_64-pc-windows-msvc";
    let executable = binary(target);
    let mut bytes = zip_members(target, &[("zuse.exe", &executable, 0o755)]);
    let directory = zip::ZipArchive::new(Cursor::new(&bytes))
        .unwrap()
        .central_directory_start() as usize;
    // The high byte of "version made by" is the creator OS. This reproduces
    // Python ZipInfo on Windows: DOS creator, with Unix bits in external_attr.
    // The archive library still reads and validates all headers and payloads.
    bytes[directory + 5] = 0;
    let mut parsed = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
    assert_eq!(parsed.by_index(0).unwrap().unix_mode().unwrap() & 0o111, 0);
    let receipt = release(target, &bytes);
    assert_eq!(
        archive::executable(&bytes, &receipt, target).unwrap(),
        executable
    );
}

#[test]
fn tar_rejects_links_duplicate_executables_extra_commands_and_wrong_architecture() {
    let target = "x86_64-unknown-linux-gnu";
    let exe = binary(target);
    for entries in [
        vec![("zuse", &exe[..], 0o755, tar::EntryType::Symlink)],
        vec![
            ("zuse", &exe[..], 0o755, tar::EntryType::Regular),
            ("zuse", &exe[..], 0o755, tar::EntryType::Regular),
        ],
        vec![
            ("zuse", &exe[..], 0o755, tar::EntryType::Regular),
            ("extra-command", &exe[..], 0o755, tar::EntryType::Regular),
        ],
    ] {
        let bytes = tar_members(target, &entries);
        assert!(archive::executable(&bytes, &release(target, &bytes), target).is_err());
    }
    let wrong = binary("aarch64-unknown-linux-gnu");
    let bytes = tar_members(target, &[("zuse", &wrong, 0o755, tar::EntryType::Regular)]);
    assert!(archive::executable(&bytes, &release(target, &bytes), target).is_err());
}

#[test]
fn zip_rejects_extra_executables_unsafe_paths_and_damaged_payload() {
    let target = "x86_64-pc-windows-msvc";
    let exe = binary(target);
    for name in ["../escape", "folder\\escape", "second.exe"] {
        let bytes = zip_members(
            target,
            &[("zuse.exe", &exe, 0o755), (name, b"unwanted", 0o755)],
        );
        assert!(archive::executable(&bytes, &release(target, &bytes), target).is_err());
    }
    let (receipt, mut bytes, _) = package(target);
    bytes[0] ^= 1;
    // Give the malformed archive its own digest so structural validation,
    // rather than the outer digest check alone, must reject it.
    let malformed = release(target, &bytes);
    assert!(archive::executable(&bytes, &malformed, target).is_err());
    assert!(receipt.verify(&bytes, target).is_err());
}

fn installed(root: &Path) -> (storage::Store, storage::Installation, Vec<u8>) {
    let store = storage::Store::open(root, true).unwrap().unwrap();
    let (release, bytes, binary) = package(release::target().unwrap());
    let download = store.save_download(release, &bytes).unwrap();
    let installation = store.install(&download.download_id).unwrap();
    (store, installation, binary)
}

#[test]
fn install_is_private_atomic_and_never_changes_an_external_entry() {
    let temporary = Temporary::new();
    let external = temporary.0.join("external-zuse");
    fs::write(&external, b"external 0.4.0").unwrap();
    let root = temporary.0.join("manager");
    assert!(storage::Store::open(&root, false).unwrap().is_none());
    assert!(!root.exists());
    let (store, receipt, binary) = installed(&root);
    let path = store.executable(&receipt);
    assert_eq!(fs::read(&path).unwrap(), binary);
    assert_eq!(fs::read(&external).unwrap(), b"external 0.4.0");
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    assert!(
        fs::read_dir(root.join("installations"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pending"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(store.uninstall("../external-zuse").is_err());
    store.uninstall(&receipt.installation_id).unwrap();
    assert!(!path.exists());
    assert_eq!(fs::read(&external).unwrap(), b"external 0.4.0");
}

#[test]
fn uninstall_refuses_changed_bytes_replaced_identity_or_unrecorded_files() {
    let temporary = Temporary::new();
    let (store, receipt, binary) = installed(&temporary.0.join("manager"));
    let path = store.executable(&receipt);
    fs::write(&path, b"replacement").unwrap();
    assert!(store.uninstall(&receipt.installation_id).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    fs::write(&path, &binary).unwrap();
    let extra = path.parent().unwrap().join("user-file");
    fs::write(&extra, b"preserve").unwrap();
    assert!(store.uninstall(&receipt.installation_id).is_err());
    assert_eq!(fs::read(&extra).unwrap(), b"preserve");
    fs::remove_file(&extra).unwrap();
    let replacement = path.with_file_name("replacement");
    fs::write(&replacement, &binary).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::remove_file(&path).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(store.uninstall(&receipt.installation_id).is_err());
    assert_eq!(fs::read(&path).unwrap(), binary);
}

#[test]
fn canonical_owned_path_keeps_receipt_identity_and_external_paths_stay_unmanaged() {
    let temporary = Temporary::new();
    let (store, receipt, _) = installed(&temporary.0.join("manager"));
    let canonical = store.executable(&receipt).canonicalize().unwrap();
    let matched = store.matching(&canonical).unwrap().unwrap();
    assert_eq!(matched.installation_id, receipt.installation_id);
    let external = temporary.0.join("external-zuse");
    fs::write(&external, b"external").unwrap();
    assert!(
        store
            .matching(&external.canonicalize().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn uninstall_refuses_a_hard_link_even_when_identity_and_bytes_match() {
    let temporary = Temporary::new();
    let (store, receipt, binary) = installed(&temporary.0.join("manager"));
    let path = store.executable(&receipt);
    let external_link = temporary.0.join("linked-zuse");
    fs::hard_link(&path, &external_link).unwrap();
    assert!(store.uninstall(&receipt.installation_id).is_err());
    assert_eq!(fs::read(&path).unwrap(), binary);
    assert_eq!(fs::read(&external_link).unwrap(), binary);
    fs::remove_file(&external_link).unwrap();
    store.uninstall(&receipt.installation_id).unwrap();
    assert!(!path.exists());
}

#[test]
fn install_rechecks_cached_download_before_publishing_any_executable() {
    let temporary = Temporary::new();
    let root = temporary.0.join("manager");
    let store = storage::Store::open(&root, true).unwrap().unwrap();
    let (release, bytes, _) = package(release::target().unwrap());
    let receipt = store.save_download(release, &bytes).unwrap();
    fs::write(
        root.join("downloads")
            .join(&receipt.download_id)
            .join("archive"),
        b"changed",
    )
    .unwrap();
    assert!(store.install(&receipt.download_id).is_err());
    assert_eq!(fs::read_dir(root.join("installations")).unwrap().count(), 0);
}

#[cfg(unix)]
fn fake_cli(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(
        path,
        format!("#!/bin/sh\n[ \"$#\" -eq 1 ] && [ \"$1\" = --version ] || exit 22\n{body}\n"),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn version_queries_the_actual_fake_cli_and_rejects_wrong_identity() {
    let temporary = Temporary::new();
    let path = temporary.0.join("zuse");
    fake_cli(&path, "printf 'zuse 0.4.0\\n'");
    let actual = probe::version(&path, std::future::pending()).await.unwrap();
    assert_eq!(actual["version"], "0.4.0");
    assert_eq!(actual["sha256"], release::digest(&fs::read(&path).unwrap()));
    fake_cli(&path, "printf 'different-program 0.4.0\\n'");
    assert!(probe::version(&path, std::future::pending()).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn version_has_a_real_deadline_and_owned_process_cleanup() {
    let temporary = Temporary::new();
    let path = temporary.0.join("zuse");
    fake_cli(&path, "exec sleep 30");
    assert!(
        probe::version(&path, std::future::pending())
            .await
            .unwrap_err()
            .contains("deadline")
    );
}

#[tokio::test]
async fn only_four_lifecycle_tools_exist_and_legacy_actions_are_rejected() {
    let names = Computer::tools()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "ssmcp.computer.version",
            "ssmcp.computer.download",
            "ssmcp.computer.install",
            "ssmcp.computer.uninstall",
        ]
    );
    let temporary = Temporary::new();
    let computer = Computer {
        root: temporary.0.join("never-created"),
        operation: Mutex::new(()),
        closed: AtomicBool::new(false),
    };
    assert!(
        computer
            .perform("ssmcp.computer.click", json!({}))
            .await
            .is_err()
    );
    assert!(
        computer
            .perform("ssmcp.computer.version", json!({"path":"/tmp/arbitrary"}))
            .await
            .is_err()
    );
    assert!(!computer.root.exists());
    computer.shutdown().await.unwrap();
    assert!(
        computer
            .perform("ssmcp.computer.version", json!({}))
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn selected_external_entry_rejects_a_downloader_without_searching_another_cli() {
    let temporary = Temporary::new();
    let selected = temporary.0.join("first-zuse");
    let other = temporary.0.join("second-zuse");
    fake_cli(&selected, "exec npx zuse --version");
    fake_cli(&other, "printf 'zuse 0.4.0\\n'");
    let error = crate::program::selected_entry(&selected, "zuse").unwrap_err();
    assert!(error.contains("refusing to run a downloader"));
    assert_eq!(
        crate::program::selected_entry(&other, "zuse").unwrap(),
        other.canonicalize().unwrap()
    );
}

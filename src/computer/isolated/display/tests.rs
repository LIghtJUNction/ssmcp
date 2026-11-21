use super::super::{Desktop, Directory, OwnedX11Files, Start};
use super::*;
use std::os::{
    linux::net::SocketAddrExt,
    unix::{
        fs::{DirBuilderExt, PermissionsExt},
        net::{SocketAddr, UnixListener},
    },
};

fn private_directory() -> Directory {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).unwrap();
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let directory = Directory(std::env::temp_dir().join(format!("ssmcp-display-test-{suffix}")));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory.0)
        .unwrap();
    directory
}

#[test]
fn selection_stays_high_and_recognizes_host_aliases() {
    assert_eq!(candidate(END - 1, 1), FIRST);
    for start in [FIRST, FIRST + 23, END - 1] {
        for offset in 0..ATTEMPTS {
            assert!((FIRST..END).contains(&candidate(start, offset)));
        }
    }
    for (value, expected) in [
        (":0", Some(0)),
        (":10000.0", Some(10_000)),
        ("unix/:10001", Some(10_001)),
        ("host:10002_", Some(10_002)),
        (":10003_.0", Some(10_003)),
        (":", None),
        (":-1", None),
        (":65536", None),
        ("wayland-0", None),
    ] {
        assert_eq!(display_number(value), expected, "{value}");
    }
}

#[test]
fn dangling_alias_is_reserved_and_process_identity_requires_a_live_parent() {
    let directory = private_directory();
    let alias = directory.0.join("X10000_");
    assert!(!path_reserved(&alias).unwrap());
    std::os::unix::fs::symlink("missing-owned-fixture", &alias).unwrap();
    assert!(path_reserved(&alias).unwrap());
    assert_eq!(process_parent("12 (Xvfb) S 10 12 12 0"), Some(10));
    assert_eq!(process_parent("12 (odd ) command) S 10 12"), Some(10));
    assert_eq!(process_parent("12 (Xvfb) Z 10 12"), None);
    assert_eq!(process_parent("12 (Xvfb) X 10 12"), None);
    assert_eq!(process_parent("invalid"), None);
}

#[test]
fn allocation_lock_rejects_links_permissions_and_replaced_inodes() {
    let directory = private_directory();
    let first = AllocationLock::open(&directory.0).unwrap();
    first.file.try_lock().unwrap();
    let second = AllocationLock::open(&directory.0).unwrap();
    assert!(matches!(
        second.file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(first);
    second.file.try_lock().unwrap();
    let path = second.path.join("allocate.lock");
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "replacement").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(second.ensure_current().is_err());
    drop(second);
    assert!(
        AllocationLock::open(&directory.0).is_err(),
        "wrong permissions must not be repaired silently"
    );
    std::fs::remove_file(&path).unwrap();
    let target = directory.0.join("target");
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&target)
        .unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(AllocationLock::open(&directory.0).is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&target, &path).unwrap();
    assert!(AllocationLock::open(&directory.0).is_err());

    let other = private_directory();
    std::os::unix::fs::symlink(
        &directory.0,
        other.0.join(format!("ssmcp-x11-{}", effective_uid())),
    )
    .unwrap();
    assert!(AllocationLock::open(&other.0).is_err());
}

#[test]
fn allocation_lock_drop_releases_while_an_inherited_descriptor_stays_open() {
    let directory = private_directory();
    let first = AllocationLock::open(&directory.0).unwrap();
    first.file.try_lock().unwrap();
    // dup and fork retain the same open file description. Keeping this clone
    // alive makes the fork-before-exec window deterministic without a child.
    let inherited = first.file.try_clone().unwrap();
    let second = AllocationLock::open(&directory.0).unwrap();
    assert!(matches!(
        second.file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(first);
    second.file.try_lock().unwrap();
    assert!(inherited.metadata().unwrap().is_file());

    // Neither an unacquired guard nor the old owner's descriptor may release
    // the new owner's independent lock.
    drop(AllocationLock::open(&directory.0).unwrap());
    drop(inherited);
    let third = AllocationLock::open(&directory.0).unwrap();
    assert!(matches!(
        third.file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(second);
    third.file.try_lock().unwrap();
}

#[test]
fn allocation_lock_excludes_another_process() {
    let directory = private_directory();
    let lock = AllocationLock::open(&directory.0).unwrap();
    lock.file.try_lock().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "computer::isolated::display::tests::allocation_lock_subprocess_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("SSMCP_PRIVATE_DISPLAY_LOCK_TEST_ROOT", &directory.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    lock.ensure_current().unwrap();
}

#[test]
#[ignore = "subprocess fixture for a lock owned by the parent test"]
fn allocation_lock_subprocess_fixture() {
    let Some(root) = std::env::var_os("SSMCP_PRIVATE_DISPLAY_LOCK_TEST_ROOT") else {
        return;
    };
    let lock = AllocationLock::open(Path::new(&root)).unwrap();
    assert!(matches!(
        lock.file.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
}

#[tokio::test]
#[ignore = "requires installed private desktop helpers; creates only owned high displays"]
async fn real_private_displays_avoid_conflicts_concurrently_and_clean_up() {
    let root = private_directory();
    let environment: Vec<_> = ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XAUTHORITY"]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
    let (first, second) = tokio::join!(
        Desktop::start(
            &root.0,
            Start {
                width: 320,
                height: 240
            }
        ),
        Desktop::start(
            &root.0,
            Start {
                width: 320,
                height: 240
            }
        ),
    );
    let mut first = first.unwrap();
    let mut second = second.unwrap();
    let first_number =
        display_number(&first.computer.environment.as_ref().unwrap().display).unwrap();
    let second_number =
        display_number(&second.computer.environment.as_ref().unwrap().display).unwrap();
    assert!((FIRST..END).contains(&first_number));
    assert!((FIRST..END).contains(&second_number));
    assert_ne!(first_number, second_number);
    let occupied = Reservation::acquire_from(first_number).await.unwrap();
    assert_ne!(
        occupied.number(),
        first_number,
        "a live server is an occupied candidate"
    );
    assert_ne!(occupied.number(), second_number);
    drop(occupied);
    let paths = [first.directory.0.clone(), second.directory.0.clone()];
    let inodes: Vec<_> = first
        .x11_files
        .entries
        .iter()
        .chain(&second.x11_files.entries)
        .map(|entry| (entry.path.clone(), entry.device, entry.inode))
        .collect();
    assert_eq!(
        inodes.len(),
        2,
        "only the two proven filesystem sockets are cleanup-owned"
    );
    first.shutdown().await;
    second.shutdown().await;
    drop(first);
    drop(second);
    for path in paths {
        assert!(!path.exists(), "owned desktop directory must be gone");
    }
    for (path, device, inode) in inodes {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            assert_ne!(
                (metadata.dev(), metadata.ino()),
                (device, inode),
                "owned socket was retained"
            );
        }
    }
    for (key, value) in environment {
        assert_eq!(std::env::var_os(key), value, "{key}");
    }
}

#[tokio::test]
#[ignore = "Linux native socket/process ownership; only self-created high-range reservations"]
async fn real_aliases_and_unrelated_endpoints_are_never_adopted() {
    let reservation = Reservation::acquire(39_147).await.unwrap();
    let number = reservation.number();
    let alias = PathBuf::from(format!("{SOCKET_DIRECTORY}/X{number}_"));
    let filesystem = UnixListener::bind(&alias).unwrap();
    let mut files = OwnedX11Files::default();
    files.remember(alias.clone(), true);
    let address = SocketAddr::from_abstract_name(alias.as_os_str().as_bytes()).unwrap();
    let abstract_socket = UnixListener::bind_addr(&address).unwrap();
    drop(reservation);
    let next = Reservation::acquire_from(number).await.unwrap();
    assert_ne!(
        next.number(),
        number,
        "underscore filesystem alias must reserve its base number"
    );
    drop(next);
    drop(filesystem);
    drop(files);
    assert!(!alias.exists());
    let next = Reservation::acquire_from(number).await.unwrap();
    assert_ne!(
        next.number(),
        number,
        "abstract-only underscore alias must reserve its base number"
    );
    drop(next);
    drop(abstract_socket);

    let reservation = Reservation::acquire_from(number).await.unwrap();
    let lock_path = PathBuf::from(format!("/tmp/.X{}-lock", reservation.number()));
    let lock = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    let mut files = OwnedX11Files::default();
    files.remember(lock_path.clone(), false);
    let locked_number = reservation.number();
    drop(reservation);
    let next = Reservation::acquire_from(locked_number).await.unwrap();
    assert_ne!(
        next.number(),
        locked_number,
        "an existing PID lock is a reservation even if no socket exists"
    );
    drop(next);
    drop(lock);
    drop(files);
    assert!(!lock_path.exists());

    let reservation = Reservation::acquire_from(number).await.unwrap();
    let path = socket_path(reservation.number());
    let listener = UnixListener::bind(&path).unwrap();
    let mut files = OwnedX11Files::default();
    files.remember(path.clone(), true);
    let mut command = tokio::process::Command::new(crate::program::executable("sleep").unwrap());
    command.arg("20");
    let mut unrelated_child = Managed::spawn(&mut command).unwrap();
    let mut claimed = Vec::new();
    let error = reservation
        .verify(&mut unrelated_child, &mut claimed)
        .unwrap_err();
    assert!(error.contains("newly spawned Xvfb child"), "{error}");
    assert!(
        claimed.is_empty(),
        "a socket owned by another process must never be adopted for cleanup"
    );
    unrelated_child.shutdown().await;
    assert!(
        path.exists(),
        "rejecting ownership must preserve the unrelated endpoint"
    );
    drop(listener);
    drop(files);
    assert!(!path.exists());
    drop(reservation);

    // Force a real bind conflict only against our own high abstract endpoint.
    // Explicit :N plus -displayfd must exit here, never search down from :0;
    // -nopn must also reject a successful filesystem-only partial bind.
    let reservation = Reservation::acquire_from(number).await.unwrap();
    let number = reservation.number();
    let path = socket_path(number);
    let address = SocketAddr::from_abstract_name(path.as_os_str().as_bytes()).unwrap();
    let occupied = UnixListener::bind_addr(&address).unwrap();
    let directory = private_directory();
    let authority = directory.0.join("Xauthority");
    let mut cookie = [0u8; 16];
    crate::platform::random_bytes(&mut cookie).unwrap();
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&authority)
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        &super::super::authority_record(&number.to_string(), &cookie),
    )
    .unwrap();
    drop(file);
    let mut command = tokio::process::Command::new(crate::program::executable("Xvfb").unwrap());
    command
        .arg(format!(":{number}"))
        .args([
            "-displayfd",
            "1",
            "-nopn",
            "-screen",
            "0",
            "320x240x24",
            "-nolisten",
            "tcp",
            "-noreset",
            "-auth",
        ])
        .arg(&authority)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut conflict = Managed::spawn(&mut command).unwrap();
    let attempt = tokio::time::timeout(Duration::from_secs(5), async {
        use tokio::io::AsyncReadExt;
        let stdout = conflict.child.stdout.take().unwrap();
        let mut announcement = Vec::new();
        stdout.take(64).read_to_end(&mut announcement).await?;
        let status = conflict.wait().await?;
        Ok::<_, io::Error>((status, announcement))
    })
    .await;
    conflict.shutdown().await;
    let (status, announcement) = attempt
        .expect("explicit conflict must terminate within five seconds")
        .unwrap();
    assert!(
        !status.success(),
        "an occupied explicit display must reject startup"
    );
    assert!(
        announcement.is_empty(),
        "a conflict must not announce any fallback display: {announcement:?}"
    );
    let peer = connect_peer(&path, true).unwrap();
    assert_eq!(
        peer.credentials.pid as u32,
        std::process::id(),
        "the original abstract endpoint must still belong to this fixture"
    );
    assert!(
        !path_reserved(&path).unwrap(),
        "failed Xvfb startup must clean its partial filesystem endpoint"
    );
    drop(peer);
    drop(occupied);
    drop(reservation);
}

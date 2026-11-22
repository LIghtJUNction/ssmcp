#![cfg(target_os = "linux")]

#[path = "fixtures/managed_build_host.rs"]
mod host;

use host::{Build, Host};
use sha2::{Digest, Sha256};
use std::{os::unix::fs::DirBuilderExt, path::PathBuf};

struct Sandbox(PathBuf);
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup() -> (Sandbox, Host) {
    let compiler = std::env::var_os("SSMCP_FIXTURE_RUSTC")
        .expect("set SSMCP_FIXTURE_RUSTC to the absolute native rustc binary");
    let compiler = PathBuf::from(compiler);
    assert!(compiler.is_absolute());
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).unwrap();
    let root =
        std::env::temp_dir().join(format!("ssmcp-build-{:032x}", u128::from_ne_bytes(nonce)));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let host = Host::new(compiler, root.clone(), ssmcp::wire::MAX_INFLIGHT);
    (Sandbox(root), host)
}

const SOURCE: &str = "pub fn fixture() -> u8 { 42 }";
const BUILD: Build = Build {
    grant: 1,
    snapshot: SOURCE,
};

#[tokio::test]
#[ignore = "requires a native rustc binary via SSMCP_FIXTURE_RUSTC"]
async fn shares_one_compiler_and_verified_artifact_for_arbitrary_group_size() {
    for n in [1, 2, 5, 8] {
        let (_sandbox, mut host) = setup();
        let replies: Vec<_> = (0..n).map(|_| host.submit(BUILD).unwrap()).collect();
        host.finish(BUILD, true).await;
        let bytes = std::fs::read(host.root.join("0/build.o")).unwrap();
        let expected: [u8; 32] = Sha256::digest(bytes).into();
        for reply in replies {
            assert_eq!(reply.await.unwrap().unwrap(), expected);
        }
        assert_eq!((host.launches, host.avoided), (1, n - 1));
        let next = host.submit(BUILD).unwrap();
        host.finish(BUILD, true).await;
        next.await.unwrap().unwrap();
        assert_eq!((host.launches, host.avoided), (2, n - 1));
    }
}

#[tokio::test]
#[ignore = "requires a native rustc binary via SSMCP_FIXTURE_RUSTC"]
async fn separates_inputs_and_grants_and_excludes_failed_or_cancelled_delivery() {
    let (_sandbox, mut host) = setup();
    let other_grant = Build { grant: 2, ..BUILD };
    let other_source = Build {
        snapshot: "pub fn fixture() -> u8 { 7 }",
        ..BUILD
    };
    drop(host.submit(BUILD).unwrap());
    let survivor = host.submit(BUILD).unwrap();
    let revoked = host.submit(other_grant).unwrap();
    let distinct = host.submit(other_source).unwrap();
    let broken = Build {
        snapshot: "invalid Rust input",
        ..BUILD
    };
    let failures = [host.submit(broken).unwrap(), host.submit(broken).unwrap()];
    host.finish(BUILD, true).await;
    host.finish(other_grant, false).await;
    host.finish(other_source, true).await;
    host.finish(broken, true).await;
    assert_ne!(
        survivor.await.unwrap().unwrap(),
        distinct.await.unwrap().unwrap()
    );
    assert_eq!(revoked.await.unwrap().unwrap_err(), "grant revoked");
    for reply in failures {
        assert_eq!(reply.await.unwrap().unwrap_err(), "compiler failed");
    }
    assert_eq!((host.launches, host.avoided), (4, 0));

    drop(host.submit(BUILD).unwrap());
    host.finish(BUILD, true).await;
    assert!(!host.root.join("4/build.o").exists());
    assert_eq!((host.launches, host.avoided), (5, 0));
}

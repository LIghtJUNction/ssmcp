//! Give the Unix peer a registered account through the normal server admission path.
use serde_json::json;
use ssmcp::{
    broker::Broker,
    history::{AuditConfig, AuditLog},
    management::ManagementPolicy,
    portal::Portal,
};
use std::{
    collections::HashMap,
    fs::{self, DirBuilder, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    process::Command,
    sync::Arc,
};

pub struct Accounts(PathBuf);

impl Accounts {
    pub fn configure(broker: &mut Broker) -> Self {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ssmcp-test-owner-{:032x}",
            u128::from_be_bytes(nonce)
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let root = Self(root);
        let portal = root.0.join("portal");
        DirBuilder::new().mode(0o700).create(&portal).unwrap();
        let accounts = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(portal.join("accounts.json"))
            .unwrap();
        serde_json::to_writer(
            accounts,
            &json!({"accounts":{"123":{
                "subject":"123","username":"gh-123","github_login":"fixture-owner",
                "handle":"fixture-owner","keys":[]
            }}}),
        )
        .unwrap();
        broker.portal = Portal::open(&portal).unwrap();
        let peer = Command::new("id").arg("-un").output().unwrap();
        assert!(peer.status.success());
        let user = String::from_utf8(peer.stdout)
            .unwrap()
            .trim_end()
            .to_owned();
        let policy = ManagementPolicy::new(vec![], HashMap::from([(user, "123".into())])).unwrap();
        let audit = AuditLog::open(&root.0.join("audit"), AuditConfig::default()).unwrap();
        broker.configure_management(policy, Arc::new(audit));
        root
    }
}

impl Drop for Accounts {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

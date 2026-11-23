//! One typed directory and one short-lived numbering snapshot for all CLI routes.
use super::*;
use std::collections::HashSet;

const TTL: u64 = 300;
const LIMIT: usize = 512;
const SNAPSHOT_BYTES: usize = 256 * 1024;
const REFRESH: &str = "the numbered session list is missing, expired or belongs to another account; run ssmcp list again, or use a full process/thread ID";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Process,
    Thread,
}
impl Kind {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "process" | "proc" => Some(Self::Process),
            "thread" => Some(Self::Thread),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    pub(super) kind: Kind,
    pub(super) client_id: String,
    pub(super) thread_id: Option<String>,
}
impl Target {
    pub(super) fn id(&self) -> String {
        match self.kind {
            Kind::Process => self.client_id.clone(),
            Kind::Thread => match &self.thread_id {
                Some(thread) => format!("agent:{}:{thread}", self.client_id),
                None => format!("client:{}", self.client_id),
            },
        }
    }
    fn validate(&self) -> Result<()> {
        if !full_client_id(&self.client_id)
            || (self.kind == Kind::Process && self.thread_id.is_some())
        {
            return Err("invalid directory target".into());
        }
        if self.kind == Kind::Thread {
            crate::wire::messages::parse_recipient(&self.id())
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        }
        Ok(())
    }
    pub(super) fn recipient(&self) -> Value {
        json!({"client_id":self.client_id,"thread_id":self.thread_id})
    }
}
pub(super) fn full_client_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
pub(super) fn numbered(value: &str) -> bool {
    !value.is_empty() && !full_client_id(value) && value.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) target: Target,
    pub(super) name: String,
    hostname: String,
    pub(super) sendable: bool,
}
#[derive(Clone)]
pub(super) struct Directory {
    pub(super) entries: Vec<Entry>,
}
pub(super) struct Selected {
    pub(super) entry: Entry,
    pub(super) recipients: Vec<Target>,
}
impl Directory {
    pub(super) fn parse(value: &Value) -> Result<Self> {
        let mut entries = Vec::new();
        let mut ids = HashSet::new();
        for process in clients(value)? {
            let client_id = process["id"]
                .as_str()
                .ok_or("directory process has no ID")?
                .to_owned();
            let target = Target {
                kind: Kind::Process,
                client_id: client_id.clone(),
                thread_id: None,
            };
            target.validate()?;
            if !ids.insert(target.id()) {
                return Err("directory has duplicate process IDs".into());
            }
            entries.push(Entry {
                target,
                name: process["name"].as_str().unwrap_or("Process").into(),
                hostname: process["hostname"].as_str().unwrap_or("").into(),
                sendable: false,
            });
            if let Some(threads) = process["threads"].as_array() {
                for thread in threads {
                    if thread["client_id"].as_str() != Some(&client_id) {
                        return Err("directory thread belongs to a different process".into());
                    }
                    let thread_id = match thread.get("thread_id") {
                        Some(Value::String(id)) => Some(id.clone()),
                        Some(Value::Null) => None,
                        _ => return Err("directory thread has no typed identity".into()),
                    };
                    let target = Target {
                        kind: Kind::Thread,
                        client_id: client_id.clone(),
                        thread_id,
                    };
                    target.validate()?;
                    if thread["id"].as_str() != Some(target.id().as_str())
                        || !ids.insert(target.id())
                    {
                        return Err("directory has inconsistent thread IDs".into());
                    }
                    entries.push(Entry {
                        target,
                        name: thread["name"].as_str().unwrap_or("Thread").into(),
                        hostname: String::new(),
                        sendable: thread["online"] == true && thread["sendable"] == true,
                    });
                }
            }
        }
        if entries.len() > LIMIT {
            return Err(
                "session directory exceeds 512 rows; select a smaller relay directory".into(),
            );
        }
        Ok(Self { entries })
    }
    pub(super) fn table(&self) -> String {
        if self.entries.is_empty() {
            return "No controllable sessions are online.\n".into();
        }
        let mut text = String::from("  #  NAME / HOST\n");
        // Keep display order identical to the saved snapshot and selection JSON.
        for (index, entry) in self.entries.iter().enumerate() {
            let (branch, kind) = match entry.target.kind {
                Kind::Process => ("", "process"),
                Kind::Thread => {
                    let has_next_sibling = self.entries.get(index + 1).is_some_and(|next| {
                        next.target.kind == Kind::Thread
                            && next.target.client_id == entry.target.client_id
                    });
                    (
                        if has_next_sibling {
                            "├─ "
                        } else {
                            "└─ "
                        },
                        if entry.target.thread_id.is_none() {
                            "main"
                        } else {
                            "thread"
                        },
                    )
                }
            };
            text.push_str(&format!(
                "{:>3}  {branch}{} [{kind}]",
                index + 1,
                safe(&entry.name),
            ));
            let hostname = safe(&entry.hostname);
            if entry.target.kind == Kind::Process && !hostname.is_empty() {
                text.push_str(&format!(" · {hostname}"));
            }
            text.push('\n');
        }
        text.push_str(
            "Numbers identify this exact list for 5 minutes. Use ssmcp list --json for full IDs.\n",
        );
        text.push_str("Process chat input fans out to its listed main and registered threads.\n");
        text
    }
    pub(super) fn selection_json(&self) -> Value {
        json!({"schema_version":2,"expires_in_seconds":TTL,"entries":self.entries.iter().enumerate().map(|(i,e)|json!({"number":i+1,"kind":e.target.kind,"id":e.target.id(),"client_id":e.target.client_id,"thread_id":e.target.thread_id,"name":e.name,"sendable":e.sendable})).collect::<Vec<_>>()})
    }
    fn exact(&self, selected: &str, kind: Option<Kind>) -> Result<&Entry> {
        let matches: Vec<_> = self
            .entries
            .iter()
            .filter(|e| kind.is_none_or(|k| k == e.target.kind))
            .filter(|e| {
                e.target.id() == selected
                    || (kind == Some(Kind::Thread)
                        && e.target.thread_id.is_none()
                        && e.target.kind == Kind::Thread
                        && e.target.client_id == selected)
                    || (e.target.kind == Kind::Thread
                        && e.target.thread_id.as_deref() == Some(selected))
                    || (e.target.kind == Kind::Process && e.name == selected)
            })
            .collect();
        if matches.len() != 1 {
            return Err("select an exact unique process/thread ID or a number from ssmcp list; display names never select a thread".into());
        }
        Ok(matches[0])
    }
    pub(super) fn select(
        &self,
        credential: &Credential,
        selected: &str,
        kind: Option<Kind>,
    ) -> Result<Selected> {
        if numbered(selected) {
            let snapshot = Snapshot::read(credential)?;
            return snapshot.select(
                credential,
                self,
                selected.parse().map_err(|_| REFRESH)?,
                kind,
                now(),
            );
        }
        let entry = self.exact(selected, kind)?.clone();
        self.selected(
            entry,
            &self
                .entries
                .iter()
                .map(|e| e.target.clone())
                .collect::<Vec<_>>(),
        )
    }
    fn selected(&self, entry: Entry, targets: &[Target]) -> Result<Selected> {
        let recipients = if entry.target.kind == Kind::Thread {
            vec![entry.target.clone()]
        } else {
            targets
                .iter()
                .filter(|t| t.kind == Kind::Thread && t.client_id == entry.target.client_id)
                .cloned()
                .collect()
        };
        for target in &recipients {
            if !self.entries.iter().any(|e| e.target == *target) {
                return Err("a listed thread is no longer available; run ssmcp list again without silently replacing its recipients".into());
            }
        }
        Ok(Selected { entry, recipients })
    }
    pub(super) fn remember(&self, credential: &Credential) -> Result<()> {
        let root = state_root()?;
        verify_directory(&root)?;
        let snapshot = Snapshot {
            schema_version: 2,
            server: credential.server.clone(),
            principal: credential.principal.clone(),
            subject: credential.subject.clone(),
            created_at: now(),
            targets: self.entries.iter().map(|e| e.target.clone()).collect(),
        };
        write_private_limit(
            &snapshot_path(&root, credential),
            &serde_json::to_vec(&snapshot)?,
            SNAPSHOT_BYTES,
        )
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    server: String,
    principal: String,
    subject: Option<String>,
    created_at: u64,
    targets: Vec<Target>,
}
fn snapshot_path(root: &Path, credential: &Credential) -> PathBuf {
    root.join(format!(
        "last-list-{}.json",
        crate::hex::encode(Sha256::digest(credential.server.as_bytes()))
    ))
}
impl Snapshot {
    fn read(credential: &Credential) -> Result<Self> {
        let root = state_root()?;
        verify_directory(&root)?;
        let bytes = read_private_limit(&snapshot_path(&root, credential), SNAPSHOT_BYTES)?
            .ok_or(REFRESH)?;
        serde_json::from_slice(&bytes).map_err(|_| REFRESH.into())
    }
    fn select(
        &self,
        credential: &Credential,
        directory: &Directory,
        index: usize,
        kind: Option<Kind>,
        at: u64,
    ) -> Result<Selected> {
        if self.schema_version != 2
            || self.server != credential.server
            || self.principal != credential.principal
            || self.subject != credential.subject
            || self.targets.len() > LIMIT
            || at.checked_sub(self.created_at).is_none_or(|age| age >= TTL)
        {
            return Err(REFRESH.into());
        }
        for target in &self.targets {
            target.validate()?;
        }
        let target = self
            .targets
            .get(index.checked_sub(1).ok_or(REFRESH)?)
            .ok_or(REFRESH)?;
        if kind.is_some_and(|kind| kind != target.kind) {
            return Err("that list number has a different process/thread type".into());
        }
        let entry = directory
            .entries
            .iter()
            .find(|e| e.target == *target)
            .ok_or("the target at that list number is no longer available; run ssmcp list again")?
            .clone();
        directory.selected(entry, &self.targets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credential() -> Credential {
        Credential {
            server: "https://relay.example".into(),
            token: "x".repeat(64),
            principal: "fixture@github".into(),
            subject: Some("42".into()),
            username: None,
            expires_at: 99999,
        }
    }
    fn process(id: char, thread: char) -> Value {
        let client = id.to_string().repeat(64);
        let agent = format!("agent_{}", thread.to_string().repeat(32));
        json!({"id":client,"name":"process","threads":[{"id":format!("client:{client}"),"client_id":client,"thread_id":null,"name":"main","online":true,"sendable":true},{"id":format!("agent:{client}:{agent}"),"client_id":client,"thread_id":agent,"name":"same label","online":true,"sendable":true}]})
    }
    #[test]
    fn documented_tree_renders_independent_processes_and_numbered_threads() {
        let mut workstation = process('a', 'b');
        workstation["name"] = json!("workstation");
        workstation["hostname"] = json!("devbox");
        workstation["threads"][0]["name"] = json!("Main / next unbound caller");
        workstation["threads"][1]["name"] = json!("reviewer");
        let mut pool = process('c', 'd');
        pool["name"] = json!("worker-pool");
        pool["hostname"] = json!("devbox");
        pool["threads"][0]["name"] = json!("Main / next unbound caller");
        pool["threads"][1]["name"] = json!("worker-a");
        let directory = Directory::parse(&json!({"clients":[workstation,pool]})).unwrap();
        let tree = directory.table();
        assert!(include_str!("../../../README.md").contains(&format!("~~~text\n{tree}~~~")));
        let selection = directory.selection_json();
        for (index, entry) in directory.entries.iter().enumerate() {
            assert_eq!(selection["entries"][index]["number"], json!(index + 1));
            assert_eq!(selection["entries"][index]["id"], json!(entry.target.id()));
            assert!(!tree.contains(&entry.target.id()));
        }
    }

    #[test]
    fn tree_keeps_clients_without_parent_metadata_as_independent_roots() {
        let directory = Directory::parse(&json!({"clients":[
            {"id":"a".repeat(64),"name":"control"},
            {"id":"c".repeat(64),"name":"control/worker-pool","threads":[]}
        ]}))
        .unwrap();
        assert!(directory.table().starts_with(
            "  #  NAME / HOST\n  1  control [process]\n  2  control/worker-pool [process]\n"
        ));
        assert_eq!(
            Directory::parse(&json!({"clients":[]})).unwrap().table(),
            "No controllable sessions are online.\n"
        );
    }

    #[test]
    fn tree_rejects_thread_identity_without_its_containing_process() {
        let mut missing_parent = process('a', 'b');
        missing_parent["threads"][1]
            .as_object_mut()
            .unwrap()
            .remove("client_id");
        assert!(Directory::parse(&json!({"clients":[missing_parent]})).is_err());

        let mut wrong_parent = process('a', 'b');
        wrong_parent["threads"][1]["client_id"] = json!("c".repeat(64));
        assert!(Directory::parse(&json!({"clients":[wrong_parent,process('c','d')]})).is_err());
    }

    #[test]
    fn tree_numbers_bind_process_and_thread_identity_across_reorder_and_member_addition() {
        let old =
            Directory::parse(&json!({"clients":[process('a','b'),process('c','d')]})).unwrap();
        let c = credential();
        let snapshot = Snapshot {
            schema_version: 2,
            server: c.server.clone(),
            principal: c.principal.clone(),
            subject: c.subject.clone(),
            created_at: 1000,
            targets: old.entries.iter().map(|e| e.target.clone()).collect(),
        };
        let mut updated = process('a', 'b');
        let newcomer = process('a', 'e')["threads"][1].clone();
        updated["threads"].as_array_mut().unwrap().push(newcomer);
        let fresh = Directory::parse(&json!({"clients":[process('c','d'),updated]})).unwrap();
        assert_eq!(fresh.entries.len(), 7);
        assert_eq!(
            fresh
                .table()
                .lines()
                .filter(|line| line.contains("[process]"))
                .count(),
            2
        );
        assert_eq!(
            snapshot
                .select(&c, &fresh, 3, Some(Kind::Thread), 1001)
                .unwrap()
                .entry
                .target
                .thread_id,
            Some(format!("agent_{}", "b".repeat(32)))
        );
        assert_eq!(
            snapshot
                .select(&c, &fresh, 1, Some(Kind::Process), 1001)
                .unwrap()
                .recipients
                .len(),
            2
        );
        assert!(
            snapshot
                .select(&c, &fresh, 3, Some(Kind::Process), 1001)
                .is_err()
        );
        assert!(snapshot.select(&c, &fresh, 1, None, 1300).is_err());
        assert!(snapshot.select(&c, &fresh, 1, None, 999).is_err());
        let mut foreign = c.clone();
        foreign.subject = Some("43".into());
        assert!(snapshot.select(&foreign, &fresh, 1, None, 1001).is_err());
        let replacement = Directory::parse(&json!({"clients":[process('a','e')]})).unwrap();
        assert!(snapshot.select(&c, &replacement, 1, None, 1001).is_err());
        assert!(fresh.exact("same label", Some(Kind::Thread)).is_err());
        assert!(fresh.exact("process", Some(Kind::Process)).is_err());
        let without_first = Directory::parse(&json!({"clients":[process('c','d')]})).unwrap();
        assert!(snapshot.select(&c, &without_first, 3, None, 1001).is_err());
    }
}

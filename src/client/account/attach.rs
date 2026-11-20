//! Resolve attach targets without confusing account devices with saved sessions.
use super::*;
use std::collections::HashSet;

const LIST_TTL_SECONDS: u64 = 300;
const MAX_LIST_IDS: usize = 256;
const REFRESH_LIST: &str = "the numbered session list is missing, expired or belongs to another account; run ssmcp list again, or use a full client ID";
const HELP: &str = "Usage: ssmcp attach [list] [--json]\n       ssmcp attach TARGET [--local | --remote] [--read-only]\n       ssmcp attach --remote [--server URL] [--client ID] [--terminal ID]\n\nTARGET accepts a saved local name, an online device ID/name, or a number from your last ssmcp list (valid for 5 minutes).\nExisting saved names are local first, without a login or network request; --remote selects an online device with the same name.\nNumbers and complete client IDs select remote devices; --local selects a local session with a numeric name.\nBare attach and attach list show local saved sessions; attach --remote opens the online chooser.\nRemote attach opens the selected device's existing shell terminal. --read-only sends neither input nor resize.\nCtrl-] detaches without stopping the session; Ctrl-C reaches an interactive remote shell.\nUse -- before a literal name such as list. --local never falls back to a remote device.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scope {
    #[default]
    Auto,
    Local,
    Remote,
}

#[derive(Debug, Default)]
struct Options {
    scope: Scope,
    target: Option<String>,
    server: Option<String>,
    terminal: Option<String>,
    read_only: bool,
    json: bool,
    listing: bool,
    help: bool,
}

impl Options {
    fn parse(args: &[OsString]) -> Result<Self> {
        let mut options = Self::default();
        let mut literal = false;
        let mut remote_option = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let arg = arg.to_str().ok_or("attach arguments must be UTF-8")?;
            let mut value = || -> Result<String> {
                Ok(args
                    .next()
                    .ok_or("attach option requires a value")?
                    .to_str()
                    .ok_or("attach arguments must be UTF-8")?
                    .to_owned())
            };
            match arg {
                "--" if !literal => literal = true,
                "--help" | "-h" if !literal => options.help = true,
                "--local" | "--remote" if !literal => {
                    if options.scope != Scope::Auto {
                        return Err("choose --local or --remote only once".into());
                    }
                    options.scope = if arg == "--local" {
                        Scope::Local
                    } else {
                        Scope::Remote
                    };
                }
                "--read-only" if !literal => options.read_only = true,
                "--json" if !literal => options.json = true,
                "--server" if !literal => {
                    if options.server.is_some() {
                        return Err("--server may be supplied only once".into());
                    }
                    options.server = Some(normalize_server(&value()?)?);
                    remote_option = true;
                }
                "--terminal" if !literal => {
                    if options.terminal.is_some() {
                        return Err("--terminal may be supplied only once".into());
                    }
                    options.terminal = Some(value()?);
                    remote_option = true;
                }
                "--client" if !literal => {
                    if options.target.is_some() {
                        return Err("attach accepts exactly one target".into());
                    }
                    options.target = Some(value()?);
                    remote_option = true;
                }
                _ if !literal && arg.starts_with('-') => {
                    return Err("unknown attach option; see ssmcp attach --help".into());
                }
                _ => {
                    if options.target.is_some() {
                        return Err("attach accepts exactly one target".into());
                    }
                    options.listing = arg == "list" && !literal;
                    options.target = Some(arg.into());
                }
            }
        }
        if remote_option {
            if options.scope == Scope::Local {
                return Err("--local cannot use --server, --client or --terminal".into());
            }
            options.scope = Scope::Remote;
        }
        if options.json && options.target.is_some() && !options.listing {
            return Err("--json is only available when listing attach targets".into());
        }
        if options
            .target
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
        {
            return Err("attach target must be 1..=512 bytes without control characters".into());
        }
        Ok(options)
    }

    fn local_args(&self) -> Vec<OsString> {
        if self.listing || self.target.is_none() {
            let mut args = vec![OsString::from("list")];
            if self.json {
                args.push("--json".into());
            }
            return args;
        }
        let mut args = Vec::new();
        if self.read_only {
            args.push("--read-only".into());
        }
        args.push("--".into());
        args.push(self.target.as_deref().expect("local target").into());
        args
    }
}

fn complete_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn numbered(value: &str) -> bool {
    !value.is_empty() && !complete_id(value) && value.bytes().all(|b| b.is_ascii_digit())
}

pub(super) async fn run(args: &[OsString]) -> Result<()> {
    let options = Options::parse(args)?;
    if options.help {
        println!("{HELP}");
        return Ok(());
    }
    if options.scope == Scope::Local
        || (options.scope == Scope::Auto && (options.listing || options.target.is_none()))
    {
        return crate::session::attach::manage(options.local_args()).await;
    }
    if options.scope == Scope::Auto {
        let target = options.target.as_deref().expect("automatic attach target");
        // Preserve offline local attach. Only absence permits remote lookup;
        // permissions, damaged manifests and stopped local sessions stay local.
        if !numbered(target)
            && !complete_id(target)
            && crate::session::attach::saved_session_exists(target)?
        {
            return crate::session::attach::manage(options.local_args()).await;
        }
    }
    if options.listing || options.json {
        return list(&Arguments {
            server: options.server,
            json: options.json,
            ..Arguments::default()
        })
        .await;
    }
    let credential = require_credential(options.server.as_deref()).await?;
    let api = Api::new(&credential.server)?;
    let directory = api.get("/api/client/sessions", &credential.token).await?;
    let entries = clients(&directory)?;
    let id = if let Some(target) = options.target.as_deref() {
        if numbered(target) {
            let index = target.parse::<usize>().map_err(|_| REFRESH_LIST)?;
            let snapshot = read_snapshot(&state_root()?, &credential)?;
            snapshot
                .select(&credential, entries, index, now())?
                .to_owned()
        } else {
            select_client_exact(entries, target)?.to_owned()
        }
    } else {
        require_terminal()?;
        eprintln!("{}", interactive_selection_help());
        remember_list(&credential, entries)?;
        eprint!("{}", list_table(entries));
        if entries.is_empty() {
            return Ok(());
        }
        select_client(entries, &prompt_selection().await?)?.to_owned()
    };
    require_terminal()?;
    interactive(
        &api,
        &credential,
        &id,
        options.terminal.as_deref().unwrap_or("main"),
        options.read_only,
    )
    .await
}

fn require_terminal() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("remote attach requires a terminal; use ssmcp list --json for scripts".into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    server: String,
    principal: String,
    subject: Option<String>,
    created_at: u64,
    client_ids: Vec<String>,
}

impl Snapshot {
    fn new(credential: &Credential, entries: &[Value], created_at: u64) -> Result<Self> {
        if entries.len() > MAX_LIST_IDS {
            return Err(
                "too many devices for a numbered list snapshot; select a full client ID".into(),
            );
        }
        let mut ids = HashSet::new();
        let client_ids = entries
            .iter()
            .map(|entry| {
                let id = entry["id"]
                    .as_str()
                    .filter(|id| {
                        !id.is_empty()
                            && id.len() <= 128
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    })
                    .ok_or("server returned an invalid session ID")?;
                if !ids.insert(id) {
                    return Err("server returned duplicate session IDs".into());
                }
                Ok(id.to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            schema_version: 1,
            server: credential.server.clone(),
            principal: credential.principal.clone(),
            subject: credential.subject.clone(),
            created_at,
            client_ids,
        })
    }

    fn select<'a>(
        &self,
        credential: &Credential,
        entries: &'a [Value],
        index: usize,
        at: u64,
    ) -> Result<&'a str> {
        if self.schema_version != 1
            || self.server != credential.server
            || self.principal != credential.principal
            || self.subject != credential.subject
            || self.client_ids.len() > MAX_LIST_IDS
            || at
                .checked_sub(self.created_at)
                .is_none_or(|age| age >= LIST_TTL_SECONDS)
        {
            return Err(REFRESH_LIST.into());
        }
        let id = index
            .checked_sub(1)
            .and_then(|i| self.client_ids.get(i))
            .ok_or(REFRESH_LIST)?;
        let mut matching = entries
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .filter(|live| *live == id.as_str());
        match (matching.next(), matching.next()) {
            (Some(id), None) => Ok(id),
            _ => Err("the device at that list number is no longer available; run ssmcp list again or use a full client ID".into()),
        }
    }
}

fn snapshot_path(root: &Path, credential: &Credential) -> PathBuf {
    root.join(format!(
        "last-list-{}.json",
        crate::hex::encode(Sha256::digest(credential.server.as_bytes()))
    ))
}

fn read_snapshot(root: &Path, credential: &Credential) -> Result<Snapshot> {
    verify_directory(root)?;
    let bytes = read_private(&snapshot_path(root, credential))?.ok_or(REFRESH_LIST)?;
    serde_json::from_slice(&bytes).map_err(|_| REFRESH_LIST.into())
}

pub(super) fn remember_list(credential: &Credential, entries: &[Value]) -> Result<()> {
    let root = state_root()?;
    verify_directory(&root)?;
    let snapshot = Snapshot::new(credential, entries, now())?;
    // Commit before printing: a failed write must not leave a newly displayed
    // numbered table paired with an older on-disk snapshot.
    write_private(
        &snapshot_path(&root, credential),
        &serde_json::to_vec(&snapshot)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> Result<Options> {
        Options::parse(
            &args
                .iter()
                .map(|arg| OsString::from(*arg))
                .collect::<Vec<_>>(),
        )
    }

    fn credential() -> Credential {
        Credential {
            server: "https://relay.example".into(),
            token: "a".repeat(64),
            principal: "fixture@github".into(),
            subject: Some("42".into()),
            username: Some("fixture".into()),
            expires_at: 10_000,
        }
    }

    #[test]
    fn attach_options_preserve_local_names_and_explicit_remote_selection() {
        assert!(options(&[]).unwrap().target.is_none());
        assert!(options(&["list", "--json"]).unwrap().listing);
        let local = options(&["--local", "--read-only", "--", "list"]).unwrap();
        assert!(!local.listing);
        assert_eq!(
            local.local_args(),
            ["--read-only", "--", "list"].map(OsString::from)
        );
        assert_eq!(
            options(&["--client", "device"]).unwrap().scope,
            Scope::Remote
        );
        assert_eq!(
            options(&["device", "--server", "https://relay.example"])
                .unwrap()
                .scope,
            Scope::Remote
        );
        assert!(options(&["--local", "--remote", "device"]).is_err());
        assert!(options(&["--local", "--server", "https://relay.example", "device"]).is_err());
        assert!(options(&["device", "other"]).is_err());
        assert!(options(&["device", "--json"]).is_err());
        assert!(options(&["--client", "one", "--client", "two"]).is_err());
        assert!(numbered("2"));
        assert!(!numbered(&"1".repeat(64)));
    }

    #[test]
    fn attach_number_keeps_the_listed_identity_after_reordering_and_rejects_replacement() {
        let credential = credential();
        let listed = vec![
            json!({"id":"first","name":"one"}),
            json!({"id":"original","name":"two"}),
        ];
        let snapshot = Snapshot::new(&credential, &listed, 1000).unwrap();
        let reordered = vec![
            json!({"id":"original","name":"two"}),
            json!({"id":"new","name":"three"}),
        ];
        assert_eq!(
            snapshot.select(&credential, &reordered, 2, 1001).unwrap(),
            "original"
        );
        let replacement = vec![json!({"id":"new","name":"two"})];
        assert!(snapshot.select(&credential, &replacement, 2, 1001).is_err());
        assert!(snapshot.select(&credential, &listed, 0, 1001).is_err());
        assert!(snapshot.select(&credential, &listed, 3, 1001).is_err());
    }

    #[test]
    fn attach_number_rejects_expired_future_and_other_account_snapshots() {
        let credential = credential();
        let listed = vec![json!({"id":"original"})];
        let snapshot = Snapshot::new(&credential, &listed, 1000).unwrap();
        assert!(snapshot.select(&credential, &listed, 1, 1300).is_err());
        assert!(snapshot.select(&credential, &listed, 1, 999).is_err());
        for field in ["server", "subject", "principal"] {
            let mut other = credential.clone();
            match field {
                "server" => other.server = "https://other.example".into(),
                "subject" => other.subject = Some("43".into()),
                _ => other.principal = "other@github".into(),
            }
            assert!(snapshot.select(&other, &listed, 1, 1001).is_err());
        }
        assert!(
            Snapshot::new(
                &credential,
                &[json!({"id":"same"}), json!({"id":"same"})],
                1000
            )
            .is_err()
        );
        assert!(
            select_client_exact(
                &[
                    json!({"id":"a","name":"same"}),
                    json!({"id":"b","name":"same"})
                ],
                "same"
            )
            .is_err()
        );
    }
}

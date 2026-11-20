//! Resolve local sessions and typed remote chat targets without silent fallback.
use super::*;
use directory::{Directory, Kind};

const HELP: &str = "Usage: ssmcp attach [list] [--json]\n       ssmcp attach [process|thread] ID|NUMBER [--remote] [--read-only]\n       ssmcp attach NAME --local [--read-only]\n       ssmcp attach process ID|NUMBER --terminal TERMINAL_ID [--read-only]\n\nDefault remote attachment is chat stdin/stdout. Enter submits a request; only actual replies finish it.\nA process input fans out to the fixed main and registered thread recipients from its selection.\nA thread input reaches only that exact registered thread, or client:ID for the unbound main.\nNumbers bind the last displayed list for 5 minutes; full IDs and exact unique process names also work.\nExisting saved local names are local first and work offline; --remote overrides that choice.\nBare attach / attach list show saved local sessions. attach --remote opens the typed online chooser.\nThe existing remote shell remains available through ssmcp -i or explicit --terminal ID.\n--read-only never sends chat input, terminal input, or resize. Ctrl+] detaches without killing a target.\nUse -- before a literal local name such as list. --local preserves local errors and never falls back online.";

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
    kind: Option<Kind>,
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
        let mut result = Self::default();
        let mut literal = false;
        let mut remote = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let arg = arg.to_str().ok_or("attach arguments must be UTF-8")?;
            let mut value = || -> Result<String> {
                Ok(args
                    .next()
                    .ok_or("attach option requires a value")?
                    .to_str()
                    .ok_or("attach arguments must be UTF-8")?
                    .into())
            };
            match arg {
                "--" if !literal => literal = true,
                "--help" | "-h" if !literal => result.help = true,
                "--local" | "--remote" if !literal => {
                    if result.scope != Scope::Auto {
                        return Err("choose --local or --remote only once".into());
                    }
                    result.scope = if arg == "--local" {
                        Scope::Local
                    } else {
                        Scope::Remote
                    };
                }
                "--read-only" if !literal => result.read_only = true,
                "--json" if !literal => result.json = true,
                "--server" if !literal => {
                    if result.server.is_some() {
                        return Err("--server may be supplied only once".into());
                    }
                    result.server = Some(normalize_server(&value()?)?);
                    remote = true;
                }
                "--terminal" if !literal => {
                    if result.terminal.is_some() {
                        return Err("--terminal may be supplied only once".into());
                    }
                    result.terminal = Some(value()?);
                    remote = true;
                }
                "--client" if !literal => {
                    if result.target.is_some() {
                        return Err("attach accepts one target".into());
                    }
                    result.target = Some(value()?);
                    remote = true;
                }
                _ if !literal
                    && result.scope != Scope::Local
                    && result.target.is_none()
                    && result.kind.is_none()
                    && Kind::parse(arg).is_some() =>
                {
                    result.kind = Kind::parse(arg);
                    remote = true;
                }
                _ if !literal && arg.starts_with('-') => {
                    return Err("unknown attach option; see ssmcp attach --help".into());
                }
                _ => {
                    if result.target.is_some() {
                        return Err("attach accepts one target".into());
                    }
                    result.listing = arg == "list" && !literal;
                    result.target = Some(arg.into());
                }
            }
        }
        if remote {
            if result.scope == Scope::Local {
                return Err(
                    "--local cannot use process/thread, --server, --client or --terminal".into(),
                );
            }
            result.scope = Scope::Remote;
        }
        if result.json && result.target.is_some() && !result.listing {
            return Err("--json is only available when listing attach targets".into());
        }
        if result
            .target
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
        {
            return Err("attach target must be 1..=512 bytes without controls".into());
        }
        Ok(result)
    }
    fn local_args(&self) -> Vec<OsString> {
        if self.listing || self.target.is_none() {
            let mut args = vec!["list".into()];
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
        let target = options.target.as_deref().expect("automatic target");
        if !directory::numbered(target)
            && !directory::full_client_id(target)
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
    let live = api.get("/api/client/sessions", &credential.token).await?;
    let directory = Directory::parse(&live)?;
    let target = if let Some(target) = options.target {
        target
    } else {
        require_terminal()?;
        directory.remember(&credential)?;
        eprint!("{}", directory.table());
        if directory.entries.is_empty() {
            return Ok(());
        }
        prompt_selection().await?
    };
    let selected = directory.select(&credential, &target, options.kind)?;
    require_terminal()?;
    if let Some(terminal) = options.terminal {
        if selected.entry.target.kind != Kind::Process {
            return Err(
                "--terminal requires a process target; a chat thread is not a shell PTY".into(),
            );
        }
        interactive(
            &api,
            &credential,
            &selected.entry.target.client_id,
            &terminal,
            options.read_only,
        )
        .await
    } else {
        chat::run(&api, &credential, selected, options.read_only).await
    }
}
fn require_terminal() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(
            "remote attach requires a terminal; use send/wait or list --json for scripts".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(args: &[&str]) -> Result<Options> {
        Options::parse(&args.iter().map(OsString::from).collect::<Vec<_>>())
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
            options(&["thread", "agent_fixture"]).unwrap().kind,
            Some(Kind::Thread)
        );
        assert_eq!(
            options(&["process", "2", "--terminal", "main"])
                .unwrap()
                .terminal
                .as_deref(),
            Some("main")
        );
        assert_eq!(
            options(&["--client", "device"]).unwrap().scope,
            Scope::Remote
        );
        assert!(options(&["--local", "--remote", "device"]).is_err());
        assert!(options(&["--local", "--server", "https://relay.example", "device"]).is_err());
        assert!(options(&["device", "other"]).is_err());
        assert!(options(&["device", "--json"]).is_err());
    }
}

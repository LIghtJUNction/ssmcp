//! Explicit registration and offline initialization of local workflow helpers.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Scope {
    Project,
    Global,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }

    fn path(self, paths: &Paths) -> PathBuf {
        match self {
            Self::Project => paths.project(),
            Self::Global => paths.global.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    Init { scope: Scope, git: bool },
    Enable(Scope),
    Disable(Scope),
}

pub(super) fn parse(args: &[OsString]) -> Result<Option<Operation>, String> {
    let Some(command) = args.first().and_then(|arg| arg.to_str()) else {
        return Ok(None);
    };
    if !matches!(command, "init" | "enable" | "disable") {
        return Ok(None);
    }
    let mut scope = None;
    let mut git = false;
    for arg in &args[1..] {
        match arg.to_str() {
            Some("project") if scope.is_none() => scope = Some(Scope::Project),
            Some("global") if scope.is_none() => scope = Some(Scope::Global),
            Some("--git") if command == "init" && !git => git = true,
            _ => return Err(HELP.into()),
        }
    }
    let scope = scope.unwrap_or(Scope::Project);
    Ok(Some(match command {
        "init" => Operation::Init { scope, git },
        "enable" => Operation::Enable(scope),
        "disable" => Operation::Disable(scope),
        _ => unreachable!(),
    }))
}

pub(super) fn locations(paths: &Paths) -> Value {
    json!({
        "rc": paths.rc, "official": paths.directory.join("official"),
        "global": paths.global, "project": paths.project(),
        "load_order": ["official", "RC overrides", "global", "project"],
        "selection": "only sources registered in the trusted RC"
    })
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => canonical_directory(path).map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or("local tool directory has no parent")?;
            ensure_real_directory(parent)?;
            crate::platform::private_directory(path).map_err(|e| e.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn same_identity(left: &Option<RcIdentity>, right: &Option<RcIdentity>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.path == right.path && left.sha256 == right.sha256,
        _ => false,
    }
}

pub(super) fn save_config(
    paths: &Paths,
    original: &Option<RcIdentity>,
    config: &Rc,
) -> Result<RcIdentity, String> {
    let body = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    if body.len() > MAX_RC {
        return Err("updated tool RC exceeds 64 KiB".into());
    }
    let parent = paths.rc.parent().ok_or("tool RC has no parent")?;
    ensure_real_directory(parent)?;
    let temporary = Temporary::new(parent)?;
    let candidate = temporary.0.join("ssmcprc.toml");
    write_new(&candidate, body.as_bytes())?;
    let (current, _) = paths.read_config()?;
    if !same_identity(original, &current) {
        return Err(
            "tool RC changed during local initialization; existing config was preserved".into(),
        );
    }
    git::active()?;
    std::fs::rename(&candidate, &paths.rc).map_err(|e| e.to_string())?;
    Ok(RcIdentity {
        path: paths.rc.display().to_string(),
        sha256: digest(body.as_bytes()),
    })
}

const TEMPLATE: &[u8] = b"// Define workflow helpers with authenticated core operations; loading must have no effects.\ncore => ({})\n";
const MANIFEST: &[u8] = b"{\"schema\":\"ssmcp-tools-v1\",\"entry\":\"tools.js\"}\n";
const README: &[u8] = b"# Local SSMCP workflow helpers\n\nEdit tools.js and validate in an isolated fixture before using a new code-mode call.\nThe outer tool_sources receipt identifies the source hash and Git state actually loaded.\nFactories define helpers without starting operations; protected host primitives remain authoritative.\nCommit reviewed source and workflow inputs in this repository when using Git. Keep credentials,\nlarge outputs and generated binaries out of it. A previous unknown execution must be inspected,\nnot replayed by a workflow helper. No model or background scheduler is created by initialization.\n";

async fn initialize(paths: &Paths, scope: Scope, with_git: bool) -> Result<PathBuf, String> {
    let target = scope.path(paths);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            return Err(format!(
                "{} already exists; inspect it and use ssmcp tools enable {}",
                target.display(),
                scope.name()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let parent = target
        .parent()
        .ok_or("local tool repository has no parent")?;
    ensure_real_directory(parent)?;
    let temporary = Temporary::new(parent)?;
    write_new(&temporary.0.join("manifest.json"), MANIFEST)?;
    write_new(&temporary.0.join("tools.js"), TEMPLATE)?;
    write_new(&temporary.0.join("README.md"), README)?;
    if with_git {
        git::run(
            &temporary.0,
            &["init".into(), "--initial-branch=main".into(), ".".into()],
            false,
        )
        .await?;
    }
    // Validate the staged entry before it can become selected by any new call.
    source(scope.name().into(), &temporary.0).await?;
    git::active()?;
    // Reserve the destination atomically; rename could replace a concurrent empty directory.
    crate::platform::private_directory(&target).map_err(|e| {
        format!(
            "could not reserve {} without replacing existing work: {e}",
            target.display()
        )
    })?;
    let mut files = vec!["tools.js", "README.md"];
    if with_git {
        files.push(".git");
    }
    // Publish the manifest last; registration is written only after all files exist.
    files.push("manifest.json");
    for name in files {
        std::fs::rename(temporary.0.join(name), target.join(name)).map_err(|e|
            format!("local initialization did not finish at {}; inspect the preserved directory before retrying: {e}", target.display()))?;
    }
    target.canonicalize().map_err(|e| e.to_string())
}

pub(super) async fn manage(paths: Paths, operation: Operation) -> Result<Value, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        git::owned(async move { manage_owned(&paths, operation).await }),
    )
    .await
    .map_err(|_| {
        "local tool operation timed out; inspect its RC and destination before retrying".to_owned()
    })?
}

pub(super) async fn manage_owned(paths: &Paths, operation: Operation) -> Result<Value, String> {
    let _lock = Lock::take(&paths.directory)?;
    let (original, mut config) = paths.read_config()?;
    let before = toml::to_string(&config).map_err(|e| e.to_string())?;
    let (scope, enabled, created, with_git, action) = match operation {
        Operation::Init { scope, git } => {
            initialize(paths, scope, git).await?;
            (scope, true, true, git, "init")
        }
        Operation::Enable(scope) => (scope, true, false, false, "enable"),
        Operation::Disable(scope) => (scope, false, false, false, "disable"),
    };
    let target = scope.path(paths);
    let mut migrated_overrides = 0;
    let selected = if enabled {
        let canonical = canonical_directory(&target)?;
        let selected = source(scope.name().into(), &canonical).await?;
        Bundle {
            schema: SCHEMA.into(),
            rc: None,
            sources: vec![selected.clone()],
        }
        .validate()?;
        config.tools.overrides.retain(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                paths.workspace.join(path)
            };
            let duplicate = absolute.canonicalize().is_ok_and(|resolved| {
                resolved == canonical || resolved == Path::new(&selected.path)
            });
            if duplicate {
                migrated_overrides += 1;
            }
            !duplicate
        });
        Some(selected)
    } else {
        None
    };
    match scope {
        Scope::Global => config.tools.global = enabled,
        Scope::Project => {
            if enabled {
                if !config.tools.projects.contains(&paths.workspace) {
                    config.tools.projects.push(paths.workspace.clone());
                }
            } else {
                config
                    .tools
                    .projects
                    .retain(|root| root != &paths.workspace);
            }
        }
    }
    if config.tools.projects.len() > 128 {
        return Err(format!(
            "at most 128 trusted project roots are allowed; {} was preserved",
            target.display()
        ));
    }
    if enabled {
        paths.selection(&config).map_err(|error| {
            if created {
                format!(
                    "{error}; created {} is preserved, but activation did not complete",
                    target.display()
                )
            } else {
                error
            }
        })?;
    }
    let changed = before != toml::to_string(&config).map_err(|e| e.to_string())?;
    let identity = if changed {
        Some(save_config(paths, &original, &config).map_err(|error| {
            if created {
                format!(
                    "{error}; created {} is preserved, but activation did not complete",
                    target.display()
                )
            } else {
                error
            }
        })?)
    } else {
        original
    };
    Ok(json!({
        "schema":"ssmcp-tools-local-v1", "action":action, "scope":scope.name(),
        "path":target, "enabled":enabled, "created":created, "initialized_git":with_git,
        "rc_changed":changed, "migrated_overrides":migrated_overrides, "rc":identity,
        "source":selected.map(|s| json!({
            "name":s.name, "path":s.path, "sha256":s.sha256, "bytes":s.code.len(), "repository":s.repository
        })),
        "locations":locations(paths),
        "message":"Local source registration only. Verify tool_sources on a new code-mode call; no model or workflow was started."
    }))
}

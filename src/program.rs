//! Resolve installed entry points without executing package-manager shims.
use std::{
    collections::HashSet,
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const NODE_FLAGS: &[&str] = &[
    "--jitless",
    "--no-addons",
    "--max-old-space-size=32",
    "--max-semi-space-size=1",
    "--stack-size=256",
];

pub fn executable(name: &str) -> Result<PathBuf, String> {
    let search = std::env::var_os("PATH").unwrap_or_default();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    resolve(name, &search, home.as_deref())
}

fn resolve(name: &str, search: &OsStr, home: Option<&Path>) -> Result<PathBuf, String> {
    let mut diagnostic = None;
    for directory in std::env::split_paths(search) {
        let candidates = vec![directory.join(name)];
        #[cfg(windows)]
        let candidates = {
            let mut candidates = candidates;
            if Path::new(name).extension().is_none() {
                for ext in [".exe", ".com", ".cmd", ".bat"] {
                    candidates.push(directory.join(format!("{name}{ext}")));
                }
            }
            candidates
        };
        for candidate in candidates {
            if !crate::platform::is_executable(&candidate) {
                continue;
            }
            match entry(&candidate, name, home, &mut HashSet::new(), 0) {
                Ok(path) => return Ok(path),
                Err(error) => diagnostic = Some(error),
            }
        }
    }
    Err(diagnostic.unwrap_or_else(|| format!("{name} is not installed in PATH")))
}

fn prefix(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(8192)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn entry(
    path: &Path,
    name: &str,
    home: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<PathBuf, String> {
    if depth > 8 {
        return Err(format!("{name}: installed entry point cycle"));
    }
    let resolved = path.canonicalize().map_err(|e| e.to_string())?;
    if !visited.insert(resolved.clone()) {
        return Err(format!("{name}: installed entry point cycle"));
    }
    let bytes = prefix(&resolved)?;
    let text = String::from_utf8_lossy(&bytes);
    let base = resolved.file_stem().and_then(OsStr::to_str).unwrap_or("");
    let path_text = path.to_string_lossy().replace(char::from(92), "/");
    let manager = if base == "mise"
        || path_text.contains("/mise/shims/")
        || path_text.contains("/mise/command-wrappers/")
    {
        Some("mise")
    } else if base == "asdf" || path_text.contains("/.asdf/shims/") {
        Some("asdf")
    } else if base == "volta" || path_text.contains("/.volta/bin/") {
        Some("volta")
    } else {
        None
    };
    if let Some(manager) = manager {
        let home = home.ok_or_else(|| format!("{manager} shim for {name}; home is unavailable"))?;
        return installed(manager, name, home, depth + 1)
            .ok_or_else(|| format!("{manager} shim for {name} has no usable installed entry point; no download was attempted"));
    }
    // Resolve only an exact dispatcher: extra options or shell statements may
    // carry user policy, so never silently bypass a more complex wrapper.
    if let Some(manager) = static_manager_wrapper(&text, name) {
        let home =
            home.ok_or_else(|| format!("{manager} wrapper for {name}; home is unavailable"))?;
        return installed(manager, name, home, depth + 1).ok_or_else(|| {
            format!("{manager} wrapper for {name} has no usable installed entry point; no download was attempted")
        });
    }
    // mise/aube npm shims publish their already-installed entry in a comment.
    if let Some(target) = text
        .lines()
        .take(4)
        .find(|line| line.contains("aube-bin-shim"))
        .and_then(|line| line.split("target=").nth(1))
        .and_then(|tail| tail.split_whitespace().next())
    {
        if !target.is_empty()
            && !target
                .chars()
                .any(|c| matches!(c as u32, 36 | 96 | 34 | 39))
        {
            let target = path.parent().ok_or("entry has no parent")?.join(target);
            if target.is_file() {
                return entry(&target, name, home, visited, depth + 1);
            }
        }
        return Err(format!(
            "{name}: package shim target is unavailable; no installation was attempted"
        ));
    }
    if text.starts_with("#!")
        && text.lines().any(|line| {
            let line = line.trim();
            !line.starts_with('#')
                && (line.contains("mise exec")
                    || line.contains("mise x ")
                    || line.contains("asdf exec")
                    || line.contains("volta run")
                    || line.contains("npx ")
                    || line.contains("npm exec")
                    || line.contains("bunx "))
        })
    {
        return Err(format!(
            "{name}: unresolved package-manager wrapper; refusing to run a downloader during discovery"
        ));
    }
    Ok(resolved)
}

fn static_manager_wrapper(text: &str, name: &str) -> Option<&'static str> {
    if !text.starts_with("#!") {
        return None;
    }
    let mut commands = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let command = commands.next()?;
    if commands.next().is_some() {
        return None;
    }
    for verb in ["x", "exec"] {
        for quoted in [name.to_owned(), format!("\"{name}\""), format!("'{name}'")] {
            if command == format!("exec mise {verb} {quoted} -- {quoted} \"$@\"") {
                return Some("mise");
            }
        }
    }
    None
}

fn installed(manager: &str, name: &str, home: &Path, depth: usize) -> Option<PathBuf> {
    let root = match manager {
        "mise" => home.join(".local/share/mise/installs").join(name),
        "asdf" => home
            .join(".asdf/installs")
            .join(if name == "node" { "nodejs" } else { name }),
        "volta" if name == "node" => home.join(".volta/tools/image/node"),
        "volta" => home.join(".volta/tools/image/packages").join(name),
        _ => return None,
    };
    let mut versions = vec![
        root.join("global"),
        root.join("latest"),
        root.join("current"),
    ];
    let mut installed: Vec<_> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
        })
        .take(128)
        .collect();
    installed.sort_by_cached_key(|path| {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .split(['.', '-', '_'])
            .map(|part| format!("{part:0>16}"))
            .collect::<String>()
    });
    versions.extend(installed.into_iter().rev());
    for version in versions {
        for candidate in [
            version.join("bin").join(name),
            version.join(name),
            version.join(name).join(name),
            version.join("node_modules/.bin").join(name),
        ] {
            #[cfg(windows)]
            let candidates = [
                candidate.clone(),
                candidate.with_extension("exe"),
                candidate.with_extension("cmd"),
            ];
            #[cfg(not(windows))]
            let candidates = [candidate];
            for candidate in candidates {
                if crate::platform::is_executable(&candidate)
                    && let Ok(path) =
                        entry(&candidate, name, Some(home), &mut HashSet::new(), depth)
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn native_binary(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
        || bytes.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
}

/// The isolated executor requires a real Node runtime with compatible flags.
pub fn node() -> Result<PathBuf, String> {
    let path = executable("node")?;
    if !native_binary(&prefix(&path)?) {
        return Err(
            "Node entry is not a native executable; refusing to pass --jitless to a shim".into(),
        );
    }
    let mut child = Command::new(&path)
        .args(NODE_FLAGS)
        .args(["--eval", "if (!process.versions.node) process.exit(1)"])
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not validate installed Node: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(path),
            Ok(Some(status)) => {
                return Err(format!(
                    "installed Node does not support the isolated executor flags ({status})"
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match outcome {
                    Err(error) => format!("Node compatibility check failed: {error}"),
                    _ => "Node compatibility check timed out".into(),
                });
            }
        }
    }
}

/// Prevent JS entry points from selecting a package-manager shim as Node.
pub fn configure(command: &mut tokio::process::Command) {
    // Native discovery may start configured MCP helpers; never install their dependencies.
    command
        .env("MISE_AUTO_INSTALL", "false")
        .env("UV_OFFLINE", "1")
        .env("npm_config_offline", "true")
        .env("COREPACK_ENABLE_NETWORK", "0")
        .env("PI_OFFLINE", "1")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1");
    if let Ok(node) = executable("node")
        && prefix(&node).is_ok_and(|bytes| native_binary(&bytes))
        && let Some(directory) = node.parent()
    {
        let mut paths = vec![directory.to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn directory() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-program-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn resolves_mise_without_ever_running_it_or_downloading() {
        let root = directory();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let mise = root.join("mise");
        std::fs::write(
            &mise,
            format!("#!/bin/sh\ntouch {}/downloaded\nexit 77\n", root.display()),
        )
        .unwrap();
        std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&mise, bin.join("node")).unwrap();
        assert!(
            resolve("node", bin.as_os_str(), Some(&root))
                .unwrap_err()
                .contains("no download")
        );
        let runtime = root.join(".local/share/mise/installs/node/26.13.1/bin/node");
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), &runtime).unwrap();
        assert_eq!(
            resolve("node", bin.as_os_str(), Some(&root)).unwrap(),
            runtime.canonicalize().unwrap()
        );
        assert!(!root.join("downloaded").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn skips_unresolved_shim_and_finds_real_runtime_later_in_path() {
        let root = directory();
        let shims = root.join(".asdf/shims");
        let bin = root.join("bin");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::create_dir(&bin).unwrap();
        for path in [shims.join("node"), bin.join("node")] {
            std::fs::copy(std::env::current_exe().unwrap(), path).unwrap();
        }
        let path = std::env::join_paths([shims, bin.clone()]).unwrap();
        assert_eq!(
            resolve("node", &path, Some(&root)).unwrap(),
            bin.join("node").canonicalize().unwrap()
        );
        assert!(!native_binary(b"#!/bin/sh\nexec mise"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resolves_exact_mise_dispatcher_to_installed_nested_binary() {
        let root = directory();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let wrapper = bin.join("pi");
        std::fs::write(&wrapper, "#!/bin/bash\n# Installed native bundle dispatcher.\nexec mise x \"pi\" -- \"pi\" \"$@\"\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            resolve("pi", bin.as_os_str(), Some(&root))
                .unwrap_err()
                .contains("no download")
        );
        let binary = root.join(".local/share/mise/installs/pi/1.5.0/pi/pi");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        assert_eq!(
            resolve("pi", bin.as_os_str(), Some(&root)).unwrap(),
            binary.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn does_not_skip_extra_wrapper_arguments_or_policy_statements() {
        assert_eq!(
            static_manager_wrapper("#!/bin/sh\nexec mise x \"pi\" -- \"pi\" \"$@\"", "pi"),
            Some("mise")
        );
        for script in [
            "#!/bin/sh\nexec mise x pi -- pi --config other \"$@\"",
            "#!/bin/sh\nexport POLICY=required\nexec mise x pi -- pi \"$@\"",
            "#!/bin/sh\nexec mise x other -- pi \"$@\"",
            "#!/bin/sh\nexec mise x pi -- pi \"$@\"; exit 0",
        ] {
            assert_eq!(static_manager_wrapper(script, "pi"), None, "{script}");
        }
    }
}

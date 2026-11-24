use std::path::{Path, PathBuf};

use super::{executable, is_executable};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShellPreference {
    first: Option<PathBuf>,
    fallback: bool,
}

impl ShellPreference {
    pub(crate) fn validate(value: &Path) -> Result<(), String> {
        let text = value.to_str().ok_or("Shell selection must be UTF-8")?;
        if text.is_empty() || text.len() > 4096 || text.chars().any(char::is_control) {
            return Err("Shell selection must be nonempty and at most 4096 bytes".into());
        }
        if !value.is_absolute()
            && (text.starts_with(['-', '.'])
                || !text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
        {
            return Err("Choose auto, system, an executable name or an absolute path".into());
        }
        Ok(())
    }

    pub(crate) fn new(value: Option<&Path>, required: bool) -> Self {
        match value.and_then(Path::to_str) {
            None | Some("auto" | "zrun") => Self {
                first: cfg!(unix).then(|| PathBuf::from("zrun")),
                fallback: true,
            },
            Some("system") => Self {
                first: None,
                fallback: true,
            },
            Some(value) => Self {
                first: Some(value.into()),
                fallback: !required,
            },
        }
    }

    // Library callers already provide an exact executable path.
    pub(crate) fn exact(path: &Path) -> Self {
        Self {
            first: Some(path.to_owned()),
            fallback: false,
        }
    }

    /// Try the same ordered candidates for discovery and for launching a PTY.
    pub(crate) fn select<T>(
        &self,
        mut use_shell: impl FnMut(&Path) -> Result<T, String>,
    ) -> Result<(PathBuf, T), String> {
        let mut candidates = Vec::with_capacity(3);
        if let Some(first) = &self.first {
            candidates.push((first.clone(), false));
        }
        if self.fallback {
            if let Some(path) = system_shell().filter(|path| !path.as_os_str().is_empty())
                && !candidates.iter().any(|(candidate, _)| candidate == &path)
            {
                candidates.push((path, true));
            }
            let fallback = fallback_shell();
            if !candidates
                .iter()
                .any(|(candidate, _)| candidate == &fallback)
            {
                candidates.push((fallback, true));
            }
        }
        let mut attempted = Vec::with_capacity(candidates.len());
        let mut last_error = String::new();
        let mut candidates = candidates.into_iter().peekable();
        while let Some((candidate, absolute)) = candidates.next() {
            let resolved = if absolute && !candidate.is_absolute() {
                Err("system shell path must be absolute".into())
            } else {
                resolve(&candidate)
            };
            let result = match resolved {
                Ok(path) if attempted.contains(&path) => continue,
                Ok(path) => {
                    attempted.push(path.clone());
                    use_shell(&path).map(|result| (path, result))
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(selected) => return Ok(selected),
                Err(error) => {
                    last_error = format!("Shell {candidate:?} failed ({error}).");
                    if let Some((next, _)) = candidates.peek() {
                        log("Warning", &format!("Trying {next:?}. {last_error}"));
                    }
                }
            }
        }
        let error = format!("No usable SSMCP shell. {last_error}");
        log("Error", &error);
        Err(error)
    }
}

/// Keep the launcher's interpreter usable when the inherited login shell disappeared.
pub(crate) fn launcher_environment(_path: &Path) -> std::collections::HashMap<String, String> {
    #[cfg(unix)]
    if _path.file_name().is_some_and(|name| name == "zrun") {
        let override_shell = std::env::var_os("ZRUN_SHELL").filter(|value| !value.is_empty());
        if let Some(override_shell) = override_shell {
            if resolve(Path::new(&override_shell)).is_ok() {
                return Default::default();
            }
            log(
                "Warning",
                &format!("ZRUN_SHELL {override_shell:?} is unavailable. Using the native shell."),
            );
        }
        return [(
            "ZRUN_SHELL".to_owned(),
            super::default_shell().to_string_lossy().into_owned(),
        )]
        .into();
    }
    Default::default()
}

fn log(level: &str, message: &str) {
    crate::client::render::status("shell", level, message);
}

fn resolve(candidate: &Path) -> Result<PathBuf, String> {
    if candidate.is_absolute() {
        if is_executable(candidate) {
            return Ok(candidate.to_owned());
        }
        return Err("executable is missing or cannot be executed".into());
    }
    let name = candidate
        .to_str()
        .ok_or("shell executable name must be UTF-8")?;
    let found = executable(name);
    #[cfg(unix)]
    if found.is_err()
        && name == "zrun"
        && let Ok(exe) = std::env::current_exe()
    {
        let companion = exe.with_file_name("zrun");
        if is_executable(&companion) {
            return Ok(companion);
        }
    }
    found
}

pub(super) fn system_shell() -> Option<PathBuf> {
    #[cfg(unix)]
    let variable = "SHELL";
    #[cfg(windows)]
    let variable = "COMSPEC";
    std::env::var_os(variable).map(PathBuf::from)
}

pub(super) fn fallback_shell() -> PathBuf {
    #[cfg(unix)]
    {
        "/bin/sh".into()
    }
    #[cfg(windows)]
    {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
            .join("System32/cmd.exe")
    }
}

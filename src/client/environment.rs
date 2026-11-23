//! Literal startup defaults and executable search paths; never executes an RC.
use crate::tool_repository::validate_environment as validate_for_platform;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::Path,
};

/// Build a complete plan without changing the process environment or filesystem.
/// The executable applies it while still single-threaded, before creating Tokio.
pub(super) fn startup() -> crate::Result<Vec<(OsString, OsString)>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if skips_environment(&args)? {
        return Ok(Vec::new());
    }
    let workspace = std::env::current_dir()?;
    let (global_bin, defaults) = crate::tool_repository::startup_environment(&workspace)?;
    let mut inherited = std::env::vars_os().collect::<Vec<_>>();
    // Ask the OS for PATH itself, including Windows' case-insensitive lookup.
    // Do not route existing bytes through String, even when they are not UTF-8.
    inherited.retain(|(name, _)| !same_name(name, "PATH", cfg!(windows)));
    if let Some(path) = std::env::var_os("PATH") {
        inherited.push((OsString::from("PATH"), path));
    }
    plan(
        &workspace,
        &global_bin,
        &defaults,
        &inherited,
        cfg!(windows),
    )
    .map_err(Into::into)
}

fn skips_environment(args: &[OsString]) -> Result<bool, String> {
    // Preserve diagnostic paths even when the RC or home directory is broken.
    if super::version::request(args)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(true);
    }
    if matches!(
        args.first().and_then(|arg| arg.to_str()),
        Some("paths" | "--help" | "--help-full" | "-h")
    ) {
        return Ok(true);
    }
    if super::tool_repository_arguments(args).is_some_and(|args| {
        matches!(
            args.first().and_then(|arg| arg.to_str()),
            Some("paths" | "--help" | "-h")
        )
    }) {
        return Ok(true);
    }
    // These transport/display-only children inherit the parent's prepared
    // environment. They do not own a new execution workspace or read its RC.
    Ok(matches!(
        args.first().and_then(|arg| arg.to_str()),
        Some("__relay" | "__local-relay" | "__tty-view" | "__rsync-transport")
    ))
}

fn same_name(left: &OsStr, right: &str, windows: bool) -> bool {
    if windows {
        left.to_str()
            .is_some_and(|left| left.eq_ignore_ascii_case(right))
    } else {
        left == OsStr::new(right)
    }
}

fn windows_units(value: &OsStr) -> Result<usize, String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        Ok(value.encode_wide().count())
    }
    #[cfg(not(windows))]
    {
        // The Windows branch of the pure planner is also exercised by tests
        // on Unix; production Unix plans preserve bytes without this conversion.
        value
            .to_str()
            .map(|value| value.encode_utf16().count())
            .ok_or_else(|| "cannot validate a non-Unicode value for Windows startup".into())
    }
}

fn validate_windows_pair(name: &OsStr, value: &OsStr) -> Result<(), String> {
    // Win32's 32,767-character limit includes the terminating NUL. Check
    // the name and value separately, using UTF-16 code units, not UTF-8 bytes.
    if windows_units(name)? >= 32_767 || windows_units(value)? >= 32_767 {
        return Err("Windows startup environment names and values must each fit within 32766 UTF-16 code units plus a terminating NUL".into());
    }
    Ok(())
}

fn plan(
    workspace: &Path,
    global_bin: &Path,
    defaults: &BTreeMap<String, String>,
    inherited: &[(OsString, OsString)],
    windows: bool,
) -> Result<Vec<(OsString, OsString)>, String> {
    validate_for_platform(defaults, windows)?;
    if !workspace.is_absolute() || !global_bin.is_absolute() {
        return Err("startup executable paths must be absolute".into());
    }
    let mut changes = Vec::new();
    for (name, value) in defaults {
        if !same_name(OsStr::new(name), "PATH", windows)
            && !inherited
                .iter()
                .any(|(existing, _)| same_name(existing, name, windows))
        {
            changes.push((
                OsString::from(name.as_str()),
                OsString::from(value.as_str()),
            ));
        }
    }

    // An explicitly inherited empty PATH still wins over a configured default.
    let base = inherited
        .iter()
        .find(|(name, _)| same_name(name, "PATH", windows))
        .map(|(_, value)| value.as_os_str())
        .or_else(|| {
            defaults
                .iter()
                .find(|(name, _)| same_name(OsStr::new(name), "PATH", windows))
                .map(|(_, value)| OsStr::new(value))
        });
    let project_bin = workspace.join(".ssmcp").join("bin");
    let mut search = vec![project_bin.clone()];
    if project_bin.as_path() != global_bin {
        search.push(global_bin.to_owned());
    }
    if let Some(base) = base {
        // Only remove our two exact prefixes, so nested launches do not grow
        // PATH. Keep the order and empty components of the inherited tail.
        search.extend(
            std::env::split_paths(base)
                .filter(|entry| entry != &project_bin && entry.as_path() != global_bin),
        );
    }
    let path = std::env::join_paths(search)
        .map_err(|_| "startup bin directories cannot be represented in this platform's PATH")?;
    if path.as_encoded_bytes().contains(&0) {
        return Err("startup PATH must not contain NUL characters".into());
    }
    changes.push((OsString::from("PATH"), path));
    if windows {
        // Validate every actual write, including the expanded PATH, before
        // main can apply any part of the plan. Setter failures can expose values.
        for (name, value) in &changes {
            validate_windows_pair(name, value)?;
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn roots() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join("ssmcp-environment-plan-fixture");
        (root.join("project"), root.join("home/.ssmcp/bin"))
    }

    fn defaults(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn inherited(entries: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        entries
            .iter()
            .map(|(key, value)| (OsString::from(*key), OsString::from(*value)))
            .collect()
    }

    fn path(changes: &[(OsString, OsString)]) -> Vec<PathBuf> {
        let value = &changes.iter().find(|(key, _)| key == "PATH").unwrap().1;
        std::env::split_paths(value).collect()
    }

    #[test]
    fn explicit_values_including_empty_win_and_defaults_remain_literal() {
        let (workspace, global) = roots();
        let changes = plan(
            &workspace,
            &global,
            &defaults(&[
                ("CHOICE", "configured"),
                ("EMPTY", "configured"),
                ("LITERAL", "$HOME/${OTHER}/~; $(not-a-command)"),
            ]),
            &inherited(&[("CHOICE", "inherited"), ("EMPTY", "")]),
            false,
        )
        .unwrap();
        assert!(
            !changes
                .iter()
                .any(|(key, _)| key == "CHOICE" || key == "EMPTY")
        );
        assert_eq!(
            changes[0],
            (
                OsString::from("LITERAL"),
                OsString::from("$HOME/${OTHER}/~; $(not-a-command)")
            )
        );
        assert_eq!(path(&changes), [workspace.join(".ssmcp/bin"), global]);
    }

    #[test]
    fn path_has_project_global_then_inherited_order_without_repeated_prefixes() {
        let (workspace, global) = roots();
        let project = workspace.join(".ssmcp/bin");
        let first = workspace.join("first");
        let last = workspace.join("last");
        let base = std::env::join_paths([&global, &first, &project, &last, &first]).unwrap();
        let changes = plan(
            &workspace,
            &global,
            &BTreeMap::new(),
            &[(OsString::from("PATH"), base)],
            false,
        )
        .unwrap();
        assert_eq!(
            path(&changes),
            [project, global, first.clone(), last, first]
        );
    }

    #[test]
    fn missing_and_explicitly_empty_path_are_distinct() {
        let (workspace, global) = roots();
        let fallback = workspace.join("fallback");
        let settings = defaults(&[("PATH", fallback.to_str().unwrap())]);
        let missing = plan(&workspace, &global, &settings, &[], false).unwrap();
        assert_eq!(
            path(&missing),
            [workspace.join(".ssmcp/bin"), global.clone(), fallback]
        );
        let empty = plan(
            &workspace,
            &global,
            &settings,
            &inherited(&[("PATH", "")]),
            false,
        )
        .unwrap();
        assert_eq!(
            path(&empty),
            [workspace.join(".ssmcp/bin"), global, PathBuf::new()]
        );
    }

    #[test]
    fn coincident_project_and_global_bins_are_added_once() {
        let (workspace, _) = roots();
        let bin = workspace.join(".ssmcp/bin");
        let changes = plan(&workspace, &bin, &BTreeMap::new(), &[], false).unwrap();
        assert_eq!(path(&changes), [bin]);
    }

    #[test]
    fn planning_does_not_create_missing_bin_directories() {
        let unique = format!(
            "ssmcp-env-no-create-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let workspace = root.join("project");
        let global = root.join("home/.ssmcp/bin");
        let changes = plan(&workspace, &global, &BTreeMap::new(), &[], false).unwrap();
        assert_eq!(path(&changes), [workspace.join(".ssmcp/bin"), global]);
        assert!(
            !root.exists(),
            "building a plan must not create any directory"
        );
    }

    #[test]
    fn invalid_names_values_and_configuration_locations_are_rejected() {
        for settings in [
            defaults(&[("", "value")]),
            defaults(&[("NAME=OTHER", "value")]),
            defaults(&[("1NAME", "value")]),
            defaults(&[("NÄME", "value")]),
            defaults(&[("GOOD", "nul\0value")]),
            defaults(&[("HOME", "/changed")]),
            defaults(&[("USERPROFILE", "/changed")]),
            defaults(&[("XDG_CONFIG_HOME", "/changed")]),
        ] {
            assert!(validate_for_platform(&settings, false).is_err());
        }
        assert!(validate_for_platform(&defaults(&[("home", "/changed")]), false).is_ok());
        assert!(validate_for_platform(&defaults(&[("home", "/changed")]), true).is_err());
    }

    #[test]
    fn windows_names_are_case_insensitive_but_cannot_collide_in_configuration() {
        let settings = defaults(&[("Path", "one"), ("PATH", "two")]);
        assert!(validate_for_platform(&settings, true).is_err());
        assert!(validate_for_platform(&settings, false).is_ok());
        let (workspace, global) = roots();
        let old = workspace.join("old");
        let changes = plan(
            &workspace,
            &global,
            &defaults(&[("Choice", "configured"), ("Path", "ignored")]),
            &[
                (OsString::from("CHOICE"), OsString::new()),
                (OsString::from("pAtH"), old.into_os_string()),
            ],
            true,
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            path(&changes),
            [workspace.join(".ssmcp/bin"), global, workspace.join("old")]
        );
    }

    #[test]
    fn windows_limits_include_nul_and_count_non_bmp_characters_twice() {
        let at_limit = "😀".repeat(16_383);
        assert_eq!(at_limit.encode_utf16().count(), 32_766);
        assert!(
            at_limit.len() > 32_767,
            "UTF-8 byte length is not the Windows limit"
        );
        assert!(validate_windows_pair(OsStr::new("TOKEN"), OsStr::new(&at_limit)).is_ok());
        let too_long = format!("{at_limit}x");
        let error = validate_windows_pair(OsStr::new("TOKEN"), OsStr::new(&too_long)).unwrap_err();
        assert!(!error.contains('😀'));
        assert!(!error.contains("TOKEN"));
        assert!(validate_windows_pair(OsStr::new(&"N".repeat(32_766)), OsStr::new("")).is_ok());
        assert!(validate_windows_pair(OsStr::new(&"N".repeat(32_767)), OsStr::new("")).is_err());
    }

    #[test]
    fn windows_rejects_oversized_defaults_and_composed_path_before_returning_a_plan() {
        let (workspace, global) = roots();
        let large = "x".repeat(32_767);
        assert!(
            plan(
                &workspace,
                &global,
                &defaults(&[("TOKEN", &large)]),
                &[],
                true
            )
            .is_err()
        );
        assert!(
            plan(
                &workspace,
                &global,
                &defaults(&[(&large, "value")]),
                &[],
                true
            )
            .is_err()
        );
        let inherited_path = OsString::from("x".repeat(32_766));
        assert!(
            plan(
                &workspace,
                &global,
                &defaults(&[("FIRST", "never-applied")]),
                &[(OsString::from("PATH"), inherited_path)],
                true
            )
            .is_err()
        );
        // A large unused default is never passed to the setter; explicit values win.
        let explicit = inherited(&[("TOKEN", "")]);
        assert!(
            plan(
                &workspace,
                &global,
                &defaults(&[("TOKEN", &large)]),
                &explicit,
                true
            )
            .is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn inherited_non_utf8_path_bytes_are_preserved() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let (workspace, global) = roots();
        let original = OsString::from_vec(b"/tmp/non-utf8-\xff:/usr/bin".to_vec());
        let changes = plan(
            &workspace,
            &global,
            &BTreeMap::new(),
            &[(OsString::from("PATH"), original)],
            false,
        )
        .unwrap();
        let paths = path(&changes);
        assert_eq!(paths[2].as_os_str().as_bytes(), b"/tmp/non-utf8-\xff");
        assert_eq!(paths[3], PathBuf::from("/usr/bin"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_components_with_semicolons_round_trip() {
        let (workspace, global) = roots();
        let workspace = workspace.join("project; with spaces");
        let old = global.join("old; directory");
        let original = std::env::join_paths([&old]).unwrap();
        let changes = plan(
            &workspace,
            &global,
            &BTreeMap::new(),
            &[(OsString::from("Path"), original)],
            true,
        )
        .unwrap();
        assert_eq!(path(&changes), [workspace.join(".ssmcp/bin"), global, old]);
    }

    #[cfg(unix)]
    #[test]
    fn an_unrepresentable_bin_fails_before_any_plan_is_returned() {
        let (workspace, global) = roots();
        assert!(
            plan(
                &workspace.join("colon:directory"),
                &global,
                &defaults(&[("GOOD", "must-not-be-applied")]),
                &[],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn diagnostics_and_transport_children_do_not_require_an_rc() {
        for args in [
            vec!["--version"],
            vec!["-V"],
            vec!["paths", "rc"],
            vec!["tools", "paths"],
            vec!["tool", "--help"],
            vec!["--help"],
            vec!["__relay"],
            vec!["__local-relay"],
            vec!["__tty-view", "directory", "0"],
        ] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            assert!(skips_environment(&args).unwrap());
        }
        assert!(!skips_environment(&["local".into()]).unwrap());
        assert!(!skips_environment(&["__session-run".into(), "saved".into()]).unwrap());
    }
}

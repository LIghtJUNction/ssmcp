//! Bounded, read-only observations. Presence never establishes a browser session.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

const MAX_PROCESSES: usize = 8192;
#[cfg(target_os = "linux")]
const MAX_PROCESS_RESULTS: usize = 64;
const MAX_COMMAND_BYTES: usize = 8192;
const MAX_PROFILES: usize = 16;
const MAX_EXTENSIONS: usize = 64;
const MAX_VERSIONS: usize = 16;
const MAX_JSON_BYTES: usize = 64 * 1024;
const MAX_JSON_READS: usize = 256;
const MAX_TOTAL_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRY_CANDIDATES: usize = 128;

#[derive(Default)]
struct Scan {
    profiles: usize,
    extensions: usize,
    json_reads: usize,
    json_bytes: usize,
    limited: bool,
    diagnostics: BTreeMap<&'static str, usize>,
}

impl Scan {
    fn note(&mut self, code: &'static str) {
        *self.diagnostics.entry(code).or_default() += 1;
    }
    fn limit(&mut self, code: &'static str) {
        self.limited = true;
        self.note(code);
    }
}

struct Root {
    family: &'static str,
    path: PathBuf,
}

pub(super) fn inspect() -> Value {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let roots = profile_roots(
        std::env::consts::OS,
        home.as_deref(),
        config.as_deref(),
        local.as_deref(),
    );
    let mut entries = Vec::new();
    if let Some(path) = std::env::var_os("SSMCP_PLAYWRIGHT_MCP") {
        entries.push(("operator_configured", PathBuf::from(path)));
    }
    let search = std::env::var_os("PATH").unwrap_or_default();
    let path_directories: Vec<_> = std::env::split_paths(&search).take(65).collect();
    for directory in path_directories.iter().take(64) {
        entries.push(("path", directory.join("playwright-mcp")));
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            entries.push((
                "path",
                directory.join(format!("playwright-mcp.{extension}")),
            ));
        }
    }
    for root in ["/usr/lib/node_modules", "/usr/local/lib/node_modules"] {
        #[cfg(unix)]
        entries.push((
            "standard_package_path",
            Path::new(root).join("@playwright/mcp/cli.js"),
        ));
        #[cfg(not(unix))]
        let _ = root;
    }
    if let Some(home) = &home {
        entries.push((
            "standard_package_path",
            home.join(".local/lib/node_modules/@playwright/mcp/cli.js"),
        ));
    }
    #[cfg(windows)]
    if let Some(app_data) = std::env::var_os("APPDATA") {
        entries.push((
            "standard_package_path",
            PathBuf::from(app_data).join("npm/node_modules/@playwright/mcp/cli.js"),
        ));
    }
    #[cfg(target_os = "linux")]
    let process_scope = Some((Path::new("/proc"), unsafe { libc::geteuid() }));
    #[cfg(not(target_os = "linux"))]
    let process_scope = None;
    let mut result = inspect_roots(home.as_deref(), &roots, process_scope, &entries);
    if path_directories.len() > 64 {
        result["limited"] = json!(true);
        result["diagnostics"]["path_directory_limit"] = json!(1);
    }
    result
}

fn profile_roots(
    os: &str,
    home: Option<&Path>,
    config: Option<&Path>,
    local: Option<&Path>,
) -> Vec<Root> {
    let Some(home) = home else { return Vec::new() };
    let (base, suffixes): (PathBuf, &[(&'static str, &'static str)]) = match os {
        "linux" => (
            config
                .filter(|p| p.is_absolute())
                .map_or_else(|| home.join(".config"), Path::to_owned),
            &[
                ("google_chrome", "google-chrome"),
                ("google_chrome", "google-chrome-beta"),
                ("google_chrome", "google-chrome-unstable"),
                ("chromium", "chromium"),
                ("microsoft_edge", "microsoft-edge"),
                ("microsoft_edge", "microsoft-edge-beta"),
                ("microsoft_edge", "microsoft-edge-dev"),
            ],
        ),
        "macos" => (
            home.join("Library/Application Support"),
            &[
                ("google_chrome", "Google/Chrome"),
                ("google_chrome", "Google/Chrome Beta"),
                ("google_chrome", "Google/Chrome Canary"),
                ("chromium", "Chromium"),
                ("microsoft_edge", "Microsoft Edge"),
                ("microsoft_edge", "Microsoft Edge Beta"),
            ],
        ),
        "windows" => (
            local
                .filter(|p| p.is_absolute())
                .map_or_else(|| home.join("AppData/Local"), Path::to_owned),
            &[
                ("google_chrome", "Google/Chrome/User Data"),
                ("google_chrome", "Google/Chrome Beta/User Data"),
                ("chromium", "Chromium/User Data"),
                ("microsoft_edge", "Microsoft/Edge/User Data"),
                ("microsoft_edge", "Microsoft/Edge Beta/User Data"),
            ],
        ),
        _ => return Vec::new(),
    };
    suffixes
        .iter()
        .map(|&(family, suffix)| Root {
            family,
            path: base.join(suffix),
        })
        .collect()
}

fn plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

fn disallowed_links(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() != 1
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false // Report the missing check; do not claim a verified link count.
    }
}

fn directories(
    path: &Path,
    scan_limit: usize,
    limit: usize,
    accept: fn(&str) -> bool,
    scan: &mut Scan,
) -> Vec<PathBuf> {
    if !plain_directory(path) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(path) else {
        scan.note("directory_unreadable");
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (index, entry) in entries.take(scan_limit + 1).enumerate() {
        if index == scan_limit {
            scan.limit("directory_entry_limit");
            break;
        }
        let Ok(entry) = entry else {
            scan.note("directory_entry_unreadable");
            continue;
        };
        let name = entry.file_name();
        if name.to_str().is_some_and(accept)
            && entry
                .file_type()
                .is_ok_and(|t| t.is_dir() && !t.is_symlink())
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    if paths.len() > limit {
        scan.limit("directory_candidate_limit");
        paths.truncate(limit);
    }
    paths
}

fn read_json(path: &Path, scan: &mut Scan) -> Option<Value> {
    if scan.json_reads >= MAX_JSON_READS || scan.json_bytes >= MAX_TOTAL_JSON_BYTES {
        scan.limit("json_budget_exhausted");
        return None;
    }
    scan.json_reads += 1;
    let cap = MAX_JSON_BYTES.min(MAX_TOTAL_JSON_BYTES.saturating_sub(scan.json_bytes + 1));
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || disallowed_links(&metadata)
        || metadata.len() > cap as u64
    {
        if metadata.len() > cap as u64 && cap < MAX_JSON_BYTES {
            scan.limit("json_budget_exhausted");
        }
        scan.note("json_file_rejected");
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).ok()?;
    let opened_metadata = file.metadata().ok()?;
    if !opened_metadata.is_file() || disallowed_links(&opened_metadata) {
        scan.note("json_file_rejected");
        return None;
    }
    let mut bytes = Vec::new();
    let read = file.take((cap + 1) as u64).read_to_end(&mut bytes);
    scan.json_bytes += bytes.len();
    if read.is_err() || bytes.len() > cap {
        scan.note("json_file_rejected");
        return None;
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => Some(value),
        _ => {
            scan.note("invalid_json_object");
            None
        }
    }
}

fn short_text(value: &Value, max_chars: usize) -> Option<String> {
    let text = value.as_str()?;
    if text.chars().count() > max_chars || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.to_owned())
}

fn component(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn extension_name(manifest: &Value, version: &Path, scan: &mut Scan) -> Option<String> {
    let name = short_text(&manifest["name"], 160)?;
    let Some(key) = name
        .strip_prefix("__MSG_")
        .and_then(|s| s.strip_suffix("__"))
    else {
        return Some(name);
    };
    let locale = manifest["default_locale"].as_str()?;
    if !component(locale, 32) || !component(key, 64) {
        scan.note("invalid_locale_or_message_key");
        return None;
    }
    let locales = version.join("_locales");
    let directory = locales.join(locale);
    if !plain_directory(&locales) || !plain_directory(&directory) {
        return None;
    }
    let messages = read_json(&directory.join("messages.json"), scan)?;
    let value = messages
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))?
        .1;
    short_text(&value["message"], 160)
}

fn extension_family(id: &str, name: Option<&str>) -> Option<(&'static str, &'static str)> {
    // These IDs were observed with these names in actual installed manifests.
    // Recognition is a discovery hint, not a publisher or permission assertion.
    match id {
        "hehggadaopoacecdllhhajmbjkdcmajg" => return Some(("chatgpt", "observed_extension_id")),
        "mmlmfjhmonkocbjadbfplnigmagldckm" => return Some(("playwright", "observed_extension_id")),
        _ => {}
    }
    let name = name?.to_lowercase();
    for keyword in ["chatgpt", "claude", "playwright"] {
        if name.contains(keyword) {
            return Some((keyword, "name_keyword"));
        }
    }
    None
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    let text = home.and_then(|h| path.strip_prefix(h).ok()).map_or_else(
        || path.to_string_lossy().into_owned(),
        |suffix| format!("~/{}", suffix.to_string_lossy()),
    );
    text.chars().filter(|c| !c.is_control()).take(512).collect()
}

fn inspect_roots(
    home: Option<&Path>,
    roots: &[Root],
    process_scope: Option<(&Path, u32)>,
    entries: &[(&str, PathBuf)],
) -> Value {
    let mut scan = Scan::default();
    #[cfg(not(unix))]
    scan.note("hardlink_detection_unavailable");
    let processes = running_browsers(process_scope, home, &mut scan);
    let mut profiles = Vec::new();
    let mut manifests = Vec::new();
    'roots: for root in roots.iter().take(16) {
        let candidates = directories(
            &root.path,
            256,
            MAX_PROFILES,
            |name| {
                matches!(name, "Default" | "Guest Profile" | "System Profile")
                    || name.strip_prefix("Profile ").is_some_and(|n| {
                        !n.is_empty() && n.len() <= 16 && n.bytes().all(|b| b.is_ascii_digit())
                    })
            },
            &mut scan,
        );
        for profile in candidates {
            if scan.profiles >= MAX_PROFILES {
                scan.limit("profile_limit");
                break 'roots;
            }
            scan.profiles += 1;
            let profile_name = profile.file_name().unwrap().to_string_lossy().into_owned();
            profiles.push(json!({"browser":root.family,"root":display_path(&root.path,home),"profile":profile_name}));
            for extension in directories(
                &profile.join("Extensions"),
                128,
                MAX_EXTENSIONS,
                |name| name.len() == 32 && name.bytes().all(|b| (b'a'..=b'p').contains(&b)),
                &mut scan,
            ) {
                if scan.extensions >= MAX_EXTENSIONS {
                    scan.limit("extension_limit");
                    break 'roots;
                }
                scan.extensions += 1;
                let id = extension
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                for version in directories(
                    &extension,
                    64,
                    MAX_VERSIONS,
                    |name| {
                        name.len() <= 128
                            && name.starts_with(|c: char| c.is_ascii_digit())
                            && name.bytes().all(|b| {
                                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
                            })
                    },
                    &mut scan,
                ) {
                    if scan.json_reads >= MAX_JSON_READS || scan.json_bytes >= MAX_TOTAL_JSON_BYTES
                    {
                        scan.limit("json_budget_exhausted");
                        break 'roots;
                    }
                    let Some(manifest) = read_json(&version.join("manifest.json"), &mut scan)
                    else {
                        continue;
                    };
                    let name = extension_name(&manifest, &version, &mut scan);
                    let Some((family, basis)) = extension_family(&id, name.as_deref()) else {
                        continue;
                    };
                    let permissions: Vec<_> = [
                        "debugger",
                        "activeTab",
                        "tabs",
                        "tabGroups",
                        "nativeMessaging",
                        "scripting",
                        "sidePanel",
                    ]
                    .into_iter()
                    .filter(|permission| {
                        manifest["permissions"].as_array().is_some_and(|values| {
                            values.iter().any(|v| v.as_str() == Some(*permission))
                        })
                    })
                    .collect();
                    manifests.push(json!({
                        "browser":root.family,"root":display_path(&root.path,home),"profile":profile_name,
                        "id":id,"family_hint":family,"match_basis":basis,"name":name,
                        "version":short_text(&manifest["version"],64),"manifest_version":manifest["manifest_version"].as_u64(),
                        "version_directory":version.file_name().unwrap().to_string_lossy(),"permissions":permissions,
                        "state":"manifest_present","enabled":"not_inspected","paired":"not_inspected","callable":"not_verified"
                    }));
                    if manifests.len() >= 64 {
                        scan.limit("matching_manifest_limit");
                        break 'roots;
                    }
                }
            }
        }
    }
    let mut entry_files = Vec::new();
    let mut seen = HashSet::new();
    let mut candidates_checked = 0;
    if entries.len() > MAX_ENTRY_CANDIDATES {
        scan.limit("entry_candidate_limit")
    }
    for (index, (source, path)) in entries.iter().take(MAX_ENTRY_CANDIDATES).enumerate() {
        candidates_checked = index + 1;
        if !path.is_absolute() {
            if *source == "operator_configured" {
                scan.note("configured_entry_requires_absolute_path");
            }
            continue;
        }
        if let Ok(real) = path.canonicalize()
            && real.is_file()
            && seen.insert(real.clone())
        {
            entry_files.push(json!({"source":source,"path":display_path(path,home),"resolved":display_path(&real,home),"state":"file_present","protocol_handshake":"not_attempted"}));
        }
        if entry_files.len() >= 16 {
            scan.limit("entry_result_limit");
            break;
        }
    }
    json!({
        "discovery_only":true,"selected_browser":null,"running_browsers":processes,
        "profiles":profiles,"extension_manifests":manifests,
        "playwright_mcp":{"state":if entry_files.is_empty(){"not_found_in_observed_paths"}else{"entry_candidates_present"},"entries":entry_files,"candidates_checked":candidates_checked,"protocol_handshake":"not_attempted"},
        "filesystem_observation":{"hardlink_check":if cfg!(unix){"enforced"}else{"unsupported"},"transactional_snapshot":false},
        "limited":scan.limited,"diagnostics":scan.diagnostics,
        "scanned":{"profiles":scan.profiles,"extension_candidates":scan.extensions,"json_read_attempts":scan.json_reads,"json_bytes":scan.json_bytes},
        "limits":{"process_entries":MAX_PROCESSES,"cmdline_bytes":MAX_COMMAND_BYTES,"profiles":MAX_PROFILES,"extension_candidates":MAX_EXTENSIONS,"versions_per_extension":MAX_VERSIONS,"json_file_bytes":MAX_JSON_BYTES,"json_read_attempts":MAX_JSON_READS,"json_bytes_total":MAX_TOTAL_JSON_BYTES,"path_directories":64,"entry_candidates":MAX_ENTRY_CANDIDATES},
        "scope":["Only standard profile manifest metadata is inspected; enabled state and connection approval are not inferred.","No browser, model, MCP server or package installer is started.","Only manifest.json and safe locale messages.json are requested; sensitive browser stores are not requested by path.","Filesystem checks do not provide an atomic boundary against concurrent directory replacement; check hardlink_check for platform coverage.","Process observation is supported only on Linux and only for the current UID."]
    })
}

#[cfg(any(target_os = "linux", test))]
fn browser_family(executable: &Path) -> Option<&'static str> {
    let reported = executable.to_string_lossy();
    let executable = Path::new(reported.strip_suffix(" (deleted)").unwrap_or(&reported));
    let base = executable.file_name()?.to_str()?;
    let path = executable.to_string_lossy();
    if base == "msedge" || base == "microsoft-edge" {
        Some("microsoft_edge")
    } else if matches!(base, "chromium" | "chromium-browser" | "chromium-bin")
        || (base == "chrome" && path.contains("/chromium/"))
    {
        Some("chromium")
    } else if matches!(
        base,
        "google-chrome" | "google-chrome-stable" | "google-chrome-beta" | "google-chrome-unstable"
    ) || (base == "chrome" && path.contains("/google/chrome"))
    {
        Some("google_chrome")
    } else {
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn command_flags(bytes: &[u8]) -> Option<(Option<u16>, bool)> {
    let text = std::str::from_utf8(bytes).ok()?;
    // Chromium may rewrite argv into a single NUL-terminated process title.
    // Inspect flag tokens in both representations; never return the raw argv.
    let words: Vec<_> = text
        .split('\0')
        .flat_map(str::split_ascii_whitespace)
        .map(|s| s.trim_matches(['\'', '"']))
        .collect();
    let mut port = None;
    let mut pipe = false;
    for (i, word) in words.iter().enumerate() {
        if *word == "--type" || word.starts_with("--type=") {
            return None;
        }
        if *word == "--remote-debugging-pipe" {
            pipe = true;
        }
        if let Some(value) = word.strip_prefix("--remote-debugging-port=").or_else(|| {
            if *word == "--remote-debugging-port" {
                words.get(i + 1).copied()
            } else {
                None
            }
        }) && !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_digit())
        {
            port = value.parse().ok();
        }
    }
    (!words.is_empty()).then_some((port, pipe))
}

#[cfg(target_os = "linux")]
fn running_browsers(scope: Option<(&Path, u32)>, home: Option<&Path>, scan: &mut Scan) -> Value {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let Some((root, uid)) = scope else {
        return json!({"status":"unsupported","items":[]});
    };
    let Ok(entries) = fs::read_dir(root) else {
        scan.note("process_directory_unreadable");
        return json!({"status":"unavailable","items":[]});
    };
    let mut processes = Vec::new();
    for (index, entry) in entries.take(MAX_PROCESSES + 1).enumerate() {
        if index == MAX_PROCESSES {
            scan.limit("process_entry_limit");
            break;
        }
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|p| *p > 0)
        else {
            continue;
        };
        if !entry
            .file_type()
            .is_ok_and(|t| t.is_dir() && !t.is_symlink())
            || !entry.metadata().is_ok_and(|m| m.uid() == uid)
        {
            continue;
        }
        let path = entry.path();
        let Ok(executable) = fs::read_link(path.join("exe")) else {
            scan.note("process_executable_unreadable");
            continue;
        };
        let Some(family) = browser_family(&executable) else {
            continue;
        };
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path.join("cmdline"))
        else {
            scan.note("process_cmdline_unreadable");
            continue;
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            scan.note("process_cmdline_unreadable");
            continue;
        }
        let mut bytes = Vec::new();
        if file
            .take((MAX_COMMAND_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() > MAX_COMMAND_BYTES
        {
            scan.limit("process_cmdline_limit");
            continue;
        }
        if std::str::from_utf8(&bytes).is_err() {
            scan.note("process_cmdline_invalid_utf8");
            continue;
        }
        let Some((port, pipe)) = command_flags(&bytes) else {
            continue;
        };
        processes.push(json!({"pid":pid,"browser":family,"executable":display_path(&executable,home),"state":"running_main_process","remote_debugging_port_requested":port,"remote_debugging_pipe":pipe,"protocol_handshake":"not_attempted"}));
        if processes.len() >= MAX_PROCESS_RESULTS {
            scan.limit("process_result_limit");
            break;
        }
    }
    processes.sort_by_key(|p| p["pid"].as_u64());
    json!({"status":"observed","scope":"current_uid","items":processes})
}

#[cfg(not(target_os = "linux"))]
fn running_browsers(_: Option<(&Path, u32)>, _: Option<&Path>, _: &mut Scan) -> Value {
    json!({"status":"unsupported","items":[],"reason":"Running browser process discovery is currently supported only on Linux."})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-browser-discovery-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn chrome(&self) -> Root {
            Root {
                family: "google_chrome",
                path: self.0.join("Chrome"),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn id(number: usize) -> String {
        format!("{number:032x}")
            .chars()
            .map(|c| (b'a' + c.to_digit(16).unwrap() as u8) as char)
            .collect()
    }
    fn manifest(root: &Path, profile: &str, id: &str, version: &str, value: Value) -> PathBuf {
        let directory = root.join(profile).join("Extensions").join(id).join(version);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        directory
    }

    #[test]
    fn localized_unicode_metadata_does_not_claim_a_connection_or_read_sensitive_stores() {
        let fixture = Fixture::new();
        let root = Root {
            family: "google_chrome",
            path: fixture.0.join("Chrome 配置 🧪"),
        };
        let version = manifest(
            &root.path,
            "Default",
            &id(1),
            "1.0_0",
            json!({"name":"__MSG_extensionName__","default_locale":"zh_CN","version":"1.0","manifest_version":3,"permissions":["debugger","SECRET_IGNORED_PERMISSION"],"key":"SECRET_PRIVATE_KEY","description":"SECRET_DESCRIPTION"}),
        );
        let locale = version.join("_locales/zh_CN");
        fs::create_dir_all(&locale).unwrap();
        fs::write(locale.join("messages.json"),br#"{"extensionName":{"message":"ChatGPT \u4f60\u597d \ud83e\uddea"},"secret":{"message":"SECRET_UNUSED_MESSAGE"}}"#).unwrap();
        for name in [
            "Preferences",
            "Local State",
            "Cookies",
            "History",
            "DevToolsActivePort",
        ] {
            fs::write(root.path.join(name), "SECRET_SENSITIVE_STORE").unwrap();
        }
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        let found = &result["extension_manifests"][0];
        assert_eq!(found["name"], "ChatGPT 你好 🧪");
        assert_eq!(found["state"], "manifest_present");
        assert_eq!(found["enabled"], "not_inspected");
        assert_eq!(found["paired"], "not_inspected");
        assert_eq!(found["permissions"], json!(["debugger"]));
        assert!(result["selected_browser"].is_null());
        assert!(!result.to_string().contains("SECRET"));
    }

    #[test]
    fn invalid_locale_does_not_escape_to_adjacent_messages() {
        let fixture = Fixture::new();
        let root = fixture.chrome();
        let version = manifest(
            &root.path,
            "Default",
            &id(2),
            "1.0_0",
            json!({"name":"__MSG_name__","default_locale":"../../private","version":"1.0"}),
        );
        fs::create_dir_all(version.join("_locales")).unwrap();
        let secret = version.parent().unwrap().join("private");
        fs::create_dir(&secret).unwrap();
        fs::write(
            secret.join("messages.json"),
            br#"{"name":{"message":"Claude SECRET"}}"#,
        )
        .unwrap();
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        assert_eq!(result["extension_manifests"], json!([]));
        assert_eq!(result["diagnostics"]["invalid_locale_or_message_key"], 1);
        assert!(!result.to_string().contains("SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_manifests_and_locale_directories_are_not_read() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let root = fixture.chrome();
        let first = manifest(&root.path, "Default", &id(3), "1.0_0", json!({}));
        let sensitive = fixture.0.join("sensitive.json");
        fs::write(&sensitive, br#"{"name":"ChatGPT SECRET"}"#).unwrap();
        fs::remove_file(first.join("manifest.json")).unwrap();
        symlink(&sensitive, first.join("manifest.json")).unwrap();
        let second = manifest(
            &root.path,
            "Default",
            &id(4),
            "1.0_0",
            json!({"name":"__MSG_name__","default_locale":"en"}),
        );
        fs::create_dir(second.join("_locales")).unwrap();
        let private = fixture.0.join("private");
        fs::create_dir(&private).unwrap();
        fs::write(
            private.join("messages.json"),
            br#"{"name":{"message":"Claude SECRET"}}"#,
        )
        .unwrap();
        symlink(private, second.join("_locales/en")).unwrap();
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        assert_eq!(result["extension_manifests"], json!([]));
        assert!(!result.to_string().contains("SECRET"));
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_manifests_and_locale_messages_are_rejected() {
        let fixture = Fixture::new();
        let root = fixture.chrome();
        let first = manifest(&root.path, "Default", &id(5), "1.0_0", json!({}));
        let private_manifest = fixture.0.join("private-manifest.json");
        fs::write(
            &private_manifest,
            br#"{"name":"ChatGPT SECRET","version":"SECRET"}"#,
        )
        .unwrap();
        fs::remove_file(first.join("manifest.json")).unwrap();
        fs::hard_link(&private_manifest, first.join("manifest.json")).unwrap();
        let second = manifest(
            &root.path,
            "Default",
            &id(6),
            "1.0_0",
            json!({"name":"__MSG_name__","default_locale":"en"}),
        );
        fs::create_dir_all(second.join("_locales/en")).unwrap();
        let private_messages = fixture.0.join("private-messages.json");
        fs::write(
            &private_messages,
            br#"{"name":{"message":"Claude SECRET"}}"#,
        )
        .unwrap();
        fs::hard_link(&private_messages, second.join("_locales/en/messages.json")).unwrap();
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        assert_eq!(result["extension_manifests"], json!([]));
        assert_eq!(result["diagnostics"]["json_file_rejected"], 2);
        assert_eq!(
            result["scanned"]["json_bytes"],
            fs::metadata(second.join("manifest.json")).unwrap().len()
        );
        assert_eq!(
            result["filesystem_observation"]["hardlink_check"],
            "enforced"
        );
        assert!(!result.to_string().contains("SECRET"));
    }

    #[test]
    fn candidate_count_reports_only_entries_examined_before_the_result_limit() {
        let fixture = Fixture::new();
        let mut entries = Vec::new();
        for number in 0..32 {
            let path = fixture.0.join(format!("entry-{number}"));
            fs::write(&path, "not executed").unwrap();
            entries.push(("path", path));
        }
        let result = inspect_roots(Some(&fixture.0), &[], None, &entries);
        assert_eq!(
            result["playwright_mcp"]["entries"]
                .as_array()
                .unwrap()
                .len(),
            16
        );
        assert_eq!(result["playwright_mcp"]["candidates_checked"], 16);
        assert_eq!(result["diagnostics"]["entry_result_limit"], 1);
        assert!(result["scanned"].get("json_reads").is_none());
        assert_eq!(result["scanned"]["json_read_attempts"], 0);
    }

    #[test]
    fn profiles_extensions_versions_and_json_budgets_are_bounded() {
        let fixture = Fixture::new();
        let root = fixture.chrome();
        for number in 0..17 {
            fs::create_dir_all(root.path.join(format!("Profile {number}"))).unwrap();
        }
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        assert_eq!(result["profiles"].as_array().unwrap().len(), 16);
        assert_eq!(result["limited"], true);
        let root = Root {
            family: "google_chrome",
            path: fixture.0.join("OtherChrome"),
        };
        for number in 0..65 {
            manifest(
                &root.path,
                "Default",
                &id(number),
                "1.0_0",
                json!({"name":"Other"}),
            );
        }
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        assert_eq!(result["scanned"]["extension_candidates"], 64);
        assert_eq!(result["limited"], true);
        let root = Root {
            family: "google_chrome",
            path: fixture.0.join("Versions"),
        };
        for number in 0..17 {
            manifest(
                &root.path,
                "Default",
                &id(1),
                &format!("1.{number}_0"),
                json!({"name":"Playwright"}),
            );
        }
        let result = inspect_roots(Some(&fixture.0), &[root], None, &[]);
        assert_eq!(result["extension_manifests"].as_array().unwrap().len(), 16);
        assert_eq!(result["limited"], true);

        let json = fixture.0.join("bounded.json");
        fs::write(&json, b"{}").unwrap();
        let mut scan = Scan {
            json_reads: MAX_JSON_READS - 1,
            ..Scan::default()
        };
        assert!(read_json(&json, &mut scan).is_some());
        assert!(read_json(&json, &mut scan).is_none());
        assert!(scan.limited);
        let mut scan = Scan {
            json_bytes: MAX_TOTAL_JSON_BYTES - 2,
            ..Scan::default()
        };
        assert!(read_json(&json, &mut scan).is_none());
        assert!(scan.json_bytes <= MAX_TOTAL_JSON_BYTES);
        assert!(scan.limited);
        fs::write(&json, vec![b' '; MAX_JSON_BYTES + 1]).unwrap();
        let mut scan = Scan::default();
        assert!(read_json(&json, &mut scan).is_none());
        assert_eq!(scan.json_bytes, 0);
    }

    #[test]
    fn executable_candidates_are_presence_only_and_platform_roots_remain_distinct() {
        let fixture = Fixture::new();
        let entry = fixture.0.join("playwright-mcp.js");
        fs::write(
            &entry,
            "throw new Error('discovery must never execute this file')",
        )
        .unwrap();
        let result = inspect_roots(
            Some(&fixture.0),
            &[],
            None,
            &[
                ("operator_configured", entry),
                ("path", fixture.0.join("missing")),
            ],
        );
        assert_eq!(
            result["playwright_mcp"]["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            result["playwright_mcp"]["protocol_handshake"],
            "not_attempted"
        );
        assert_eq!(
            result["playwright_mcp"]["entries"][0]["state"],
            "file_present"
        );
        for os in ["linux", "macos", "windows"] {
            let roots = profile_roots(os, Some(&fixture.0), None, None);
            assert!(roots.iter().any(|r| r.family == "google_chrome"));
            assert!(roots.iter().any(|r| r.family == "chromium"));
            assert!(roots.iter().any(|r| r.family == "microsoft_edge"));
        }
        assert_eq!(
            browser_family(Path::new("/opt/google/chrome/chrome")),
            Some("google_chrome")
        );
        assert_eq!(
            browser_family(Path::new("/usr/lib/chromium/chromium")),
            Some("chromium")
        );
        assert_eq!(
            browser_family(Path::new("/opt/google/chrome/chrome (deleted)")),
            Some("google_chrome")
        );
        assert_eq!(browser_family(Path::new("/unknown/chrome")), None);
    }

    #[test]
    fn normal_and_collapsed_process_titles_do_not_turn_children_into_browsers() {
        assert!(command_flags(b"/opt/google/chrome/chrome\0--type=renderer\0").is_none());
        assert!(
            command_flags(b"/opt/google/chrome/chrome --type=zygote --secret=SECRET\0").is_none()
        );
        assert!(command_flags(b"chrome --type renderer\0").is_none());
        assert_eq!(
            command_flags(b"chrome\0--remote-debugging-port=9222\0"),
            Some((Some(9222), false))
        );
        assert_eq!(
            command_flags(b"chrome --remote-debugging-port 0 --remote-debugging-pipe\0"),
            Some((Some(0), true))
        );
        assert_eq!(
            command_flags(b"chrome --remote-debugging-port=999999\0"),
            Some((None, false))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_process_inventory_is_uid_scoped_bounded_and_never_returns_arguments() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let proc = fixture.0.join("proc");
        for (pid, command) in [
            (
                1,
                b"chrome --remote-debugging-port=9222 --token=SECRET\0".as_slice(),
            ),
            (2, b"chrome --type=renderer --token=SECRET\0".as_slice()),
            (3, b"chrome\0".as_slice()),
        ] {
            let path = proc.join(pid.to_string());
            fs::create_dir_all(&path).unwrap();
            symlink("/opt/google/chrome/chrome", path.join("exe")).unwrap();
            fs::write(path.join("cmdline"), command).unwrap();
        }
        fs::write(proc.join("3/cmdline"), vec![b'a'; MAX_COMMAND_BYTES + 1]).unwrap();
        let uid = unsafe { libc::geteuid() };
        let result = inspect_roots(Some(&fixture.0), &[], Some((&proc, uid)), &[]);
        assert_eq!(
            result["running_browsers"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(result["running_browsers"]["items"][0]["pid"], 1);
        assert_eq!(
            result["running_browsers"]["items"][0]["remote_debugging_port_requested"],
            9222
        );
        assert_eq!(result["limited"], true);
        assert!(!result.to_string().contains("SECRET"));
        let other_uid = inspect_roots(
            Some(&fixture.0),
            &[],
            Some((&proc, uid.wrapping_add(1))),
            &[],
        );
        assert_eq!(other_uid["running_browsers"]["items"], json!([]));
    }
}

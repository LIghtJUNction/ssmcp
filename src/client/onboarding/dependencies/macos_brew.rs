//! Bounded Homebrew bottle retrieval. Only archive routing changes between
//! attempts; Homebrew still obtains/verifies formula checksums through its
//! existing metadata configuration. Never put the install transaction on a timer.
use super::{TemporaryDirectory, run_program};
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const ROUTES: [(&str, &str); 3] = [
    ("Homebrew", "https://ghcr.io/v2/homebrew/core"),
    (
        "TUNA",
        "https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles",
    ),
    ("USTC", "https://mirrors.ustc.edu.cn/homebrew-bottles"),
];

#[derive(Clone, Copy)]
struct FetchLimits {
    per_route: Duration,
    total: Duration,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            per_route: Duration::from_secs(180),
            total: Duration::from_secs(480),
        }
    }
}

// bin/brew reads these files AFTER inheriting its environment. Refuse a
// conflicting managed policy instead of overriding it or accidentally sending a
// private registry credential to an automatically selected public mirror.
const CONTROLLED_ENV: &[(&str, Option<&str>)] = &[
    ("HOMEBREW_BOTTLE_DOMAIN", None),
    ("HOMEBREW_ARTIFACT_DOMAIN", Some("")),
    ("HOMEBREW_ARTIFACT_DOMAIN_NO_FALLBACK", Some("")),
    ("HOMEBREW_DOCKER_REGISTRY_TOKEN", Some("")),
    ("HOMEBREW_DOCKER_REGISTRY_BASIC_AUTH_TOKEN", Some("")),
    ("HOMEBREW_GITHUB_PACKAGES_AUTH", Some("")),
    ("HOMEBREW_CURLRC", None),
    ("HOMEBREW_CURL_RETRIES", Some("0")),
    ("HOMEBREW_NO_AUTO_UPDATE", Some("1")),
];

fn validate_environment_files(paths: &[PathBuf]) -> Result<(), String> {
    for path in paths {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect Homebrew download policy {}: {error}",
                    path.display()
                ));
            }
        };
        let mut text = String::new();
        file.take(256 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if text.len() > 256 * 1024 {
            return Err(format!(
                "Homebrew policy {} is too large to inspect",
                path.display()
            ));
        }
        for line in text.lines() {
            let Some((name, value)) = line.trim_start().split_once('=') else {
                continue;
            };
            if CONTROLLED_ENV
                .iter()
                .any(|(controlled, compatible)| *controlled == name && *compatible != Some(value))
            {
                return Err(format!(
                    "Homebrew policy {} sets {name}, which can override bounded public bottle routing; install OpenSSH with that managed policy or adjust it explicitly before rerunning setup. No Homebrew configuration was changed",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) async fn install(brew: &Path) -> Result<(), String> {
    let mut paths = vec![PathBuf::from("/etc/homebrew/brew.env")];
    if let Some(prefix) = brew.parent().and_then(Path::parent) {
        paths.push(prefix.join("etc/homebrew/brew.env"));
    }
    // executable() resolves symlinks. Intel Homebrew can restore /usr/local as
    // its prefix when that entry points at /usr/local/Homebrew/bin/brew.
    if let (Ok(selected), Ok(intel_entry)) = (
        std::fs::canonicalize(brew),
        std::fs::canonicalize("/usr/local/bin/brew"),
    ) && selected == intel_entry
    {
        paths.push(PathBuf::from("/usr/local/etc/homebrew/brew.env"));
    }
    let user_config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| std::env::var_os("HOMEBREW_XDG_CONFIG_HOME").filter(|path| !path.is_empty()))
        .map(|path| PathBuf::from(path).join("homebrew"))
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".homebrew")));
    if let Some(config) = user_config {
        paths.push(config.join("brew.env"));
    }
    validate_environment_files(&paths)?;
    install_with_limits(brew, FetchLimits::default()).await
}

fn environment(curlrc: &Path, bottle_domain: &str) -> Vec<(&'static str, OsString)> {
    vec![
        ("HOMEBREW_NO_AUTO_UPDATE", "1".into()),
        ("HOMEBREW_NO_INSTALL_CLEANUP", "1".into()),
        ("HOMEBREW_CURL_RETRIES", "0".into()),
        ("HOMEBREW_CURLRC", curlrc.as_os_str().to_owned()),
        ("HOMEBREW_BOTTLE_DOMAIN", bottle_domain.into()),
        // A general artifact proxy would otherwise take precedence over this
        // route and could change metadata routing too. Empty values are unset
        // in Homebrew's environment accessors; the caller's environment remains
        // untouched. API_DOMAIN and local formula taps are never rewritten.
        ("HOMEBREW_ARTIFACT_DOMAIN", OsString::new()),
        ("HOMEBREW_ARTIFACT_DOMAIN_NO_FALLBACK", OsString::new()),
        // GitHub Packages also downloads legacy-mirror manifests. Remove
        // inherited private registry authentication; brew can recreate its
        // public anonymous GHCR token without forwarding a user secret.
        ("HOMEBREW_DOCKER_REGISTRY_TOKEN", OsString::new()),
        ("HOMEBREW_DOCKER_REGISTRY_BASIC_AUTH_TOKEN", OsString::new()),
        ("HOMEBREW_GITHUB_PACKAGES_AUTH", OsString::new()),
    ]
}

async fn install_with_limits(brew: &Path, limits: FetchLimits) -> Result<(), String> {
    let temp = TemporaryDirectory::create(&std::env::temp_dir())?;
    // Only an absolute CURLRC selects the explicit file in Homebrew. A caller
    // may supply a relative TMPDIR, so normalize the directory after creating it.
    let curlrc = std::fs::canonicalize(&temp.0)
        .map_err(|error| error.to_string())?
        .join("curlrc");
    std::fs::write(
        &curlrc,
        "connect-timeout = 8\nspeed-time = 12\nspeed-limit = 65536\nmax-time = 180\nretry = 0\nproto = \"=https\"\nproto-redir = \"=https\"\n",
    )
    .map_err(|error| error.to_string())?;
    let started = tokio::time::Instant::now();
    let fetch = ["fetch", "--deps", "--force-bottle", "homebrew/core/openssh"].map(OsString::from);
    let mut errors = Vec::new();
    for (label, domain) in ROUTES {
        let remaining = limits.total.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let env = environment(&curlrc, domain);
        eprintln!(
            "[ssmcp setup] Fetching checksum-verified OpenSSH bottles via {label} (public archives only)."
        );
        match run_program(brew, &fetch, &env, Some(limits.per_route.min(remaining))).await {
            Ok(()) => {
                if started.elapsed() >= limits.total {
                    break;
                }
                // Keep the successful route for cache identity. Switching back
                // to the official URL here could unnecessarily fetch it again.
                // Applying a package must never be killed by a download timer.
                return run_program(
                    brew,
                    &["install", "--force-bottle", "homebrew/core/openssh"].map(OsString::from),
                    &env,
                    None,
                )
                .await;
            }
            Err(error) => errors.push(format!("{label}: {error}")),
        }
    }
    Err(format!(
        "Homebrew bottle retrieval failed within its bounded routes (shared download budget {}s); no installation was started. {}",
        limits.total.as_secs(),
        errors.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_brew(body: &str) -> (TemporaryDirectory, PathBuf) {
        let temp = TemporaryDirectory::create(&std::env::temp_dir()).unwrap();
        let brew = temp.0.join("brew");
        let script = format!(
            "#!/bin/sh\nset -eu\nroot=$(dirname \"$0\")\nprintf '%s|%s\\n' \"$1\" \"$HOMEBREW_BOTTLE_DOMAIN\" >> \"$root/attempts\"\n[ -z \"$HOMEBREW_DOCKER_REGISTRY_TOKEN\" ]\n[ -z \"$HOMEBREW_DOCKER_REGISTRY_BASIC_AUTH_TOKEN\" ]\n[ -z \"$HOMEBREW_GITHUB_PACKAGES_AUTH\" ]\n[ -z \"$HOMEBREW_ARTIFACT_DOMAIN\" ]\n[ \"$HOMEBREW_NO_AUTO_UPDATE\" = 1 ]\n[ \"$HOMEBREW_CURL_RETRIES\" = 0 ]\n[ -f \"$HOMEBREW_CURLRC\" ]\ncase \"$1\" in\nfetch) [ \"$#\" = 4 ] && [ \"$2\" = --deps ] && [ \"$3\" = --force-bottle ] && [ \"$4\" = homebrew/core/openssh ] ;;\ninstall) [ \"$#\" = 3 ] && [ \"$2\" = --force-bottle ] && [ \"$3\" = homebrew/core/openssh ] ;;\n*) exit 91 ;;\nesac\n{body}\n"
        );
        std::fs::write(&brew, script).unwrap();
        std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o700)).unwrap();
        (temp, brew)
    }

    #[tokio::test]
    async fn failed_sources_fall_back_in_order_and_install_only_the_verified_cache() {
        let (temp, brew) = fake_brew(
            r#"if [ "$1" = install ]; then
  [ -f "$root/verified" ] && printf applied > "$root/applied"
  exit 0
fi
case "$HOMEBREW_BOTTLE_DOMAIN" in
  *ghcr.io*) exit 51 ;;
  *tuna*) exit 52 ;;
  *ustc*) printf verified > "$root/verified" ;;
  *) exit 90 ;;
esac"#,
        );
        install_with_limits(&brew, FetchLimits::default())
            .await
            .unwrap();
        let attempts = std::fs::read_to_string(temp.0.join("attempts")).unwrap();
        assert_eq!(
            attempts.lines().collect::<Vec<_>>(),
            vec![
                format!("fetch|{}", ROUTES[0].1),
                format!("fetch|{}", ROUTES[1].1),
                format!("fetch|{}", ROUTES[2].1),
                format!("install|{}", ROUTES[2].1),
            ]
        );
        assert!(temp.0.join("applied").exists());
        let env = environment(Path::new("/private/curlrc"), ROUTES[1].1);
        assert!(!env.iter().any(|(key, _)| *key == "HOMEBREW_API_DOMAIN"));
    }

    #[tokio::test]
    async fn rejected_archives_never_start_installation() {
        let (temp, brew) = fake_brew("exit 52");
        let error = install_with_limits(&brew, FetchLimits::default())
            .await
            .unwrap_err();
        assert!(error.contains("no installation was started"), "{error}");
        let attempts = std::fs::read_to_string(temp.0.join("attempts")).unwrap();
        assert_eq!(attempts.lines().count(), 3);
        assert!(!attempts.contains("install|"));
    }

    #[tokio::test]
    async fn stalled_route_falls_back_but_package_application_is_not_timed_out() {
        let (temp, brew) = fake_brew(
            r#"if [ "$1" = install ]; then
  sleep 4
  printf applied > "$root/applied"
elif [ "$HOMEBREW_BOTTLE_DOMAIN" = "https://ghcr.io/v2/homebrew/core" ]; then
  sleep 30
  printf leaked > "$root/leaked"
fi"#,
        );
        let limits = FetchLimits {
            per_route: Duration::from_secs(1),
            total: Duration::from_secs(3),
        };
        let started = tokio::time::Instant::now();
        install_with_limits(&brew, limits).await.unwrap();
        assert!(started.elapsed() > limits.total);
        assert!(temp.0.join("applied").exists());
        assert!(!temp.0.join("leaked").exists());
        let attempts = std::fs::read_to_string(temp.0.join("attempts")).unwrap();
        assert_eq!(attempts.lines().count(), 3);
        assert!(attempts.contains(&format!("install|{}", ROUTES[1].1)));
    }

    #[tokio::test]
    async fn shared_budget_stops_stalled_routes_before_installation() {
        let (temp, brew) = fake_brew("sleep 30");
        let limits = FetchLimits {
            per_route: Duration::from_secs(1),
            total: Duration::from_millis(1500),
        };
        let started = tokio::time::Instant::now();
        let error = install_with_limits(&brew, limits).await.unwrap_err();
        assert!(error.contains("shared download budget"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let attempts = std::fs::read_to_string(temp.0.join("attempts")).unwrap();
        assert!((1..=2).contains(&attempts.lines().count()), "{attempts}");
        assert!(!attempts.contains("install|"));
    }

    #[test]
    fn managed_download_policy_is_preserved_and_credentials_are_not_reported() {
        let temp = TemporaryDirectory::create(&std::env::temp_dir()).unwrap();
        let config = temp.0.join("brew.env");
        std::fs::write(
            &config,
            "# untouched local metadata policy\nHOMEBREW_API_DOMAIN=https://metadata.example/api\nHOMEBREW_CURL_RETRIES=0\nHOMEBREW_NO_AUTO_UPDATE=1\n",
        )
        .unwrap();
        validate_environment_files(std::slice::from_ref(&config)).unwrap();
        let secret = "fixture-registry-secret";
        let contents = format!("HOMEBREW_DOCKER_REGISTRY_TOKEN={secret}\n");
        std::fs::write(&config, &contents).unwrap();
        let error = validate_environment_files(std::slice::from_ref(&config)).unwrap_err();
        assert!(error.contains("HOMEBREW_DOCKER_REGISTRY_TOKEN"));
        assert!(error.contains(config.to_str().unwrap()));
        assert!(!error.contains(secret));
        assert_eq!(std::fs::read_to_string(config).unwrap(), contents);
    }
}

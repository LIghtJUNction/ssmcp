//! First-run SSH discovery and explicit-consent dependency installation.
//! Downloads and package application are separate phases. Never terminate a
//! running OS package transaction merely because a network deadline expired.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "linux")]
mod linux_apt;
#[cfg(any(target_os = "macos", all(test, unix)))]
mod macos_brew;

pub(super) struct SshPrograms {
    pub ssh: PathBuf,
    pub keygen: PathBuf,
}

fn programs_in(directory: &Path) -> Option<SshPrograms> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let ssh = directory.join(format!("ssh{suffix}"));
    let keygen = directory.join(format!("ssh-keygen{suffix}"));
    if executable_file(&ssh) && executable_file(&keygen) {
        Some(SshPrograms { ssh, keygen })
    } else {
        None
    }
}
fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

pub(super) fn discover() -> Result<SshPrograms, String> {
    let mut directories = Vec::new();
    for name in ["ssh", "ssh-keygen"] {
        if let Ok(path) = crate::program::executable(name)
            && let Some(parent) = path.parent()
        {
            directories.push(parent.to_path_buf());
        }
    }
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("SystemRoot") {
            directories.push(PathBuf::from(&root).join("System32/OpenSSH"));
            directories.push(PathBuf::from(root).join("Sysnative/OpenSSH"));
        }
        if let Some(path) = managed_windows_directory() {
            directories.push(path);
        }
    }
    #[cfg(unix)]
    for path in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        directories.push(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    {
        // Keg-only formulae are not linked into Homebrew's normal bin directory.
        directories.extend([
            PathBuf::from("/opt/homebrew/opt/openssh/bin"),
            PathBuf::from("/usr/local/opt/openssh/bin"),
        ]);
        if let Ok(brew) = crate::program::executable("brew")
            && let Some(prefix) = brew.parent().and_then(Path::parent)
        {
            directories.push(prefix.join("opt/openssh/bin"));
        }
    }
    for directory in directories {
        if let Some(programs) = programs_in(&directory) {
            return Ok(programs);
        }
    }
    Err("OpenSSH client tools ssh and ssh-keygen were not found together; first-run setup can install them after you approve".into())
}

pub(super) fn ssh_executable() -> OsString {
    // Normal connections must preserve the user's PATH-selected SSH wrapper,
    // even when key generation tools are absent. Setup still requires a pair.
    crate::program::executable("ssh")
        .map(PathBuf::into_os_string)
        .or_else(|_| discover().map(|programs| programs.ssh.into_os_string()))
        .unwrap_or_else(|_| OsString::from("ssh"))
}

/// The caller must obtain an affirmative installation choice before this call.
pub(super) async fn install() -> Result<SshPrograms, String> {
    if let Ok(programs) = discover() {
        return Ok(programs);
    }
    #[cfg(target_os = "linux")]
    {
        install_linux().await?;
        discover()
    }
    #[cfg(target_os = "macos")]
    {
        install_macos().await?;
        discover()
    }
    #[cfg(windows)]
    {
        install_windows().await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err("automatic OpenSSH installation is unavailable on this operating system".into())
    }
}

struct TemporaryDirectory(PathBuf);
impl TemporaryDirectory {
    fn create(parent: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        for _ in 0..8 {
            let mut random = [0_u8; 12];
            getrandom::fill(&mut random).map_err(|error| error.to_string())?;
            let name = random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let path = parent.join(format!(".ssmcp-setup-{name}"));
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let mut builder = builder;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("could not create a private setup directory".into())
    }
}
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run_program(
    program: &Path,
    arguments: &[OsString],
    environment: &[(&str, OsString)],
    download_deadline: Option<Duration>,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .envs(environment.iter().map(|(k, v)| (*k, v)))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    // Fetch-only/extraction work owns every descendant. Package application is
    // deliberately spawned separately without a deadline or kill-on-drop.
    let status = if let Some(deadline) = download_deadline {
        let mut process = crate::backend::process::Managed::spawn(&mut command)
            .map_err(|error| format!("could not start {}: {error}", program.display()))?;
        match tokio::time::timeout(deadline, process.wait()).await {
            Ok(status) => status.map_err(|error| error.to_string())?,
            Err(_) => {
                process.shutdown().await;
                return Err("dependency download/extraction deadline exceeded; owned helper processes were stopped and no package installation transaction was interrupted".into());
            }
        }
    } else {
        command.kill_on_drop(false);
        command
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", program.display()))?
            .wait()
            .await
            .map_err(|error| error.to_string())?
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {status}", program.display()))
    }
}

#[cfg(target_os = "linux")]
async fn elevated(program: &Path, arguments: &[&str], download_only: bool) -> Result<(), String> {
    let mut args: Vec<OsString> = Vec::new();
    let selected = if download_only {
        // timeout itself runs under sudo, so it can stop its privileged fetch
        // child. This wrapper is never used around the apply/install phase.
        let timeout = crate::program::executable("timeout")
            .map_err(|_| "coreutils timeout is required to bound package downloads")?;
        args.extend(
            ["--signal=TERM", "--kill-after=5s", "150s"]
                .into_iter()
                .map(OsString::from),
        );
        args.push(program.as_os_str().to_owned());
        timeout
    } else {
        program.to_path_buf()
    };
    args.extend(arguments.iter().map(OsString::from));
    if unsafe { libc::geteuid() } == 0 {
        run_program(&selected, &args, &[], None).await
    } else {
        let sudo = crate::program::executable("sudo")
            .map_err(|_| "installing the missing system package requires root or sudo")?;
        let mut sudo_args = vec![OsString::from("--"), selected.into_os_string()];
        sudo_args.extend(args);
        run_program(&sudo, &sudo_args, &[], None).await
    }
}

#[cfg(any(target_os = "linux", test))]
fn pacman_configuration(original: &str, transfer: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut found = false;
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("XferCommand")
            && trimmed
                .split('=')
                .next()
                .is_some_and(|s| s.trim() == "XferCommand")
        {
            continue;
        }
        if trimmed == "DisableDownloadTimeout" {
            continue;
        }
        result.push_str(line);
        result.push('\n');
        if trimmed == "[options]" && !found {
            result.push_str("XferCommand = ");
            result.push_str(transfer);
            result.push('\n');
            found = true;
        }
    }
    if !found {
        return Err(
            "pacman.conf has no [options] section; no system configuration was changed".into(),
        );
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
async fn install_linux() -> Result<(), String> {
    if let Ok(apt) = crate::program::executable("apt-get") {
        return linux_apt::install(&apt).await;
    }
    if let Ok(pacman) = crate::program::executable("pacman") {
        let temp = TemporaryDirectory::create(&std::env::temp_dir())?;
        let original = std::fs::read_to_string("/etc/pacman.conf")
            .map_err(|error| format!("cannot inspect pacman configuration: {error}"))?;
        let curl = crate::program::executable("curl")
            .map_err(|_| "curl is required for bounded pacman downloads")?;
        let curl = curl.to_str().ok_or("curl path is not valid UTF-8")?;
        if !curl
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
        {
            return Err(
                "bounded package download requires curl at a standard path without shell syntax"
                    .into(),
            );
        }
        let fetch_config = temp.0.join("fetch.conf");
        let offline_config = temp.0.join("offline.conf");
        let fetch = format!(
            "{curl} --disable --fail --location --connect-timeout 8 --speed-time 12 --speed-limit 65536 --max-time 120 --retry 0 --output %o %u"
        );
        std::fs::write(&fetch_config, pacman_configuration(&original, &fetch)?)
            .map_err(|e| e.to_string())?;
        std::fs::write(
            &offline_config,
            pacman_configuration(&original, "/usr/bin/false")?,
        )
        .map_err(|e| e.to_string())?;
        let query = crate::program::executable("pacman-conf")
            .map_err(|_| "pacman-conf is required to verify the offline install configuration")?;
        for (config, wanted) in [
            (&fetch_config, fetch.as_str()),
            (&offline_config, "/usr/bin/false"),
        ] {
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::process::Command::new(&query)
                    .arg("--config")
                    .arg(config)
                    .arg("XferCommand")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| "pacman configuration probe timed out")?
            .map_err(|e| e.to_string())?;
            if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != wanted
            {
                return Err("included pacman configuration overrode the bounded/offline transfer policy; no packages were installed".into());
            }
        }
        eprintln!(
            "[ssmcp setup] Fetching OpenSSH using the configured signed pacman repositories and mirror fallback."
        );
        elevated(
            &pacman,
            &[
                "--config",
                fetch_config.to_str().ok_or("invalid temporary path")?,
                "-Sw",
                "--noconfirm",
                "--needed",
                "openssh",
            ],
            true,
        )
        .await?;
        return elevated(
            &pacman,
            &[
                "--config",
                offline_config.to_str().ok_or("invalid temporary path")?,
                "-S",
                "--noconfirm",
                "--needed",
                "openssh",
            ],
            false,
        )
        .await;
    }
    if let Ok(dnf) = crate::program::executable("dnf") {
        eprintln!(
            "[ssmcp setup] Fetching the signed OpenSSH client package with dnf; then installing from cache."
        );
        elevated(
            &dnf,
            &[
                "--setopt=timeout=10",
                "--setopt=minrate=65536",
                "--setopt=retries=0",
                "--assumeyes",
                "--downloadonly",
                "install",
                "openssh-clients",
            ],
            true,
        )
        .await?;
        return elevated(
            &dnf,
            &["--assumeyes", "--cacheonly", "install", "openssh-clients"],
            false,
        )
        .await;
    }
    Err("no supported native package manager was found; install the OpenSSH client package with this distribution's package manager, then rerun ssmcp".into())
}

#[cfg(target_os = "macos")]
async fn install_macos() -> Result<(), String> {
    // Supported macOS installations include /usr/bin/ssh and ssh-keygen. The
    // caller reaches this path only when those system tools are missing.
    let brew=crate::program::executable("brew").or_else(|_|{
        ["/opt/homebrew/bin/brew","/usr/local/bin/brew"].into_iter().map(PathBuf::from)
            .find(|p|executable_file(p)).ok_or_else(||"Homebrew is unavailable".to_owned())
    }).map_err(|_|"macOS normally supplies /usr/bin/ssh and ssh-keygen; they are missing, and Homebrew is not installed. Restore the system tools or install OpenSSH with an approved package manager")?;
    macos_brew::install(&brew).await
}

#[cfg(windows)]
fn windows_base() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("ssmcp/openssh"))
        .ok_or_else(|| {
            "LOCALAPPDATA is unavailable; cannot choose a per-user OpenSSH installation directory"
                .into()
        })
}
#[cfg(windows)]
fn managed_windows_directory() -> Option<PathBuf> {
    let base = windows_base().ok()?;
    let marker = base.join("current.json");
    if std::fs::metadata(&marker).ok()?.len() > 4096 {
        return None;
    }
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(marker).ok()?).ok()?;
    let digest = value["sha256"].as_str()?;
    let layout = value["layout"].as_str()?;
    if !valid_digest(digest)
        || !["OpenSSH-Win64", "OpenSSH-Win32", "OpenSSH-ARM64"].contains(&layout)
    {
        return None;
    }
    Some(base.join("packages").join(digest).join(layout))
}
#[cfg(any(windows, test))]
fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(windows)]
async fn install_windows() -> Result<SshPrograms, String> {
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
        digest: Option<String>,
        size: u64,
    }
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        draft: bool,
        assets: Vec<Asset>,
    }
    let name = match std::env::consts::ARCH {
        "x86_64" => "OpenSSH-Win64.zip",
        "aarch64" => "OpenSSH-ARM64.zip",
        "x86" => "OpenSSH-Win32.zip",
        _ => {
            return Err(
                "no official OpenSSH archive exists for this Windows CPU architecture".into(),
            );
        }
    };
    let bytes = download::official_metadata(
        "https://api.github.com/repos/PowerShell/Win32-OpenSSH/releases/latest",
    )
    .await?;
    let release: Release = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid official OpenSSH release metadata: {error}"))?;
    if release.draft
        || release.tag_name.len() > 96
        || !release
            .tag_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("official OpenSSH release metadata has an invalid tag".into());
    }
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == name)
        .ok_or("official release has no archive for this Windows architecture")?;
    if asset.size == 0 || asset.size > 64 * 1024 * 1024 {
        return Err("official OpenSSH archive size is outside the allowed limit".into());
    }
    let digest=asset.digest.as_deref().and_then(|s|s.strip_prefix("sha256:"))
        .filter(|s|valid_digest(s)).ok_or("official GitHub metadata has no SHA-256 asset digest; unverified mirror downloads will not be installed")?.to_ascii_lowercase();
    download::validate_official_asset_url(&asset.browser_download_url, name)?;
    eprintln!(
        "[ssmcp setup] Installing Microsoft Win32-OpenSSH {} as a per-user portable client. Windows servicing and SSH server services are not changed.",
        release.tag_name
    );
    let archive =
        download::verified_public_asset(&asset.browser_download_url, &digest, asset.size).await?;
    let base = windows_base()?;
    let packages = base.join("packages");
    let temp = TemporaryDirectory::create(&packages)?;
    let zip = temp.0.join("openssh.zip");
    let unpacked = temp.0.join("unpacked");
    std::fs::write(&zip, &archive).map_err(|e| e.to_string())?;
    let powershell = crate::program::executable("pwsh")
        .or_else(|_| crate::program::executable("powershell"))
        .or_else(|_| {
            std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .map(|p| p.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .filter(|p| executable_file(p))
                .ok_or_else(|| "Windows PowerShell is unavailable".to_owned())
        })?;
    const EXTRACT: &str = r#"$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.IO.Compression.FileSystem
Add-Type -AssemblyName System.IO.Compression
if(-not [IO.Compression.ZipArchiveEntry].GetProperty('ExternalAttributes')){throw 'Verified ZIP extraction requires .NET Framework 4.7.2 or later. Update Windows/.NET, or use PowerShell 7.'}
$z=[IO.Compression.ZipFile]::OpenRead($env:SSMCP_SETUP_ARCHIVE)
try {
  if($z.Entries.Count -gt 5000){throw 'Archive contains too many entries'}
  $seen=New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
  [long]$total=0
  foreach($e in $z.Entries){
    $n=$e.FullName.Replace('\','/')
    if([IO.Path]::IsPathRooted($n) -or $n -match '[<>:"|?*\x00-\x1f]'){throw 'Unsafe archive path'}
    $segments=$n.TrimEnd('/').Split('/')
    foreach($segment in $segments){
      if(-not $segment -or $segment -eq '.' -or $segment -eq '..' -or $segment -ne $segment.Trim() -or $segment.EndsWith('.') -or $segment.EndsWith(' ') -or $segment -match '^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)'){throw 'Unsafe normalized archive path'}
    }
    $normalized=$segments -join '/'
    if(-not $seen.Add($normalized)){throw 'Duplicate archive path'}
    $root=[IO.Path]::GetFullPath($env:SSMCP_SETUP_DESTINATION).TrimEnd([IO.Path]::DirectorySeparatorChar)+[IO.Path]::DirectorySeparatorChar
    $destination=[IO.Path]::GetFullPath([IO.Path]::Combine($root,$normalized))
    if(-not $destination.StartsWith($root,[StringComparison]::OrdinalIgnoreCase)){throw 'Archive path escapes destination'}
    if((($e.ExternalAttributes -shr 16) -band 61440) -eq 40960 -or ($e.ExternalAttributes -band 1024) -ne 0){throw 'Archive links are not accepted'}
    $total+=$e.Length
    if($total -gt 536870912){throw 'Archive expands beyond its limit'}
  }
} finally {$z.Dispose()}
Expand-Archive -LiteralPath $env:SSMCP_SETUP_ARCHIVE -DestinationPath $env:SSMCP_SETUP_DESTINATION
"#;
    let args = [
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        EXTRACT,
    ]
    .map(OsString::from);
    run_program(
        &powershell,
        &args,
        &[
            ("SSMCP_SETUP_ARCHIVE", zip.into_os_string()),
            ("SSMCP_SETUP_DESTINATION", unpacked.as_os_str().to_owned()),
        ],
        Some(Duration::from_secs(90)),
    )
    .await?;
    let layout = name.strip_suffix(".zip").ok_or("invalid archive name")?;
    let installed = programs_in(&unpacked.join(layout))
        .ok_or("verified archive did not contain ssh.exe and ssh-keygen.exe")?;
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(&installed.ssh)
            .arg("-V")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "installed OpenSSH version probe timed out")?
    .map_err(|e| e.to_string())?;
    let version = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || !version.contains("OpenSSH") {
        return Err("verified OpenSSH client could not run on this Windows installation".into());
    }
    let final_directory = packages.join(&digest);
    if final_directory.exists() {
        let prior = programs_in(&final_directory.join(layout))
            .ok_or("existing managed OpenSSH version is incomplete; it was not overwritten")?;
        for (old, new) in [
            (&prior.ssh, &installed.ssh),
            (&prior.keygen, &installed.keygen),
        ] {
            if std::fs::read(old).map_err(|e| e.to_string())?
                != std::fs::read(new).map_err(|e| e.to_string())?
            {
                return Err("existing managed OpenSSH version differs from the verified archive; it was not overwritten".into());
            }
        }
    } else {
        std::fs::rename(&unpacked, &final_directory)
            .map_err(|e| format!("could not activate the verified OpenSSH directory: {e}"))?;
    }
    let marker = temp.0.join("current.json");
    std::fs::write(
        &marker,
        serde_json::json!({
            "sha256":digest,"layout":layout,"version":release.tag_name,
            "source":"https://api.github.com/repos/PowerShell/Win32-OpenSSH/releases/latest"
        })
        .to_string(),
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(marker, base.join("current.json"))
        .map_err(|e| format!("could not activate OpenSSH tools: {e}"))?;
    programs_in(&final_directory.join(layout))
        .ok_or_else(|| "activated OpenSSH tools are unavailable".into())
}

#[cfg(any(windows, test))]
mod download {
    use super::valid_digest;
    use sha2::{Digest, Sha256};
    use std::time::Duration;
    use tokio::time::Instant;

    #[derive(Clone, Copy)]
    struct Limits {
        connect: Duration,
        first_byte: Duration,
        idle: Duration,
        total: Duration,
        rate_window: Duration,
        min_bytes_per_second: u64,
        max_bytes: u64,
    }
    impl Limits {
        fn archive(max_bytes: u64) -> Self {
            Self {
                connect: Duration::from_secs(8),
                first_byte: Duration::from_secs(12),
                idle: Duration::from_secs(10),
                total: Duration::from_secs(180),
                rate_window: Duration::from_secs(8),
                min_bytes_per_second: 65536,
                max_bytes,
            }
        }
    }
    fn client(limits: Limits) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(limits.connect)
            .timeout(limits.total)
            .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
                    attempt.error("download redirect limit or HTTPS policy violation")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| e.to_string())
    }
    async fn fetch(client: &reqwest::Client, url: &str, limits: Limits) -> Result<Vec<u8>, String> {
        let begin = Instant::now();
        let overall = begin + limits.total;
        let first = begin + limits.first_byte;
        let mut response = tokio::time::timeout_at(first.min(overall), client.get(url).send())
            .await
            .map_err(|_| "download first-byte deadline exceeded")?
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        if response
            .content_length()
            .is_some_and(|n| n > limits.max_bytes)
        {
            return Err("download exceeds its declared size budget".into());
        }
        let mut body = Vec::new();
        let mut window_start = Instant::now();
        let mut window_bytes = 0_u64;
        let mut last_print = Instant::now();
        loop {
            let now = Instant::now();
            if now >= overall {
                return Err("download total deadline exceeded".into());
            }
            let deadline = if body.is_empty() {
                first
            } else {
                now + limits.idle
            }
            .min(overall);
            let chunk = tokio::time::timeout_at(deadline, response.chunk())
                .await
                .map_err(|_| "download stalled before the next bytes arrived")?
                .map_err(|e| e.to_string())?;
            let Some(chunk) = chunk else {
                return Ok(body);
            };
            if body.len() as u64 + chunk.len() as u64 > limits.max_bytes {
                return Err("download exceeded its size budget".into());
            }
            window_bytes += chunk.len() as u64;
            body.extend_from_slice(&chunk);
            let elapsed = window_start.elapsed();
            if elapsed >= limits.rate_window {
                if window_bytes as f64 / elapsed.as_secs_f64() < limits.min_bytes_per_second as f64
                {
                    return Err(format!(
                        "download stayed below {} KiB/s; trying another public source",
                        limits.min_bytes_per_second / 1024
                    ));
                }
                window_start = Instant::now();
                window_bytes = 0;
            }
            if last_print.elapsed() >= Duration::from_secs(1) {
                eprintln!("[ssmcp setup] Downloaded {} KiB", body.len() / 1024);
                last_print = Instant::now();
            }
        }
    }

    pub(super) fn validate_official_asset_url(url: &str, name: &str) -> Result<(), String> {
        let parsed = reqwest::Url::parse(url).map_err(|_| "invalid public artifact URL")?;
        if parsed.scheme() != "https"
            || parsed.host_str() != Some("github.com")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed
                .path()
                .starts_with("/PowerShell/Win32-OpenSSH/releases/download/")
            || parsed.path_segments().and_then(|mut s| s.next_back()) != Some(name)
        {
            return Err(
                "OpenSSH asset URL is not an unauthenticated public URL in the official repository"
                    .into(),
            );
        }
        Ok(())
    }
    #[cfg(windows)]
    pub(super) async fn official_metadata(url: &str) -> Result<Vec<u8>, String> {
        if url != "https://api.github.com/repos/PowerShell/Win32-OpenSSH/releases/latest" {
            return Err("unrecognized dependency metadata origin".into());
        }
        let mut limits = Limits::archive(1024 * 1024);
        limits.total = Duration::from_secs(30);
        limits.min_bytes_per_second = 1024;
        fetch(&client(limits)?, url, limits).await
    }
    #[cfg(windows)]
    pub(super) async fn verified_public_asset(
        url: &str,
        digest: &str,
        size: u64,
    ) -> Result<Vec<u8>, String> {
        if !valid_digest(digest) {
            return Err("missing trusted SHA-256 digest".into());
        }
        let limits = Limits::archive(size);
        let sources = [
            url.to_owned(),
            format!("https://ghfast.top/{url}"),
            format!("https://gh-proxy.com/{url}"),
        ];
        verified_from_sources(&client(limits)?, &sources, digest, limits).await
    }
    async fn verified_from_sources(
        client: &reqwest::Client,
        sources: &[String],
        digest: &str,
        limits: Limits,
    ) -> Result<Vec<u8>, String> {
        if !valid_digest(digest) {
            return Err("invalid trusted SHA-256 digest".into());
        }
        let mut failures = Vec::new();
        for source in sources {
            eprintln!("[ssmcp setup] Download source: {source}");
            match fetch(client, source, limits).await {
                Ok(bytes)
                    if format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(digest) =>
                {
                    return Ok(bytes);
                }
                Ok(_) => failures.push("checksum mismatch; rejected source".to_owned()),
                Err(error) => failures.push(error),
            }
            eprintln!(
                "[ssmcp setup] {}. Trying the next source.",
                failures.last().unwrap()
            );
        }
        Err(format!(
            "all public download routes failed verification: {}",
            failures.join("; ")
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn limits() -> Limits {
            Limits {
                connect: Duration::from_millis(50),
                first_byte: Duration::from_millis(80),
                idle: Duration::from_millis(60),
                total: Duration::from_millis(400),
                rate_window: Duration::from_millis(50),
                min_bytes_per_second: 1024,
                max_bytes: 4096,
            }
        }
        async fn server(
            body: Vec<u8>,
            first_delay: Duration,
            byte_delay: Duration,
        ) -> (String, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _ = socket.set_nodelay(true);
                let mut request = [0_u8; 2048];
                let _ = socket.read(&mut request).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                tokio::time::sleep(first_delay).await;
                if byte_delay.is_zero() {
                    let _ = socket.write_all(&body).await;
                } else {
                    for byte in body {
                        if socket.write_all(&[byte]).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(byte_delay).await;
                    }
                }
            });
            (format!("http://{address}/public-fixture"), handle)
        }
        fn local_client() -> reqwest::Client {
            reqwest::Client::builder().no_proxy().build().unwrap()
        }
        #[tokio::test]
        async fn stalled_source_falls_back_and_hash_tampering_is_rejected() {
            let payload = b"verified public archive".to_vec();
            let digest = format!("{:x}", Sha256::digest(&payload));
            let (slow, slow_server) =
                server(payload.clone(), Duration::from_millis(200), Duration::ZERO).await;
            let (bad, bad_server) =
                server(b"tampered".to_vec(), Duration::ZERO, Duration::ZERO).await;
            let (good, good_server) = server(payload.clone(), Duration::ZERO, Duration::ZERO).await;
            let received =
                verified_from_sources(&local_client(), &[slow, bad, good], &digest, limits())
                    .await
                    .unwrap();
            assert_eq!(received, payload);
            slow_server.abort();
            bad_server.await.unwrap();
            good_server.await.unwrap();
        }
        #[tokio::test]
        async fn sustained_low_rate_is_aborted_without_waiting_for_total_timeout() {
            let (url, task) = server(vec![1; 100], Duration::ZERO, Duration::from_millis(10)).await;
            let start = Instant::now();
            let error = fetch(&local_client(), &url, limits()).await.unwrap_err();
            assert!(error.contains("below"), "{error}");
            assert!(start.elapsed() < limits().total);
            task.abort();
        }
        #[tokio::test]
        async fn declared_size_limit_and_invalid_digest_fail_closed() {
            let (url, task) = server(vec![1; 5000], Duration::ZERO, Duration::ZERO).await;
            assert!(
                fetch(&local_client(), &url, limits())
                    .await
                    .unwrap_err()
                    .contains("size")
            );
            task.abort();
            assert!(
                verified_from_sources(&local_client(), &[], &"x".repeat(64), limits())
                    .await
                    .unwrap_err()
                    .contains("digest")
            );
        }
        #[test]
        fn mirror_inputs_are_public_official_artifacts_only() {
            let root = "https://github.com/PowerShell/Win32-OpenSSH/releases/download/v1/OpenSSH-Win64.zip";
            assert!(validate_official_asset_url(root, "OpenSSH-Win64.zip").is_ok());
            for invalid in [
                root.replace("https:", "http:"),
                root.replace("github.com", "evil.example"),
                format!("{root}?token=secret"),
                root.replace("https://", "https://user:secret@"),
                root.replace("PowerShell/Win32-OpenSSH", "another/repository"),
            ] {
                assert!(validate_official_asset_url(&invalid, "OpenSSH-Win64.zip").is_err());
            }
            assert!(client(Limits::archive(4096)).is_ok());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_deadline_and_cancellation_stop_spawned_helpers() {
        for cancel in [false, true] {
            let owned = TemporaryDirectory::create(&std::env::temp_dir()).unwrap();
            let pid_file = owned.0.join("helper.pid");
            let marker = owned.0.join("helper-survived");
            // The child would create a marker after its parent is stopped.
            let script = "(sleep 1; printf leaked > \"$2\") & printf %s $! > \"$1\"; wait";
            let args = vec![
                OsString::from("-c"),
                OsString::from(script),
                OsString::from("fetch-fixture"),
                pid_file.clone().into_os_string(),
                marker.clone().into_os_string(),
            ];
            let task = tokio::spawn(async move {
                run_program(
                    Path::new("/bin/sh"),
                    &args,
                    &[],
                    Some(Duration::from_millis(if cancel { 5000 } else { 150 })),
                )
                .await
            });
            let started = tokio::time::Instant::now();
            while !pid_file.exists() {
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "fixture never started"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert!(task.await.unwrap().unwrap_err().contains("deadline"));
            }
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(!marker.exists(), "a helper outlived fetch cleanup");
        }
    }
    #[test]
    fn pacman_policy_changes_only_temporary_transfer_configuration() {
        let original = "[options]\nArchitecture = auto\nXferCommand = /old/downloader %u\nDisableDownloadTimeout\nSigLevel = Required DatabaseOptional\n[core]\nInclude = /etc/pacman.d/mirrorlist\n";
        let rewritten = pacman_configuration(original, "/usr/bin/false").unwrap();
        assert_eq!(rewritten.matches("XferCommand").count(), 1);
        assert!(rewritten.contains("XferCommand = /usr/bin/false"));
        assert!(!rewritten.contains("DisableDownloadTimeout"));
        assert!(rewritten.contains("SigLevel = Required DatabaseOptional"));
        assert!(rewritten.contains("Include = /etc/pacman.d/mirrorlist"));
        assert!(pacman_configuration("[core]\nServer = example\n", "false").is_err());
    }
    #[test]
    fn temporary_setup_directory_is_owned_and_cleaned() {
        let parent = std::env::temp_dir();
        let path;
        {
            let owned = TemporaryDirectory::create(&parent).unwrap();
            path = owned.0.clone();
            std::fs::write(path.join("owned"), b"public artifact").unwrap();
        }
        assert!(!path.exists());
    }
}

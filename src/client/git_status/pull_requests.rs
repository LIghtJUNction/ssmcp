//! Detect newly advertised numbered refs without downloading their objects.
use super::*;
use sha2::{Digest, Sha256};

const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);
const ENDPOINT_LIMIT: usize = 16 * 1024;

pub(super) fn remote_command(git: &Path, root: &Path) -> Result<Command, Failure> {
    let mut command = base_command(git, root)?;
    command
        .args([
            "-c",
            "credential.interactive=false",
            "-c",
            "core.askPass=",
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
            "-c",
            "core.sshCommand=ssh -oBatchMode=yes -oConnectTimeout=10 -oStrictHostKeyChecking=yes",
        ])
        .env("GIT_ALLOW_PROTOCOL", "https:http:ssh:file")
        .env("GIT_ASKPASS", "")
        .env("GIT_SSH_VARIANT", "ssh")
        .env("GCM_INTERACTIVE", "Never");
    Ok(command)
}

async fn capture(
    mut command: Command,
    timeout: Duration,
    limit: usize,
    stopped: &mut watch::Receiver<bool>,
) -> Result<Vec<u8>, Failure> {
    if *stopped.borrow() {
        return Err(Failure::Cancelled);
    }
    let mut process = Managed::spawn(&mut command)
        .map_err(|_| Failure::Unavailable("Git PR observation could not start"))?;
    let stdout = process.child.stdout.take().expect("piped Git stdout");
    let stderr = process.child.stderr.take().expect("piped Git stderr");
    let result = {
        let complete = async {
            let (stdout, _, exit) = tokio::try_join!(
                limited(stdout, limit),
                limited(stderr, ERROR_LIMIT),
                async {
                    process
                        .wait()
                        .await
                        .map_err(|_| Failure::Error("Git PR observation could not be reaped"))
                }
            )?;
            if !exit.success() {
                // Native output may include remote URLs and credential details.
                return Err(Failure::Error("Git PR observation failed"));
            }
            Ok(stdout)
        };
        tokio::select! {
            biased;
            _ = stopped.changed() => Err(Failure::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(Failure::Error("Git PR observation timed out")),
            result = complete => result,
        }
    };
    process.shutdown().await;
    result
}

async fn endpoint_digest(
    git: &Path,
    root: &Path,
    remote: &str,
    stopped: &mut watch::Receiver<bool>,
) -> Result<[u8; 32], Failure> {
    let mut command = base_command(git, root)?;
    command.args(["remote", "get-url", "--", remote]);
    let bytes = capture(command, TIMEOUT, ENDPOINT_LIMIT, stopped).await?;
    let endpoint = bytes
        .strip_suffix(b"\n")
        .filter(|url| !url.is_empty() && !url.iter().any(u8::is_ascii_control))
        .ok_or(Failure::Error("Git remote endpoint is unavailable"))?;
    Ok(Sha256::digest(endpoint).into())
}

#[derive(Default)]
pub(super) struct PullRequests {
    // A renamed, replaced, or newly selected remote gets its own silent baseline.
    source: Option<(PathBuf, String, [u8; 32])>,
    highest: u64,
}

impl PullRequests {
    pub(super) async fn observe(
        &mut self,
        git: &Path,
        root: &Path,
        remote: &str,
        stopped: &mut watch::Receiver<bool>,
    ) -> Result<Vec<u64>, Failure> {
        if remote.is_empty() || remote.starts_with('-') || remote.chars().any(char::is_control) {
            return Err(Failure::Error("Git remote name is invalid"));
        }
        let endpoint = endpoint_digest(git, root, remote, stopped).await?;
        let source = (root.to_path_buf(), remote.to_owned(), endpoint);
        let same_source = self.source.as_ref() == Some(&source);
        let mut command = remote_command(git, root)?;
        command.args([
            "ls-remote",
            "--refs",
            "--upload-pack=git-upload-pack",
            "--",
            remote,
            "refs/pull/*/head",
        ]);
        let refs = capture(command, REMOTE_TIMEOUT, OUTPUT_LIMIT, stopped).await?;
        let mut numbers = new_numbers(&refs, if same_source { self.highest } else { 0 })?;
        // Changing a remote in the middle of a query invalidates that observation.
        if endpoint != endpoint_digest(git, root, remote, stopped).await? {
            return Err(Failure::Error("Git remote changed during PR observation"));
        }
        if !same_source {
            self.source = Some(source);
            self.highest = numbers.last().copied().unwrap_or(0);
            numbers.clear();
        } else if let Some(highest) = numbers.last() {
            self.highest = *highest;
        }
        Ok(numbers)
    }
}

fn new_numbers(bytes: &[u8], after: u64) -> Result<Vec<u64>, Failure> {
    let invalid = || Failure::Error("Git PR ref listing was incomplete or invalid");
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(invalid());
    }
    let mut numbers = Vec::new();
    for record in bytes.split_inclusive(|&b| b == b'\n') {
        let line = &record[..record.len() - 1];
        let tab = line.iter().position(|&b| b == b'\t').ok_or_else(invalid)?;
        if !valid_oid(&line[..tab]) {
            return Err(invalid());
        }
        // ls-remote patterns can also match a suffix of an unrelated branch.
        let Some(id) = line[tab + 1..]
            .strip_prefix(b"refs/pull/")
            .and_then(|name| name.strip_suffix(b"/head"))
            .filter(|id| !id.is_empty() && id.iter().all(u8::is_ascii_digit))
        else {
            continue;
        };
        if id.first() != Some(&b'0')
            && let Some(number) = std::str::from_utf8(id)
                .ok()
                .and_then(|id| id.parse::<u64>().ok())
            && number > after
            && number <= crate::wire::repository_events::MAX_NUMBER
        {
            numbers.push(number);
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    Ok(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(executable: &Path, cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new(executable)
            .current_dir(cwd)
            .args([
                "-c",
                "user.name=ssmcp test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", cwd.join("no-global-config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[tokio::test]
    async fn observes_only_new_numbered_remote_refs_and_downloads_no_objects() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let tmp = Fixture(std::env::temp_dir().join(format!(
            "ssmcp-pr-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )));
        let root = &tmp.0;
        std::fs::create_dir(root).unwrap();
        let executable = crate::program::executable("git").unwrap();
        let bare = root.join("origin.git");
        let reader = root.join("reader");
        git(&executable, root, &["init", "--bare", "origin.git"]);
        git(&executable, root, &["init", "writer"]);
        git(&executable, root, &["init", "reader"]);
        let writer = root.join("writer");
        git(
            &executable,
            &writer,
            &["commit", "--allow-empty", "-qm", "initial"],
        );
        git(
            &executable,
            &writer,
            &["push", "-q", "../origin.git", "HEAD:refs/pull/1/head"],
        );
        git(
            &executable,
            &reader,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        let before = git(&executable, &reader, &["count-objects", "-v"]);
        let (_stop, mut stopped) = watch::channel(false);
        let mut prs = PullRequests::default();
        assert!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap()
                .is_empty()
        );
        git(
            &executable,
            &writer,
            &[
                "push",
                "-q",
                "../origin.git",
                "HEAD:refs/pull/9/head",
                "HEAD:refs/pull/3/head",
                "HEAD:refs/heads/refs/pull/42/head",
            ],
        );
        assert!(
            prs.observe(&executable, &reader, "--help", &mut stopped)
                .await
                .is_err()
        );
        assert_eq!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap(),
            [3, 9]
        );
        git(
            &executable,
            &writer,
            &["commit", "--allow-empty", "-qm", "amend"],
        );
        git(
            &executable,
            &writer,
            &[
                "push",
                "-q",
                "../origin.git",
                "HEAD:refs/pull/9/head",
                ":refs/pull/3/head",
            ],
        );
        assert!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap()
                .is_empty()
        );
        git(
            &executable,
            &writer,
            &["push", "-q", "../origin.git", "HEAD:refs/pull/3/head"],
        );
        assert!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap()
                .is_empty()
        );

        // A transport failure on the same endpoint preserves its last good watermark.
        let offline = root.join("offline.git");
        std::fs::rename(&bare, &offline).unwrap();
        assert!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .is_err()
        );
        std::fs::rename(offline, &bare).unwrap();
        git(
            &executable,
            &writer,
            &["push", "-q", "../origin.git", "HEAD:refs/pull/10/head"],
        );
        assert_eq!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap(),
            [10]
        );

        git(&executable, root, &["init", "--bare", "other.git"]);
        git(
            &executable,
            &writer,
            &["push", "-q", "../other.git", "HEAD:refs/pull/27/head"],
        );
        git(
            &executable,
            &reader,
            &["remote", "set-url", "origin", "../other.git"],
        );
        assert!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap()
                .is_empty()
        );
        git(
            &executable,
            &writer,
            &["push", "-q", "../other.git", "HEAD:refs/pull/28/head"],
        );
        assert_eq!(
            prs.observe(&executable, &reader, "origin", &mut stopped)
                .await
                .unwrap(),
            [28]
        );
        assert_eq!(git(&executable, &reader, &["count-objects", "-v"]), before);
        assert!(!reader.join(".git/FETCH_HEAD").exists());
    }

    #[test]
    fn truncated_or_malformed_advertisements_are_not_partial_successes() {
        let oid = "a".repeat(40);
        for bytes in [
            format!("{oid}\trefs/pull/3/head"),
            format!("{oid}\trefs/pull/3/head\nbroken\trefs/pull/4/head\n"),
        ] {
            assert!(new_numbers(bytes.as_bytes(), 0).is_err());
        }
        let mixed = format!(
            "{oid}\trefs/pull/9999999999999999999999/head\n{oid}\trefs/pull/4/head\n{oid}\trefs/pull/9007199254740992/head\n"
        );
        assert_eq!(new_numbers(mixed.as_bytes(), 0).unwrap(), [4]);
        assert!(new_numbers(b"", 0).unwrap().is_empty());
    }
}

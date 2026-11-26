//! Run the installed client's ordinary update subcommand outside the terminal lock.
use std::process::Stdio;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
};

static UPDATING: Mutex<()> = Mutex::const_new(());

async fn output_tail(mut pipe: impl AsyncRead + Unpin) -> std::io::Result<String> {
    const LIMIT: usize = 8192;
    let mut tail = Vec::new();
    let mut chunk = [0; 2048];
    loop {
        let count = pipe.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let discard = (tail.len() + count).saturating_sub(LIMIT);
        tail.drain(..discard);
        tail.extend_from_slice(&chunk[..count]);
    }
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

async fn run(check: bool) -> crate::Result<(bool, String, String)> {
    let mut command = Command::new(crate::client::reload::installed_program()?);
    command.arg("update");
    if check {
        command.arg("--check");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (status, out, err) =
        tokio::try_join!(child.wait(), output_tail(stdout), output_tail(stderr))?;
    let summary = if status.success() {
        let lines = out.lines().filter(|line| !line.trim().is_empty());
        lines
            .clone()
            .find(|line| line.starts_with("Update available:"))
            .or_else(|| lines.last())
            .unwrap_or("Update command completed")
            .to_owned()
    } else {
        let reason = err
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("see update output");
        format!("Update failed ({status}): {reason}")
    };
    Ok((status.success(), summary, format!("{out}\n{err}")))
}

pub(in crate::client) fn submit(check: bool) -> Result<(), String> {
    let (runtime, handle) = crate::client::interface::harness::local_runtime()?;
    runtime.local_registry().ensure_open()?;
    let guard = UPDATING
        .try_lock()
        .map_err(|_| "A client update is already running")?;
    let source = crate::runtime::call_scope::Source::capture();
    handle.spawn(source.scope(async move {
        let _guard = guard;
        let (ok, summary, output) = run(check).await.unwrap_or_else(|error| {
            (
                false,
                format!("Unable to run client update: {error}"),
                String::new(),
            )
        });
        let level = if ok { "Info" } else { "Error" };
        let lines = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        for line in lines.clone().filter(|line| *line != summary) {
            crate::client::render::status("update", level, line);
        }
        crate::client::render::status("update", level, &summary);
        let details = lines
            .filter(|line| *line != summary && !line.starts_with("Tool repository: {"))
            .map(|line| match line {
                "Run ssmcp update to install it." => {
                    "Close this panel and run /update to install it."
                }
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        crate::client::terminal::finish_client_update(&summary, &details);
    }));
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn slash_update_follows_the_replaced_installation_entry() {
        use std::{fs, os::unix::fs::symlink};

        const CHILD: &str = "SSMCP_TEST_SLASH_UPDATE_ENTRY";
        const TEST: &str =
            "client::update::terminal::tests::slash_update_follows_the_replaced_installation_entry";
        if std::env::var_os(CHILD).is_none() {
            let directory = super::super::tests::temporary();
            // Exercise an aliased parent even on hosts whose temp directory is canonical.
            let alias = directory.join("alias");
            symlink(".", &alias).unwrap();
            let entry = alias.join("ssmcp fixture");
            symlink(std::env::current_exe().unwrap(), &entry).unwrap();
            let output = std::process::Command::new(&entry)
                .args(["--exact", TEST, "--nocapture"])
                .current_dir(&directory)
                .env(CHILD, &entry)
                .output()
                .unwrap();
            fs::remove_dir_all(directory).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }

        // A foreground client pins this same entry before its first connection.
        let entry = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        assert_eq!(crate::client::reload::installed_program().unwrap(), entry);
        assert!(entry.is_symlink());
        let replace = |fixture| {
            let staged = entry.with_extension("new");
            fs::write(&staged, fixture).unwrap();
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o700)).unwrap();
            fs::rename(staged, &entry).unwrap();
        };
        replace(
            "#!/bin/sh\ntest \"$#:$1\" = '1:update' || exit 70\necho 'Updated to fixture v2.'\n",
        );
        let (ok, summary, _) = run(false).await.unwrap();
        assert!(ok);
        assert_eq!(summary, "Updated to fixture v2.");

        replace(
            "#!/bin/sh\ntest \"$#:$1:$2\" = '2:update:--check' || exit 71\necho 'Fixture v2 is current.'\n",
        );
        let (ok, summary, _) = run(true).await.unwrap();
        assert!(ok);
        assert_eq!(summary, "Fixture v2 is current.");

        replace("#!/bin/sh\necho 'checksum mismatch' >&2\nexit 7\n");
        let (ok, summary, _) = run(false).await.unwrap();
        assert!(!ok);
        assert!(summary.contains("7") && summary.contains("checksum mismatch"));
        fs::remove_file(entry).unwrap();
        assert!(run(false).await.is_err());
    }

    #[tokio::test]
    async fn verbose_output_is_drained_and_keeps_the_final_diagnostic() {
        let bytes = ["x".repeat(32768), "finished".into()].concat();
        let tail = output_tail(bytes.as_bytes()).await.unwrap();
        assert_eq!(tail.len(), 8192);
        assert!(tail.ends_with("finished"));
    }
}

//! Run the installed client's ordinary update subcommand outside the terminal lock.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
};

static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
static UPDATING: Mutex<()> = Mutex::const_new(());

pub(in crate::client) fn remember_executable() {
    // On Linux current_exe() can acquire a " (deleted)" suffix once the
    // installer replaces the running image. Keep the original installation path.
    if EXECUTABLE.get().is_none()
        && let Ok(path) = std::env::current_exe()
    {
        let _ = EXECUTABLE.set(path);
    }
}

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

async fn run(program: &Path, check: bool) -> crate::Result<(bool, String, String)> {
    let mut command = Command::new(program);
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
        out.lines()
            .rev()
            .find(|line| !line.trim().is_empty())
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
    let program = EXECUTABLE
        .get()
        .ok_or("Cannot locate this client's installed executable")?;
    let guard = UPDATING
        .try_lock()
        .map_err(|_| "A client update is already running")?;
    let source = crate::runtime::call_scope::Source::capture();
    handle.spawn(source.scope(async move {
        let _guard = guard;
        let (ok, summary, output) = run(program, check).await.unwrap_or_else(|error| {
            (
                false,
                format!("Unable to run client update: {error}"),
                String::new(),
            )
        });
        let level = if ok { "Info" } else { "Error" };
        for line in output.lines().map(str::trim).filter(|s| !s.is_empty()) {
            crate::client::render::status("update", level, line);
        }
        crate::client::render::status("update", level, &summary);
        crate::client::terminal::notice(&summary);
    }));
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn slash_update_runs_only_the_installed_entry_and_reports_its_result() {
        let directory = super::super::tests::temporary();
        let program = directory.join("ssmcp fixture");
        let fixture = r#"#!/bin/sh
test "$1" = update || exit 70
case "$#:$2" in
  1:) echo 'Updated ssmcp to v1.2.3.' ;;
  2:--check) echo 'Already the latest stable release.' ;;
  *) exit 71 ;;
esac
"#;
        std::fs::write(&program, fixture).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (check, expected) in [
            (false, "Updated ssmcp to v1.2.3."),
            (true, "Already the latest stable release."),
        ] {
            let (ok, summary, _) = run(&program, check).await.unwrap();
            assert!(ok);
            assert_eq!(summary, expected);
        }
        std::fs::write(
            &program,
            "#!/bin/sh\necho 'checksum mismatch' >&2\nexit 7\n",
        )
        .unwrap();
        let (ok, summary, _) = run(&program, false).await.unwrap();
        assert!(!ok);
        assert!(summary.contains("7") && summary.contains("checksum mismatch"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn verbose_output_is_drained_and_keeps_the_final_diagnostic() {
        let bytes = ["x".repeat(32768), "finished".into()].concat();
        let tail = output_tail(bytes.as_bytes()).await.unwrap();
        assert_eq!(tail.len(), 8192);
        assert!(tail.ends_with("finished"));
    }
}

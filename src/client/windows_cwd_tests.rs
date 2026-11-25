//! Exercise relative I/O from real Windows shells without ever writing to a
//! fallback directory: the child only writes when it can see our unique marker.
use super::Options;
use crate::{
    backend::shell::Shell, output::OutputStore, platform::ShellPreference, session::SessionSpec,
    wire::Operation,
};
use serde_json::json;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

struct Workspace {
    root: PathBuf,
    marker: String,
}

impl Workspace {
    fn new() -> Self {
        let mut nonce = [0; 12];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let id = crate::hex::encode(nonce);
        let root = std::env::temp_dir().join(format!("ssmcp cwd 演示 {id}"));
        let marker = format!("cwd-{id}.marker");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        for directory in [&root, &root.join("nested")] {
            fs::write(directory.join(&marker), "fixture").unwrap();
        }
        Self { root, marker }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn write_from_terminal(
    shell: &Shell,
    id: &str,
    directory: &Path,
    marker: &str,
    ps: bool,
) -> bool {
    let data = if ps {
        format!(
            "if (Test-Path -LiteralPath '{marker}') {{ Set-Content -Encoding ASCII out.txt OK }}\r"
        )
    } else {
        format!("if exist {marker} echo OK>out.txt\r")
    };
    shell
        .get(id)
        .unwrap()
        .perform(Operation::Write { data })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if fs::read_to_string(directory.join("out.txt")).is_ok_and(|text| text.trim() == "OK") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn windows_default_conpty_executes_in_the_canonical_workspace() {
    let ws = Workspace::new();
    let shell = Shell::with_shell_preference(
        ShellPreference::new(None, false),
        ws.root.clone(),
        OutputStore::new_connection().unwrap(),
    )
    .unwrap();
    // COMSPEC is the configured Windows system shell. The Windows CI images use cmd.exe.
    let is_cmd = shell
        .shell_path()
        .unwrap()
        .file_stem()
        .is_some_and(|s| s.eq_ignore_ascii_case("cmd"));
    let writes_here = write_from_terminal(&shell, "main", &ws.root, &ws.marker, false).await;
    shell.shutdown().await;
    assert!(
        is_cmd,
        "this regression requires a standard Windows COMSPEC"
    );
    assert!(writes_here, "default cmd did not write in the workspace");
}

#[tokio::test]
async fn windows_saved_shell_and_named_terminal_write_in_the_requested_directory() {
    for choice in ["cmd", "powershell"] {
        let ws = Workspace::new();
        let options =
            Options::parse(["--ssh", "fixture.invalid", "--shell", choice].map(OsString::from))
                .unwrap()
                .unwrap();
        let mut spec = options.session_spec().unwrap();
        // sessions create --cwd canonicalizes the stored directory.
        spec.cwd = ws.root.canonicalize().unwrap();
        let restored: SessionSpec =
            serde_json::from_slice(&serde_json::to_vec(&spec).unwrap()).unwrap();
        assert_eq!(restored.cwd, spec.cwd);
        let preference = Options::parse_saved(&restored)
            .unwrap()
            .unwrap()
            .shell_preference()
            .unwrap();
        let shell = Shell::with_shell_preference(
            preference,
            restored.cwd,
            OutputStore::new_connection().unwrap(),
        )
        .unwrap();
        let ps = choice == "powershell";
        let main_works = write_from_terminal(&shell, "main", &ws.root, &ws.marker, ps).await;
        let named = shell
            .call("terminal.open", json!({"name":"nested", "cwd":"nested"}))
            .await
            .unwrap();
        let nested = ws.root.join("nested");
        assert_eq!(named["cwd"], json!(nested.canonicalize().unwrap()));
        let named_works = write_from_terminal(
            &shell,
            named["terminal_id"].as_str().unwrap(),
            &nested,
            &ws.marker,
            ps,
        )
        .await;
        shell.shutdown().await;
        assert!(
            main_works,
            "restored {choice} did not write in the workspace"
        );
        assert!(
            named_works,
            "named {choice} did not write in the requested subdirectory"
        );
    }
}

#[tokio::test]
async fn windows_pipe_cmd_keeps_working_directory_and_canonical_receipt() {
    let ws = Workspace::new();
    let nested = ws.root.join("nested");
    let shell =
        Shell::with_workspace(ws.root.clone(), OutputStore::new_connection().unwrap()).unwrap();
    let command = format!("if exist {} echo OK>out.txt", ws.marker);
    let started = shell
        .call(
            "ssmcp.exec.start",
            json!({"command":["cmd.exe","/d","/c",command],
        "mode":"pipes", "cwd":"nested"}),
        )
        .await
        .unwrap();
    let done = shell
        .call(
            "ssmcp.exec.wait",
            json!({"job_id":started["job_id"],"wait_ms":10000}),
        )
        .await
        .unwrap();
    shell.shutdown().await;
    assert_eq!(done["exit_code"], 0, "{done}");
    assert_eq!(done["cwd"], json!(nested.canonicalize().unwrap()));
    assert_eq!(
        fs::read_to_string(nested.join("out.txt")).unwrap().trim(),
        "OK"
    );
    assert!(!ws.root.join("out.txt").exists());
}

#[test]
fn windows_cmd_refuses_true_unc_before_launch_and_other_programs_keep_it() {
    for cwd in [
        r"\\nonexistent.invalid\share",
        r"\\?\UNC\nonexistent.invalid\share",
    ] {
        let path = Path::new(cwd);
        for program in [
            "cmd",
            r"C:\Windows\System32\CMD.EXE",
            "task.BAT",
            "task.cmd",
        ] {
            let error = crate::platform::child_cwd(Path::new(program), path).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
        for program in ["powershell", "pwsh.exe", "custom.exe"] {
            let passed = crate::platform::child_cwd(Path::new(program), path).unwrap();
            assert_eq!(passed.as_os_str(), path.as_os_str());
        }
    }
}

#[test]
fn windows_extended_only_directory_uses_exact_alias_or_fails_before_cmd() {
    let ws = Workspace::new();
    let root = ws.root.canonicalize().unwrap();
    let dotted = root.join("trailing-dot.");
    let similar = root.join("trailing-dot");
    fs::create_dir(&similar).unwrap();
    fs::create_dir(&dotted).unwrap();
    let long = root
        .join("long-directory-".repeat(10))
        .join("more-directory-".repeat(10));
    fs::create_dir_all(&long).unwrap();
    for original in [&dotted, &long] {
        let canonical = original.canonicalize().unwrap();
        let marker = format!("original-{}", ws.marker);
        fs::write(canonical.join(&marker), "fixture").unwrap();
        match crate::platform::child_cwd(Path::new("cmd.exe"), &canonical) {
            Ok(alias) => {
                assert_eq!(fs::canonicalize(&alias).unwrap(), canonical);
                let check = format!("if not exist {marker} exit /b 33");
                assert!(
                    std::process::Command::new("cmd.exe")
                        .args(["/d", "/c", &check])
                        .current_dir(alias)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::Unsupported),
        }
        let passed = crate::platform::child_cwd(Path::new("powershell"), &canonical).unwrap();
        assert_eq!(passed.as_os_str(), canonical.as_os_str());
    }
}

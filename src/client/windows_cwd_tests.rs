//! Exercise relative I/O from real Windows shells without ever writing to a
//! fallback directory: the child only writes when it can see our unique marker.
use super::Options;
use crate::{
    backend::shell::Shell, output::OutputStore, platform::ShellPreference, session::SessionSpec,
    wire::Operation,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
        let marker = format!("cwd-{id}");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        for directory in [&root, &root.join("nested")] {
            fs::create_dir(directory.join(&marker)).unwrap();
        }
        Self { root, marker }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Enter {
    Packed,
    Split,
    AfterEcho,
}

async fn write_from_terminal(
    terminal: std::sync::Arc<crate::pty::Terminal>,
    directory: &Path,
    marker: &str,
    ps: bool,
    enter: Enter,
    sample_modes: bool,
) -> Result<String, String> {
    let expected = if ps {
        use std::path::{Component, Prefix};
        let root = directory.canonicalize().unwrap();
        let Some(Component::Prefix(prefix)) = root.components().next() else {
            panic!("test workspace has no drive prefix");
        };
        let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
            panic!("test workspace is not on a local drive");
        };
        char::from(letter).to_ascii_uppercase().to_string()
    } else {
        "OK".into()
    };
    // Both writes are relative and require the unique parent to exist. Keep the
    // interactive line short enough to avoid an unnecessary PSReadLine stress test.
    let relative = format!(r"{marker}\out.txt");
    let command = if ps {
        format!("Set-Content {relative} $PWD.Drive.Name")
    } else {
        format!("echo OK>{relative}")
    };
    let probe = directory.join(marker);
    let output = probe.join("out.txt");
    let key = super::input::physical_enter();
    assert_eq!(key, "\r");
    let (mut typed, mut echoed, mut entered, mut sampling) = (false, false, false, false);
    let (rows, cols) = super::terminal::pane_size(24, 80);
    let mut display = vt100::Parser::new_with_callbacks(
        rows.try_into().unwrap(),
        cols.try_into().unwrap(),
        0,
        super::terminal::Replies::default(),
    );
    let mut cursor = 0;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if fs::read_to_string(&output).is_ok_and(|text| text.trim() == expected) {
                return Ok(());
            }
            let page = terminal.read_page(cursor, 8192, 100).await?;
            cursor = page["next_cursor"].as_u64().unwrap();
            let bytes = STANDARD
                .decode(page["data_base64"].as_str().unwrap())
                .unwrap();
            display.process(&bytes);
            // This test drives the PTY directly, so it also owns the VT replies
            // a running client's input loop normally sends to this session.
            let reply = display.callbacks_mut().take_bytes();
            if !reply.is_empty() {
                terminal
                    .perform(Operation::Write {
                        data: String::from_utf8(reply).unwrap(),
                    })
                    .await?;
            }
            let screen = display.screen().contents();
            if !typed && screen.trim_end().ends_with('>') {
                let data = if enter == Enter::Packed {
                    format!("{command}{key}")
                } else {
                    command.clone()
                };
                terminal.perform(Operation::Write { data }).await?;
                typed = true;
                entered = enter == Enter::Packed;
            }
            if typed && screen.replace('\n', "").contains(&command) {
                echoed = true;
            }
            if sample_modes && echoed && !sampling {
                fs::write(probe.join("sample.request"), []).map_err(|e| e.to_string())?;
                sampling = true;
            }
            if typed
                && !entered
                && (enter == Enter::Split || echoed)
                && (!sample_modes || probe.join("modes.txt").is_file())
            {
                terminal
                    .perform(Operation::Write { data: key.clone() })
                    .await?;
                entered = true;
            }
            if page["output_eof"] == true {
                return Err("terminal closed".to_owned());
            }
        }
    })
    .await;
    let stages = format!("{enter:?} typed={typed} full_echo={echoed} enter_sent={entered}");
    let reason = match result {
        Ok(Ok(())) => return Ok(format!("{stages} marker={expected:?}")),
        Ok(Err(reason)) => reason,
        Err(_) => "timed out".into(),
    };
    let actual = match fs::read_to_string(&output) {
        Ok(text) => format!("actual marker {text:?}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing marker".into(),
        Err(error) => error.to_string(),
    };
    Err(format!(
        "{reason}: {actual} at {}, expected {expected:?}, {stages}, screen: {:?}",
        output.display(),
        display.screen().contents(),
    ))
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
    let writes_here = write_from_terminal(
        shell.primary(),
        &ws.root,
        &ws.marker,
        false,
        Enter::Packed,
        false,
    )
    .await;
    shell.shutdown().await;
    assert!(
        is_cmd,
        "this regression requires a standard Windows COMSPEC"
    );
    writes_here.unwrap_or_else(|error| panic!("default cmd: {error}"));
}

#[tokio::test]
async fn windows_saved_shell_and_named_terminal_write_in_the_requested_directory() {
    let mut results = Vec::new();
    for (choice, enter) in [
        ("cmd", Enter::Packed),
        ("cmd", Enter::AfterEcho),
        ("powershell", Enter::Packed),
        ("powershell", Enter::Split),
        ("powershell", Enter::AfterEcho),
    ] {
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
        let named = shell
            .call("terminal.open", json!({"name":"nested", "cwd":"nested"}))
            .await
            .unwrap();
        let nested = ws.root.join("nested");
        assert_eq!(named["cwd"], json!(nested.canonicalize().unwrap()));
        let (main, named) = tokio::join!(
            write_from_terminal(shell.primary(), &ws.root, &ws.marker, ps, enter, false),
            write_from_terminal(
                shell.get(named["terminal_id"].as_str().unwrap()).unwrap(),
                &nested,
                &ws.marker,
                ps,
                enter,
                false
            ),
        );
        results.push((format!("{choice} {enter:?} main"), main));
        results.push((format!("{choice} {enter:?} named"), named));

        if ps && enter == Enter::AfterEcho {
            // A separate interactive process can take measurements even if the
            // ordinary shells never execute the submitted command.
            let dir = ws.root.join("instrumented");
            fs::create_dir(&dir).unwrap();
            fs::create_dir(dir.join(&ws.marker)).unwrap();
            results.push((
                "instrumented PowerShell".into(),
                measure_editor(&shell, &dir, &ws.marker).await,
            ));
        }
        shell.shutdown().await;
    }
    let report = results
        .iter()
        .map(|(case, r)| format!("{case}: {r:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(
                file,
                "### Windows ConPTY cwd probes\n\n```text\n{report}\n```"
            );
        }
    }
    assert!(results.iter().all(|(_, r)| r.is_ok()), "{report}");
}

async fn measure_editor(shell: &Shell, dir: &Path, marker: &str) -> Result<String, String> {
    let probe = dir.join(marker);
    let script: Vec<u8> = include_str!("windows_cwd_probe.ps1")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let program = shell.shell_path()?;
    let command = [
        program.to_string_lossy().into_owned(),
        "-NoLogo".into(),
        "-NoExit".into(),
        "-EncodedCommand".into(),
        STANDARD.encode(script),
    ];
    let mut env = crate::platform::launcher_environment(&program);
    env.insert(
        "SSMCP_CWD_PROBE".into(),
        probe.to_string_lossy().into_owned(),
    );
    let terminal = crate::pty::Terminal::spawn_with_capture(
        crate::pty::JobConfig {
            command: &command,
            cwd: dir,
            env: &env,
            id: "cwd-diagnostic",
            size: Some(super::terminal::pane_size(24, 80)),
            live: false,
            output_event: None,
        },
        None,
    )
    .map_err(|e| e.to_string())?;
    let outcome =
        write_from_terminal(terminal.clone(), dir, marker, true, Enter::AfterEcho, true).await;
    terminal.shutdown().await;
    let prompt = fs::read_to_string(probe.join("prompt.json"))
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(|e| e.to_string()));
    let editor = fs::read_to_string(probe.join("editor.txt"));
    let modes = fs::read_to_string(probe.join("modes.txt"));
    let flags = modes.as_ref().ok().and_then(|s| {
        let (input, output) = s.split_once(',')?;
        Some((input.parse::<u32>().ok()?, output.parse::<u32>().ok()?))
    });
    // Native ENABLE_VIRTUAL_TERMINAL_INPUT and ENABLE_VIRTUAL_TERMINAL_PROCESSING.
    let vt = flags.map(|(i, o)| (i & 0x200 != 0, o & 0x4 != 0));
    let report = format!(
        "{outcome:?} prompt={prompt:?} after_echo: console_modes={modes:?} vt_in_out={vt:?} editor_assembly={editor:?}"
    );
    if outcome.is_ok() && prompt.is_ok() && editor.is_ok() && flags.is_some() {
        Ok(report)
    } else {
        Err(report)
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

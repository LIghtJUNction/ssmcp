//! Manual ARM64 diagnosis. All processes and markers belong to this single fixture.
use super::Workspace;
use crate::{pty::Terminal, wire::Operation};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{collections::HashMap, fs, path::Path, sync::Arc, time::Duration};
use tokio::time::{Instant, timeout};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Input {
    Cr,
    Lf,
    NegotiatedWin32,
    Bootstrap,
    Noninteractive,
    ReadKey,
}

async fn modes(pid: u32, path: &Path) -> Value {
    let mut helper = tokio::process::Command::new("python");
    helper
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/windows_arm_console.py"
        ))
        .arg(pid.to_string())
        .arg(path)
        .kill_on_drop(true);
    if matches!(timeout(Duration::from_secs(4), helper.status()).await, Ok(Ok(s)) if s.success())
        && let Ok(bytes) = fs::read(path)
        && let Ok(record) = serde_json::from_slice(&bytes)
    {
        return record;
    }
    json!({"unavailable":true})
}

fn win32_requested(raw: &[u8]) -> bool {
    // ConPTY's DECSET/DECRST 9001 can span read_page boundaries.
    raw.windows(3)
        .enumerate()
        .filter(|(_, w)| *w == b"\x1b[?")
        .filter_map(|(i, _)| {
            let tail = &raw[i + 3..];
            let end = tail
                .iter()
                .position(|b| !b.is_ascii_digit() && *b != b';')?;
            let set = match tail[end] {
                b'h' => true,
                b'l' => false,
                _ => return None,
            };
            tail[..end]
                .split(|b| *b == b';')
                .any(|p| p == b"9001")
                .then_some(set)
        })
        .next_back()
        .unwrap_or(false)
}

#[test]
fn win32_enter_requires_an_observed_mode_set() {
    assert!(!win32_requested(b"\x1b[?9001$p"));
    assert!(win32_requested(b"\x1b[?25;9001h"));
    assert!(!win32_requested(b"\x1b[?9001h\x1b[?9001l"));
}

async fn submit(term: &Arc<Terminal>, bytes: &[u8]) -> Result<(), String> {
    term.perform(Operation::Write {
        data: String::from_utf8(bytes.to_vec()).unwrap(),
    })
    .await
    .map(|_| ())
}

async fn probe(program: &str, cmd: bool, input: Input) -> Result<Value, String> {
    let ws = Workspace::new();
    let dir = ws.root.join(&ws.marker);
    let out = dir.join("out.txt");
    let started = dir.join("boot.json");
    let mut argv = vec![program.to_owned()];
    let mut env = HashMap::new();
    env.insert("SSMCP_ARM_BOOT".to_owned(), started.display().to_string());
    if !cmd {
        argv.push("-NoLogo".into());
        if matches!(
            input,
            Input::Bootstrap | Input::Noninteractive | Input::ReadKey
        ) {
            // This first durable receipt precedes prompt handlers, Add-Type and any typed input.
            let mut script = String::from(
                "[IO.File]::WriteAllText(($env:SSMCP_ARM_BOOT+'.entry'),'1');[IO.File]::WriteAllText($env:SSMCP_ARM_BOOT, (@{engine=$PSVersionTable.PSVersion.ToString();cwd=$PWD.Path;drive=$PWD.Drive.Name}|ConvertTo-Json -Compress));",
            );
            argv.push("-NoProfile".into());
            if input == Input::Noninteractive {
                argv.push("-NonInteractive".into());
                script.push_str(&format!(
                    "Set-Content -LiteralPath '{}\\out.txt' -Value 'OK';",
                    ws.marker
                ));
            } else if input == Input::ReadKey {
                script.push_str("[Console]::Write('PS> '); $k=[Console]::ReadKey($true); [IO.File]::WriteAllText(($env:SSMCP_ARM_BOOT+'.key'),(@{key=$k.Key.ToString();code=[int]$k.KeyChar}|ConvertTo-Json -Compress))");
            } else {
                argv.push("-NoExit".into());
                // Observe the loaded editor, without importing it or changing Enter's binding.
                script.push_str("function global:prompt {try {$m=Get-Module PSReadLine; $key=if($m){(Get-PSReadLineKeyHandler -Bound|Where-Object {$_.Key -eq 'Enter'}).Function}; [IO.File]::WriteAllText(($env:SSMCP_ARM_BOOT+'.prompt'),(@{editor=[string]$m.Version;enter=@($key)}|ConvertTo-Json -Compress))}catch{[IO.File]::WriteAllText(($env:SSMCP_ARM_BOOT+'.prompt-error'),$_.Exception.GetType().Name)}; 'PS> '}");
            }
            let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
            argv.extend(["-EncodedCommand".into(), STANDARD.encode(encoded)]);
        }
    }
    let (rows, cols) = (24_u16, 120_u16);
    let term = Terminal::spawn_with_capture(
        crate::pty::JobConfig {
            command: &argv,
            cwd: &ws.root,
            env: &env,
            id: "arm-ci",
            size: Some((rows, cols)),
            live: false,
            output_event: None,
        },
        None,
    )
    .map_err(|e| e.to_string())?;
    let command = if cmd {
        format!("echo OK>{}\\out.txt", ws.marker)
    } else {
        format!("Set-Content {}\\out.txt OK", ws.marker)
    };
    let end = Instant::now() + Duration::from_secs(22);
    let mut screen = vt100::Parser::new_with_callbacks(
        rows.try_into().unwrap(),
        cols.try_into().unwrap(),
        0,
        crate::client::terminal::Replies::default(),
    );
    let mut raw = Vec::new();
    let mut cursor = 0;
    let (mut typed, mut echoed, mut sent, mut eof) = (false, false, false, false);
    let (mut prompt_modes, mut edit_modes) = (Value::Null, Value::Null);
    let mut skipped = false;
    let mut replies = 0;
    let mut error = None;
    let drive_ok = || fs::read_to_string(&out).is_ok_and(|s| s.trim() == "OK");
    let key_receipt = dir.join("boot.json.key");
    let result: Result<(), String> = async {
        while Instant::now() < end && !drive_ok() && !key_receipt.exists() {
            let page = term.read_page(cursor, 8192, 200).await?;
            cursor = page["next_cursor"].as_u64().ok_or("missing cursor")?;
            let data = STANDARD
                .decode(page["data_base64"].as_str().unwrap_or_default())
                .map_err(|e| e.to_string())?;
            let keep = (65536usize.saturating_sub(raw.len())).min(data.len());
            raw.extend_from_slice(&data[..keep]);
            screen.process(&data);
            let bytes = screen.callbacks_mut().take_bytes();
            if !bytes.is_empty() {
                replies += 1;
                submit(&term, &bytes).await?;
            }
            let text = screen.screen().contents();
            if input == Input::ReadKey && !sent && text.trim_end().ends_with('>') {
                prompt_modes = modes(term.pid(), &dir.join("prompt-modes.json")).await;
                submit(&term, b"\r").await?;
                sent = true;
            } else if !matches!(input, Input::Noninteractive | Input::ReadKey)
                && !typed
                && text.trim_end().ends_with('>')
            {
                prompt_modes = modes(term.pid(), &dir.join("prompt-modes.json")).await;
                submit(&term, command.as_bytes()).await?;
                typed = true;
            }
            if typed && !sent && text.replace('\n', "").contains(&command) {
                echoed = true;
                edit_modes = modes(term.pid(), &dir.join("edit-modes.json")).await;
                let key: &[u8] = match input {
                    Input::NegotiatedWin32
                        if cursor == raw.len() as u64 && win32_requested(&raw) =>
                    {
                        b"\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_"
                    }
                    Input::NegotiatedWin32 => {
                        skipped = true;
                        break;
                    }
                    Input::Lf => b"\n",
                    _ => b"\r",
                };
                submit(&term, key).await?;
                sent = true;
            }
            if page["output_eof"] == true {
                eof = true;
                break;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        error = Some(e);
    }
    let marker = drive_ok();
    let last_modes = if !marker && !eof && !skipped {
        modes(term.pid(), &dir.join("final-modes.json")).await
    } else {
        Value::Null
    };
    term.shutdown().await;
    let read_json = |p: &Path| {
        fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    };
    let boot = read_json(&started).map(|v| {
        let actual = v["cwd"]
            .as_str()
            .and_then(|s| Path::new(s).canonicalize().ok());
        let matches = actual.is_some() && actual == ws.root.canonicalize().ok();
        json!({"engine":v["engine"],"drive":v["drive"],"cwd_matches":matches})
    });
    let editor = read_json(&dir.join("boot.json.prompt"));
    let editor_error = fs::read_to_string(dir.join("boot.json.prompt-error")).ok();
    let received_key = read_json(&key_receipt);
    Ok(
        json!({"input":format!("{input:?}"),"typed":typed,"echoed":echoed,"sent":sent,"marker":marker,
        "eof":eof,"bytes_observed":cursor,"replies":replies,"skipped_unadvertised":skipped,
        "protocol_sample_truncated":cursor>raw.len() as u64,"win32_requested":win32_requested(&raw),
        "entered_bootstrap":dir.join("boot.json.entry").is_file(),"boot":boot,"received_key":received_key,"prompt_editor":editor,"prompt_editor_error":editor_error,"prompt_modes":prompt_modes,"edit_modes":edit_modes,"last_modes":last_modes,"transport_error":error}),
    )
}

#[tokio::test]
#[ignore = "requires a disposable native Windows ARM64 CI runner"]
async fn compare_shell_input_and_startup() {
    let mut records = Vec::new();
    for (shell, variable, inputs) in [
        ("cmd", "SSMCP_ARM_CMD", &[Input::Cr][..]),
        (
            "WindowsPowerShell",
            "SSMCP_ARM_LEGACY",
            &[
                Input::Noninteractive,
                Input::ReadKey,
                Input::Cr,
                Input::Bootstrap,
                Input::Lf,
                Input::NegotiatedWin32,
            ][..],
        ),
        (
            "PowerShell7",
            "SSMCP_ARM_PWSH",
            &[
                Input::Noninteractive,
                Input::ReadKey,
                Input::Cr,
                Input::Bootstrap,
                Input::NegotiatedWin32,
            ][..],
        ),
    ] {
        let program =
            std::env::var(variable).expect("workflow must provide each resolved executable");
        for &input in inputs {
            let observed = probe(&program, shell == "cmd", input).await;
            println!("{shell} {input:?}: {observed:?}");
            records
                .push(json!({"shell":shell,"input":format!("{input:?}"),"observation":observed}));
        }
    }
    let path = std::env::var_os("SSMCP_ARM_REPORT").expect("CI report path");
    let data = json!({"source":std::env::var("GITHUB_SHA").unwrap_or_default(),"cases":records});
    fs::write(path, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
    for shell in ["WindowsPowerShell", "PowerShell7"] {
        assert!(
            records.iter().any(|r| r["shell"] == shell
                && r["input"] == "Noninteractive"
                && r["observation"]["Ok"]["marker"] == true
                && r["observation"]["Ok"]["boot"]["cwd_matches"] == true),
            "{shell} independent startup or cwd marker was missing; see diagnostic records"
        );
    }
    for shell in ["cmd", "WindowsPowerShell", "PowerShell7"] {
        assert!(
            records.iter().any(|r| r["shell"] == shell
                && r["input"] == "Cr"
                && r["observation"]["Ok"]["marker"] == true),
            "{shell} ordinary Enter did not produce the relative marker; see all diagnostic records"
        );
    }
}

use super::*;
use crate::computer::tests::Temporary;
use std::{fs, future::pending};

fn fixture_schema() -> Value {
    json!({"version":"0.4.2", "commands":[
        {"name":"target.locate","read_only":true,"parameters":[
            {"name":"window", "type":"text","required":false},{"name":"text","type":"text","required":false},
            {"name":"exact","type":"boolean","required":false},{"name":"case_sensitive","type":"boolean","required":false}]}
    ]})
}

#[test]
fn target_is_explicit_and_never_a_url_or_default() {
    for id in ["", "active", "1", "0x", "Fixture Title"] {
        assert!(Request::locate(id, "label").is_err());
    }
    assert!(Request::locate("0x24", " ").is_err());
    for (port, page) in [
        ("0", "id"),
        ("65536", "id"),
        ("localhost", "id"),
        ("9", ""),
        ("9", "../browser/id"),
        ("9", "a?b=c"),
    ] {
        assert!(Request::observe(port, page, "#fixture").is_err());
    }
    assert!(Request::observe("9", "page_42", "#fixture").is_ok());
}

#[test]
fn incompatible_version_and_schema_fail_before_dispatch() {
    let req = Request::locate("0x24", "label").unwrap();
    let schema = fixture_schema();
    let check = |v: Value, version: &str| {
        serde_json::from_value::<Schema>(v)
            .unwrap()
            .verify(version, &req)
    };
    assert!(check(schema.clone(), "0.4.2").is_ok());
    for version in ["0.3.0", "0.4.1", "0.5.0", "0.4.3", "unknown"] {
        assert!(check(schema.clone(), version).is_err());
    }
    for (key, value) in [("type", json!("integer")), ("name", json!("other"))] {
        let mut changed = schema.clone();
        changed["commands"][0]["parameters"][0][key] = value;
        assert!(check(changed, "0.4.2").is_err());
    }
    let mut changed = schema.clone();
    changed["commands"][0]["read_only"] = json!(false);
    assert!(check(changed, "0.4.2").is_err());
    for addition in [
        json!({"name":"unknown", "type":"integer","required":true}),
        schema["commands"][0]["parameters"][0].clone(),
    ] {
        let mut changed = schema.clone();
        changed["commands"][0]["parameters"]
            .as_array_mut()
            .unwrap()
            .push(addition);
        assert!(check(changed, "0.4.2").is_err());
    }
    let mut changed = schema.clone();
    changed["commands"]
        .as_array_mut()
        .unwrap()
        .push(schema["commands"][0].clone());
    assert!(check(changed, "0.4.2").is_err());
}

#[test]
fn malformed_failed_and_out_of_order_receipts_are_errors() {
    let op = ["timer.status"];
    let valid = json!({"status":"completed","completed":1,"results":[{"step":0,"operation":op[0],"value":{"running":false}}]});
    assert!(receipt(&serde_json::to_vec(&valid).unwrap(), &op).is_ok());
    for bad in [b"{".as_slice(), b"{} {}", b"\xff"] {
        assert!(receipt(bad, &op).is_err());
    }
    for (key, value) in [
        ("status", json!("failed")),
        ("completed", json!(0)),
        ("results", json!([])),
    ] {
        let mut changed = valid.clone();
        changed[key] = value;
        assert!(receipt(&serde_json::to_vec(&changed).unwrap(), &op).is_err());
    }
    for (key, value) in [
        ("step", json!(1)),
        ("operation", json!("mouse.click")),
        ("value", Value::Null),
    ] {
        let mut changed = valid.clone();
        changed["results"][0][key] = value;
        assert!(receipt(&serde_json::to_vec(&changed).unwrap(), &op).is_err());
    }
}

#[cfg(unix)]
fn fake(path: &Path, source: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!/bin/sh\n{source}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[test]
fn missing_and_broken_path_entries_are_distinct() {
    use std::os::unix::fs::symlink;
    let root = Temporary::new();
    let later = Temporary::new();
    let search = std::env::join_paths([&root.0, &later.0]).unwrap();
    assert!(probe::path_entry(None).unwrap().is_none());
    assert!(probe::path_entry(Some(&search)).unwrap().is_none());
    fake(&later.0.join("zuse"), "exit 74");
    let first = root.0.join("zuse");
    symlink(root.0.join("missing"), &first).unwrap();
    assert!(probe::path_entry(Some(&search)).is_err());
    fs::remove_file(&first).unwrap();
    fs::write(&first, "not executable").unwrap();
    assert!(probe::path_entry(Some(&search)).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn locate_sends_one_literal_argument_and_checks_returned_window() {
    let root = Temporary::new();
    let path = root.0.join("fixture $ zuse");
    let label = r#"label "quoted"; $(literal) \ value"#;
    let record = root.0.join("argv");
    let reply = root.0.join("receipt");
    let response = |id: &str| {
        json!({"status":"completed","completed":1,"results":[{"step":0,"operation":"target.locate",
        "value":{"location":{"window":{"id":id,"mapped":true,"pid":42},"target":{"name":label}}}}]})
    };
    fs::write(&reply, response("0x24").to_string()).unwrap();
    fake(
        &path,
        &format!(
            r#"case "$1" in
schema) printf '%s\n' '{}' ;;
eval) [ "$#" -eq 3 ] && [ "$3" = '--read-only' ] || exit 9
      printf '%s' "$2" > '{}'
      cat '{}' ;;
*) exit 87 ;;
esac"#,
            fixture_schema(),
            record.display(),
            reply.display()
        ),
    );
    let digest = release::digest(&fs::read(&path).unwrap());
    let req = Request::locate("0x24", label).unwrap();
    let mut stop = Box::pin(pending());
    let value = inspect(&path, "0.4.2", &digest, &req, stop.as_mut())
        .await
        .unwrap();
    assert_eq!(value["window"]["id"], "0x24");
    assert_eq!(
        fs::read_to_string(record).unwrap(),
        format!(
            "target.locate(window=\"0x24\",text={},exact=true,case_sensitive=true)",
            json!(label)
        )
    );
    fs::write(&reply, response("0x25").to_string()).unwrap();
    assert!(
        inspect(&path, "0.4.2", &digest, &req, stop.as_mut())
            .await
            .unwrap_err()
            .contains("different")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exit_status_and_output_budget_override_successful_looking_json() {
    let root = Temporary::new();
    let path = root.0.join("zuse");
    fake(&path, r#"printf '%s' '{"status":"completed"}'; exit 37"#);
    let err = probe::capture(
        &path,
        &["eval", "timer.status()"],
        None,
        4096,
        DEADLINE,
        pending(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("37"), "{err}");
    fake(
        &path,
        r#"exec 0<&-; printf '%s' '{"error":"rejected fixture input"}' >&2; exit 38"#,
    );
    let input = "x".repeat(128 * 1024);
    let err = probe::capture(
        &path,
        &["run", "-"],
        Some(&input),
        4096,
        DEADLINE,
        pending(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("38") && err.contains("rejected fixture input"),
        "{err}"
    );
    fake(&path, "printf '123456789'; sleep 30");
    let err = probe::capture(&path, &["schema"], None, 8, DEADLINE, pending())
        .await
        .unwrap_err();
    assert!(err.contains("output limit"), "{err}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_reaps_owned_descendant_before_return() {
    let root = Temporary::new();
    let path = root.0.join("zuse");
    let ready = root.0.join("started");
    fake(
        &path,
        &format!(
            "sleep 60 &\nprintf '%d' \"$!\" > '{}'\nwait",
            ready.display()
        ),
    );
    let stop = async {
        while fs::read_to_string(&ready)
            .ok()
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        "fixture cancelled".into()
    };
    let err = probe::capture(
        &path,
        &["run", "-"],
        Some("timer.start()"),
        1024,
        DEADLINE,
        stop,
    )
    .await
    .unwrap_err();
    assert!(err.contains("fixture cancelled"));
    let pid = fs::read_to_string(ready).unwrap();
    assert!(!Path::new("/proc").join(pid).exists());
}

/// Opt-in release compatibility check. Only a loopback protocol fixture and timer are used.
#[tokio::test]
#[ignore = "requires an explicit standalone 0.4.2+ test binary"]
async fn standalone_cli_against_owned_page_fixture() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{
        Message,
        handshake::server::{Request as Handshake, Response},
    };
    let path = std::path::PathBuf::from(
        std::env::var_os("SSMCP_TEST_ZUSE")
            .expect("set SSMCP_TEST_ZUSE to a verified fixture binary"),
    );
    assert!(path.is_absolute());
    let cli = probe::version(&path, pending()).await.unwrap();
    let version = cli["version"].as_str().unwrap();
    let digest = cli["sha256"].as_str().unwrap();
    let mut stop = Box::pin(pending());
    let check = inspect(&path, version, digest, &Request::Check, stop.as_mut())
        .await
        .unwrap();
    assert_eq!(check["persistent_shell"], true);
    let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let peer = tokio::spawn(async move {
        let (stream, _) = socket.accept().await.unwrap();
        #[allow(clippy::result_large_err)]
        let handshake = |req: &Handshake, resp: Response| {
            assert_eq!(req.uri().path(), "/devtools/page/FIXTURE");
            Ok(resp)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, handshake)
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let msg: Value = serde_json::from_str(&text).unwrap();
            let method = msg["method"].as_str().unwrap();
            let result = match method {
                "Page.enable" | "Page.setLifecycleEventsEnabled" => json!({}),
                "Target.getTargets" => {
                    json!({"targetInfos":[{"targetId":"other-fixture","title":"unrelated-fixture"}]})
                }
                "Runtime.evaluate" => {
                    let expr = msg["params"]["expression"].as_str().unwrap();
                    assert!(expr.contains("#fixture"));
                    json!({"result":{"value":{"backend":"dom","scope":"#fixture","connected":true,"nodes":[{"text":"fixture value"}],"errors":0,"truncated":false}}})
                }
                other => panic!("unexpected CDP operation {other}"),
            };
            seen.push(method.to_owned());
            ws.send(Message::Text(
                json!({"id":msg["id"],"result":result}).to_string().into(),
            ))
            .await
            .unwrap();
        }
        seen
    });
    let req = Request::observe(&port.to_string(), "FIXTURE", "#fixture").unwrap();
    let answer = inspect(&path, version, digest, &req, stop.as_mut())
        .await
        .unwrap();
    assert_eq!(answer["observation"]["nodes"][0]["text"], "fixture value");
    assert!(!answer.to_string().contains("unrelated-fixture"));
    let seen = tokio::time::timeout(Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(seen.iter().any(|method| method == "Runtime.evaluate"));
}

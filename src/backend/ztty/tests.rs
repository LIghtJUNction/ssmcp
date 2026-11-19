use super::*;
use crate::backend::Backend;

async fn tool(backend: &Arc<Backend>, name: &str, arguments: Value) -> Value {
    backend
        .perform(Operation::CallTool {
            name: name.into(),
            arguments,
        })
        .await
        .unwrap()
}
fn structured(result: &Value) -> &Value {
    result.get("structuredContent").unwrap_or(result)
}

#[test]
fn retained_utf8_prefix_does_not_shift_delivered_byte_cursors() {
    // Native OutputTail's retained left edge is in the middle of the globe
    // character in b"x\\xf0\\x9f\\x8c\\x8dy". It skips the two orphan bytes.
    let native = json!({"first_cursor":3,"next_cursor":6,"truncated":true,"output_base64":"eQ=="});
    let page = output_page(&native, 0).unwrap();
    assert_eq!((page.first, page.start, page.next), (3, 5, 6));
    assert_eq!(page.bytes, b"y");
    let mut impossible = native.clone();
    impossible["truncated"] = json!(false);
    assert!(output_page(&impossible, 0).is_err());
    impossible["next_cursor"] = json!(0);
    assert!(output_page(&impossible, 0).is_err());
    let continuation =
        json!({"first_cursor":0,"next_cursor":4,"truncated":false,"output_base64":"jA=="});
    assert_eq!(output_page(&continuation, 3).unwrap().bytes, [0x8c]);
}

async fn mapped(id: u64) -> bool {
    let mut command = Command::new("python3");
    command.args(["-c", r#"
import ctypes as c,sys
class Attr(c.Structure):
    _fields_=[('x',c.c_int),('y',c.c_int),('width',c.c_int),('height',c.c_int),('border_width',c.c_int),('depth',c.c_int),('visual',c.c_void_p),('root',c.c_ulong),('kind',c.c_int),('bit_gravity',c.c_int),('win_gravity',c.c_int),('backing_store',c.c_int),('backing_planes',c.c_ulong),('backing_pixel',c.c_ulong),('save_under',c.c_int),('colormap',c.c_ulong),('map_installed',c.c_int),('map_state',c.c_int),('all_event_masks',c.c_long),('your_event_mask',c.c_long),('do_not_propagate_mask',c.c_long),('override_redirect',c.c_int),('screen',c.c_void_p)]
x=c.CDLL('libX11.so.6');x.XOpenDisplay.restype=c.c_void_p;x.XOpenDisplay.argtypes=[c.c_char_p]
x.XGetWindowAttributes.argtypes=[c.c_void_p,c.c_ulong,c.POINTER(Attr)];x.XCloseDisplay.argtypes=[c.c_void_p]
handler=c.CFUNCTYPE(c.c_int,c.c_void_p,c.c_void_p)(lambda *_:0);x.XSetErrorHandler(handler)
d=x.XOpenDisplay(None)
if not d:sys.exit(2)
a=Attr();visible=x.XGetWindowAttributes(d,int(sys.argv[1]),c.byref(a))!=0 and a.map_state==2
x.XCloseDisplay(d);sys.exit(0 if visible else 1)
"#, &format!("{id}")]).kill_on_drop(true);
    let result = timeout(Duration::from_secs(2), command.output())
        .await
        .unwrap()
        .unwrap();
    result.status.success()
}

#[tokio::test]
#[ignore = "requires an explicitly supplied SSMCP_TEST_ZTTY and an isolated Xvfb display"]
async fn real_ztty_native_tools_and_compatibility_share_the_visible_pty() {
    let program = PathBuf::from(
        std::env::var_os("SSMCP_TEST_ZTTY").expect("supply the verified candidate explicitly"),
    );
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).unwrap();
    let root = std::env::temp_dir().join(format!(
        "ssmcp-ztty-test-{}-{}",
        std::process::id(),
        u64::from_le_bytes(nonce)
    ));
    std::fs::create_dir(&root).unwrap();
    let backend = Backend::with_output(
        &program,
        root.clone(),
        OutputStore::new_connection().unwrap(),
    )
    .await
    .unwrap();
    let catalog = backend.tools().await.unwrap();
    assert_eq!(
        catalog
            .iter()
            .filter(|t| t["name"].as_str().is_some_and(|n| n.starts_with("ztty.")))
            .count(),
        9
    );
    let registered = backend.registry.lookup("ztty.session_read").unwrap();
    assert_eq!(registered.native_name, "session_read");
    assert_eq!(registered.descriptor["annotations"]["readOnlyHint"], true);
    let native = tool(&backend, "ztty.session_list", json!({})).await;
    let main = structured(&native)["sessions"][0].clone();
    assert_eq!(main["visible"], true);
    assert_eq!(main["read_only"], false);
    let window = main["window_id"].as_u64().unwrap();
    assert!(mapped(window).await);
    let status = tool(&backend, "terminal.status", json!({})).await;
    assert_eq!(structured(&status)["pid"], main["pid"]);
    assert_eq!(
        structured(&status)["session_identity"],
        identity(&main).unwrap()
    );

    let first = tool(
        &backend,
        "terminal.read",
        json!({"cursor":0,"max_bytes":32768}),
    )
    .await;
    let origin = structured(&first)["next_cursor"].as_u64().unwrap();
    let command = "printf '\\377\\000中文 e\\314\\201 __ZTTY_DONE__\\n'\n";
    tool(&backend, "terminal.write", json!({"data":command})).await;
    let mut cursor = origin;
    let mut output = Vec::new();
    let expected = [
        vec![255, 0],
        "中文 e\u{301} __ZTTY_DONE__".as_bytes().to_vec(),
    ]
    .concat();
    timeout(Duration::from_secs(5), async {
        while !output
            .windows(expected.len())
            .any(|bytes| bytes == expected)
        {
            let result = tool(
                &backend,
                "terminal.read",
                json!({"cursor":cursor,"max_bytes":32768,"wait_ms":1000}),
            )
            .await;
            let result = structured(&result);
            output.extend(
                STANDARD
                    .decode(result["data_base64"].as_str().unwrap())
                    .unwrap(),
            );
            cursor = result["next_cursor"].as_u64().unwrap();
        }
    })
    .await
    .unwrap();
    let position = output
        .windows(2)
        .position(|bytes| bytes == [255, 0])
        .unwrap() as u64
        + origin;
    for (offset, expected) in [(0, 255), (1, 0), (2, 0xe4), (3, 0xb8), (4, 0xad)] {
        let result = tool(
            &backend,
            "terminal.read",
            json!({"cursor":position+offset,"max_bytes":1}),
        )
        .await;
        assert_eq!(
            STANDARD
                .decode(structured(&result)["data_base64"].as_str().unwrap())
                .unwrap(),
            [expected]
        );
        assert_eq!(structured(&result)["next_cursor"], position + offset + 1);
    }

    let mut stale = identity(&main).unwrap();
    stale["id"] = json!(main["id"].as_u64().unwrap() + 1);
    let rejected = tool(
        &backend,
        "ztty.session_write",
        json!({"session":main["uri"],"expected_session":stale,"data":"touch stale-wrote\n"}),
    )
    .await;
    assert_eq!(rejected["isError"], true);
    assert!(!root.join("stale-wrote").exists());
    assert!(
        rejected["content"].is_array(),
        "keep the original native error envelope"
    );

    let opened = tool(&backend, "terminal.open", json!({"name":"second"})).await;
    let second = structured(&opened).clone();
    assert_ne!(second["pid"], main["pid"]);
    let second_window = second["window_id"].as_u64().unwrap();
    assert!(mapped(second_window).await);
    // Evict into a multibyte UTF-8 character at the 256-KiB native retention
    // boundary. exec leaves no trailing shell prompt that could shift it again.
    let flood = "stty -echo; exec python3 -c 'import sys; sys.stdout.buffer.write(b\"x\\xf0\\x9f\\x8c\\x8d\" + b\"y\" * (262144 - 2))'\n";
    tool(
        &backend,
        "terminal.write",
        json!({"terminal_id":second["terminal_id"],"data":flood}),
    )
    .await;
    let second_identity = second["session_identity"].clone();
    let evicted = timeout(Duration::from_secs(5), async {
        loop {
            let result = tool(&backend, "ztty.session_read", json!({"session":second["session"],"expected_session":second_identity,"cursor":0,"max_bytes":1,"wait_ms":0})).await;
            let result = structured(&result);
            if result["output_eof"] == true { break result.clone(); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let retained = output_page(&evicted, 0).unwrap();
    assert_eq!(retained.bytes, b"y");
    assert_eq!(retained.start, retained.first + 2);
    let compatibility = tool(
        &backend,
        "terminal.read",
        json!({"terminal_id":second["terminal_id"],"cursor":0,"max_bytes":1}),
    )
    .await;
    assert_eq!(structured(&compatibility)["data_base64"], "eQ==");
    assert_eq!(structured(&compatibility)["next_cursor"], retained.next);
    assert_eq!(structured(&compatibility)["start_cursor"], retained.first);
    assert_eq!(structured(&compatibility)["truncated"], true);
    tool(
        &backend,
        "terminal.close",
        json!({"terminal_id":second["terminal_id"]}),
    )
    .await;
    assert!(!mapped(second_window).await);
    assert_eq!(
        structured(&tool(&backend, "terminal.status", json!({})).await)["running"],
        true
    );

    tool(&backend, "terminal.resize", json!({"rows":30,"cols":100})).await;
    backend.terminals().resize_all(20, 60).await.unwrap();
    let current = tool(&backend, "ztty.session_list", json!({})).await;
    let current = structured(&current)["sessions"][0].clone();
    assert_eq!(current["cols"], 100);
    assert_eq!(current["rows"], 30);
    if let Some(path) = std::env::var_os("SSMCP_TEST_ZTTY_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&json!({"session":current,"binary":program,"lossless_bytes":true,"single_byte_cursor_reads":true,"evicted_utf8_cursor":true,"native_error_preserved":true,"second_window_closed":true})).unwrap()).unwrap();
    }
    if let (Some(script), Some(path)) = (
        std::env::var_os("SSMCP_TEST_ZTTY_CAPTURE"),
        std::env::var_os("SSMCP_TEST_ZTTY_PNG"),
    ) {
        let result = Command::new("python3")
            .arg(script)
            .arg(window.to_string())
            .arg(path)
            .kill_on_drop(true)
            .status()
            .await
            .unwrap();
        assert!(result.success());
    }
    backend.shutdown().await;
    assert!(!mapped(window).await);
    for pid in [
        main["pid"].as_u64().unwrap(),
        main["broker_pid"].as_u64().unwrap(),
        second["pid"].as_u64().unwrap(),
    ] {
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "owned process {pid} was not reaped"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

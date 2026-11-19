//! A downstream binary must opt in to its internal worker entry before a
//! Computer can start a copy of that binary. This test intentionally does not.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires Xvfb, xdotool and dbus-run-session; creates only a private display"]
async fn missing_worker_entry_is_recoverable_without_spawning_the_embedding_binary() {
    let computer = ssmcp::computer::Computer::new(std::env::temp_dir()).unwrap();
    computer
        .perform(
            "ssmcp.computer.desktop_start",
            serde_json::json!({"width":640,"height":480}),
        )
        .await
        .unwrap();
    for _ in 0..2 {
        let error = computer
            .perform("ssmcp.computer.move", serde_json::json!({"x":12,"y":13}))
            .await
            .unwrap_err();
        assert!(
            error.contains("must call computer::worker_entry()"),
            "{error}"
        );
        assert!(error.contains("no worker was started"), "{error}");
    }
    computer.shutdown().await.unwrap();
}

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn gui_gateway_private_attach_round_trip_uses_real_lease_and_native_state() {
    use std::os::unix::fs::PermissionsExt;
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = Directory(std::env::temp_dir().join(format!(
        "ssmcp-gui-private-{}-{}",
        std::process::id(),
        crate::session::random_suffix().unwrap()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let instance = crate::session::random_suffix().unwrap();
    let live = directory.0.join("live");
    let entry = live.join(&instance);
    crate::platform::private_directory(&live).unwrap();
    crate::platform::private_directory(&entry).unwrap();
    crate::session::write_json(
        &entry.join("overview.json"),
        &json!({
            "format": 1, "instance": instance, "pid":std::process::id(), "process_start_ticks":null,
            "metadata":{"name":"private-attach-fixture","cwd":"/workspace","hostname":"fixture"}
        }),
    )
    .unwrap();
    let server = crate::session::ipc::Server::bind(&entry, super::super::ENDPOINT)
        .await
        .unwrap();
    let capture = Arc::new(Capture::new().unwrap());
    let observe_started = Arc::new(tokio::sync::Notify::new());
    let resume_events = Arc::new(tokio::sync::Notify::new());
    let observed = observe_started.clone();
    let resumed = resume_events.clone();
    let serving = tokio::spawn(async move {
        loop {
            let Ok(mut peer) = server.accept().await else {
                break;
            };
            let capture = capture.clone();
            let observed = observed.clone();
            let resumed = resumed.clone();
            tokio::spawn(async move {
                let Ok(request) = peer.read().await else {
                    return;
                };
                if request["op"] == "bridge"
                    && request["request"]["operation"]["op"] == "events"
                    && request["request"]["operation"]["wait_ms"]
                        .as_u64()
                        .is_some_and(|wait| wait > 0)
                {
                    observed.notify_one();
                    resumed.notified().await;
                }
                let reply = super::super::handle_request(&capture, request)
                    .await
                    .map_err(|e| e.to_string());
                let _ = peer.reply(reply).await;
            });
        }
    });
    crate::session::gui::exercise_gateway_fixture(
        directory.0.clone(),
        instance,
        observe_started,
        resume_events,
    )
    .await;
    serving.abort();
    let _ = serving.await;
}

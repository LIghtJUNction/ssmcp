//! Native browser input through the real SSMCP authorization and worker path.
//! Only an owned headless Chromium profile and the unit-test input store are used.
use super::*;
use std::path::Path;
use tokio::task::JoinHandle;
use zuse::browser::Browser;

const FIXTURE: &str = r#"<!doctype html><meta charset="utf-8">
<title>Owned SSMCP click-watch fixture</title>
<style>body{margin:0}#arena{position:absolute;left:32px;top:32px;width:320px;height:200px}
button{width:140px;height:70px}</style>
<div id="arena"><button class="target" hidden>Owned target</button></div>
<script>
const button = document.querySelector('button');
globalThis.fixture = {events: [], clicks: [], round: 0,
  show() { this.round++; button.hidden = false; },
  hide() { button.hidden = true; }
};
for (const type of ['mousedown', 'mouseup', 'click']) {
  document.addEventListener(type, event => {
    fixture.events.push({type, trusted: event.isTrusted, round: fixture.round,
      target: event.target === button});
    if (type === 'click') {
      fixture.clicks.push({trusted: event.isTrusted, round: fixture.round,
        target: event.target === button});
      button.hidden = true;
    }
  }, true);
}
</script>"#;

struct FixtureServer {
    origin: String,
    task: JoinHandle<()>,
}

impl FixtureServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let handled = async {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if socket.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        request.push(byte[0]);
                        assert!(request.len() <= 8192);
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{FIXTURE}",
                        FIXTURE.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                };
                let _ = tokio::time::timeout(Duration::from_secs(2), handled).await;
            }
        });
        Self { origin, task }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct OwnedChromium {
    process: Managed,
    endpoint: String,
}

impl OwnedChromium {
    async fn start(root: &Path) -> Self {
        let executable = std::env::var_os("ZUSE_BROWSER")
            .map(PathBuf::from)
            .or_else(|| {
                ["chromium", "chromium-browser", "google-chrome"]
                    .iter()
                    .find_map(|name| crate::program::executable(name).ok())
            })
            .expect("this ignored native test requires sandboxed Chromium or ZUSE_BROWSER");
        let profile = root.join("chromium-profile");
        let log = root.join("chromium.stderr");
        let mut command = tokio::process::Command::new(executable);
        command
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .env("XDG_RUNTIME_DIR", root)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        let mut process = Managed::spawn(&mut command).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let endpoint = loop {
            if let Ok(text) = std::fs::read_to_string(profile.join("DevToolsActivePort")) {
                let mut lines = text.lines();
                let port: u16 = lines.next().unwrap().parse().unwrap();
                let path = lines.next().unwrap();
                assert!(path.starts_with("/devtools/browser/"));
                break format!("ws://127.0.0.1:{port}{path}");
            }
            let exited = process.child.try_wait().unwrap();
            assert!(
                exited.is_none() && tokio::time::Instant::now() < deadline,
                "owned Chromium did not start: {exited:?}\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        Self { process, endpoint }
    }
}

async fn wait_for_page(browser: &mut Browser, expression: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let actual = browser.evaluate(expression).unwrap();
        if actual == "true" {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owned fixture condition timed out: {expression}: {actual}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn page_value(browser: &mut Browser, expression: &str) -> Value {
    serde_json::from_str(&browser.evaluate(expression).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires installed sandboxed Chromium; uses only an owned profile and temporary host-input state"]
async fn owned_chromium_watch_uses_host_ticket_and_stops_after_off() {
    let directory = CaptureDirectory::new().unwrap();
    let server = FixtureServer::start().await;
    let mut chromium = OwnedChromium::start(&directory.0).await;
    let mut browser = Browser::connect(&chromium.endpoint).unwrap();
    let url = format!("{}/owned-fixture", server.origin);
    browser.navigate(&url).unwrap();
    assert_eq!(
        browser
            .evaluate("document.hasFocus() && document.visibilityState === 'visible'")
            .unwrap(),
        "true"
    );
    let tabs: Value = serde_json::from_str(&browser.tabs().unwrap()).unwrap();
    let matches: Vec<_> = tabs["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tab| tab["type"] == "page" && tab["url"] == url)
        .collect();
    assert_eq!(matches.len(), 1);
    let tab_id = matches[0]["targetId"].as_str().unwrap();
    let tab_count = tabs["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tab| tab["type"] == "page")
        .count();

    // HostControl::default uses Store::temporary under cfg(test). This is the
    // normal host route and worker, without reading or changing the user's switch.
    let computer = Computer::new(directory.0.clone()).unwrap();
    computer.set_host_input_local(true).unwrap();
    let mut args = json!({"desktop":"host","endpoint":chromium.endpoint,
        "tab_id":tab_id,"scope":"#arena","selector":"button.target",
        "expected_origin":server.origin,"max_clicks":1,"timeout_ms":10000});
    let installed = "globalThis[Symbol.for('zuse.observation.v1')]?.size === 1";
    let drive = async {
        wait_for_page(&mut browser, installed).await;
        browser.evaluate("fixture.show();true").unwrap();
    };
    let (result, ()) = tokio::join!(
        computer.perform("ssmcp.computer.watch_until_click", args.clone()),
        drive
    );
    let result = result.unwrap();
    assert_ne!(result["isError"], true, "{result}");
    let receipt = &result["structuredContent"];
    assert_eq!(receipt["status"], "completed", "{receipt}");
    assert_eq!(receipt["backend"], "cdp");
    assert_eq!(receipt["desktop"], "host");
    assert_eq!(receipt["input_scope"], "browser_page");
    assert_eq!(receipt["moves_host_pointer"], false);
    assert_eq!(receipt["confirmed_clicks"], 1);
    assert_eq!(receipt["dispatch_attempts"], 1);
    assert_eq!(receipt["screenshots"], 0);
    assert_eq!(receipt["clicks"][0]["dispatch"]["sent_mouse_events"], 2);
    assert_eq!(receipt["clicks"][0]["dispatch"]["release_confirmed"], true);
    assert_eq!(receipt["clicks"][0]["received"]["trusted"], true);
    assert_eq!(browser.evaluate("fixture.clicks.length === 1 && fixture.events.length === 3 && fixture.events.every(event => event.trusted && event.target)").unwrap(), "true");
    assert_eq!(
        browser
            .evaluate("globalThis[Symbol.for('zuse.observation.v1')].size")
            .unwrap(),
        "0"
    );

    args["max_clicks"] = json!(64);
    args["timeout_ms"] = json!(30000);
    let revoke = async {
        wait_for_page(&mut browser, installed).await;
        browser.evaluate("fixture.show();true").unwrap();
        wait_for_page(&mut browser, "fixture.clicks.length === 2").await;
        assert_eq!(
            computer.set_host_input_local(false).unwrap()["enabled"],
            false
        );
        let before = page_value(&mut browser, "fixture.events");
        // These synchronous page calls keep the parent operation future from
        // processing EOF yet. The child must consult the revoked original
        // Ticket even while its DOM observer and stdin are still live.
        browser.evaluate("fixture.show();true").unwrap();
        let after = page_value(
            &mut browser,
            "new Promise(resolve => setTimeout(() => resolve(fixture.events), 150))",
        );
        assert_eq!(
            after, before,
            "a new visible target received input after local off"
        );
        before
    };
    let (result, after_off) = tokio::join!(
        computer.perform("ssmcp.computer.watch_until_click", args.clone()),
        revoke
    );
    assert_eq!(result.unwrap_err(), host_control::ENDED);
    assert!(!HOST_INPUT_CLEANUP_FAILED.load(Ordering::Acquire));
    computer.releases.wait(RELEASE_WAIT_TIMEOUT).await.unwrap();
    assert_eq!(
        browser
            .evaluate("globalThis[Symbol.for('zuse.observation.v1')].size")
            .unwrap(),
        "0"
    );
    let later = page_value(
        &mut browser,
        "new Promise(resolve => {fixture.hide();setTimeout(() => fixture.show(), 40);setTimeout(() => resolve(fixture.events), 200)})",
    );
    assert_eq!(
        later, after_off,
        "the completed worker left an input-producing callback"
    );
    assert_eq!(
        computer
            .perform("ssmcp.computer.watch_until_click", args)
            .await
            .unwrap_err(),
        host_control::LOCKED
    );
    assert_eq!(browser.evaluate("fixture.clicks.length === 2 && fixture.events.length === 6 && fixture.events.every(event => event.trusted && event.target)").unwrap(), "true");
    let after: Value = serde_json::from_str(&browser.tabs().unwrap()).unwrap();
    assert_eq!(
        after["targetInfos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tab| tab["type"] == "page")
            .count(),
        tab_count
    );
    println!(
        "owned Chromium via SSMCP: 2 trusted clicks; release acknowledged; zero new mouse events after host off; worker cleanup confirmed; user tabs and host input untouched"
    );
    computer.shutdown().await.unwrap();
    browser.close().unwrap();
    chromium.process.shutdown().await;
}

//! Forward the installed Playwright MCP server's native extension tools.
//! Discovery never launches a browser, installs packages, or approves a connection.
use super::rpc::{Dialect, Peer};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const MAX_PAGES: usize = 128;
const MAX_TOOLS: usize = 4096;

pub struct Playwright {
    peer: Arc<Peer>,
    server: Value,
    next_progress: AtomicU64,
    background: bool,
}

/// An operator may point at a previously installed CLI entry. Never run npx,
/// package-manager shims, or Playwright's unrelated test MCP server to discover it.
pub fn installed() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SSMCP_PLAYWRIGHT_MCP") {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            return Err(
                "SSMCP_PLAYWRIGHT_MCP must name an existing absolute Playwright MCP CLI entry"
                    .into(),
            );
        }
        return path.canonicalize().map_err(|e| e.to_string());
    }
    if let Some(path) = crate::client::browser_settings()
        .map_err(|e| e.to_string())?
        .playwright_mcp
    {
        if !path.is_absolute() || !path.is_file() {
            return Err("browser.playwright_mcp must name an existing absolute Playwright MCP CLI entry; run ssmcp setup browser --install".into());
        }
        return path.canonicalize().map_err(|e| e.to_string());
    }
    crate::program::executable("playwright-mcp").map_err(|_| {
        "Playwright MCP server is not installed in PATH; the Chrome extension alone is not a callable server. Set SSMCP_PLAYWRIGHT_MCP to an already-installed CLI entry; discovery never downloads packages".into()
    })
}

fn package_dispatcher(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let name = [".js", ".cjs", ".mjs", ".exe", ".com", ".cmd", ".bat"]
        .iter()
        .find_map(|suffix| name.strip_suffix(*suffix))
        .unwrap_or(&name);
    matches!(
        name,
        "npx"
            | "npx-cli"
            | "npm"
            | "npm-cli"
            | "corepack"
            | "pnpm"
            | "pnpx"
            | "yarn"
            | "yarnpkg"
            | "mise"
            | "asdf"
            | "volta"
            | "bun"
            | "bunx"
            | "uv"
            | "uvx"
    )
}

pub async fn command(path: &Path) -> Result<Command, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        // Detect an accidentally selected known downloader even through a
        // renamed symlink. An explicit JavaScript CLI remains operator-trusted
        // code; this is not a static audit of arbitrary program behavior.
        if package_dispatcher(&path) {
            return Err("Playwright MCP entry resolves to a known package-manager dispatcher; discovery will not execute it".into());
        }
        let mut prefix = Vec::new();
        std::fs::File::open(&path).map_err(|e| e.to_string())?
            .take(4096).read_to_end(&mut prefix).map_err(|e| e.to_string())?;
        let header = String::from_utf8_lossy(&prefix);
        let javascript = matches!(path.extension().and_then(|s| s.to_str()), Some("js" | "cjs" | "mjs"))
            || header.lines().next().is_some_and(|line| line.starts_with("#!") && line.split_whitespace().any(|s| s == "node" || s.ends_with("/node")));
        let mut command = if javascript {
            // The real installed Node is validated, but the MCP server itself
            // does not run with the code-mode sandbox's small memory limit.
            let mut command = Command::new(crate::program::node()?);
            command.arg(path);
            command
        } else if prefix.starts_with(b"\x7fELF") || prefix.starts_with(b"MZ")
            || prefix.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
            || prefix.starts_with(&[0xfe, 0xed, 0xfa, 0xcf])
            || prefix.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
            Command::new(path)
        } else {
            return Err("Playwright MCP entry must be its native executable or JavaScript CLI; package-manager and shell wrappers are not executed during discovery".into());
        };
        crate::program::configure(&mut command);
        command.env_remove("NODE_OPTIONS").env_remove("NODE_PATH");
        Ok(command)
    }).await.map_err(|e| format!("Playwright runtime preparation failed: {e}"))?
}

/// Probe the installed CLI with bounded output and owned process cleanup.
async fn background_supported(path: &Path) -> Result<(), String> {
    let mut command = command(path).await?;
    command
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = super::process::Managed::spawn(&mut command)
        .map_err(|e| format!("Playwright background capability check: {e}"))?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("Playwright help stdout unavailable")?
        .take(65537);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("Playwright help stderr unavailable")?
        .take(65537);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let checked = tokio::time::timeout(Duration::from_secs(5), async {
        let (a, b, status) = tokio::join!(
            stdout.read_to_end(&mut out), stderr.read_to_end(&mut err), process.wait()
        );
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        let status = status.map_err(|e| e.to_string())?;
        if out.len() > 65536 || err.len() > 65536 {
            return Err("Playwright help output exceeds 64 KiB".to_owned());
        }
        if !status.success() {
            return Err(format!("Playwright --help failed ({status})"));
        }
        if !help_has_background(&out) && !help_has_background(&err) {
            return Err("Installed Playwright MCP does not support --background; run ssmcp setup browser --install to install a version with background control. Foreground fallback is disabled; set browser.background=false only when foreground control is intended.".to_owned());
        }
        Ok(())
    }).await.map_err(|_| "Playwright background capability check timed out".to_owned()).and_then(|v| v);
    process.shutdown().await;
    checked
}

fn help_has_background(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .any(|word| word == "--background")
}

impl Playwright {
    pub async fn start_with_output(
        path: &Path,
        output: Arc<crate::output::OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        let mut command = command(path).await?;
        let background = crate::client::browser_settings()?.background;
        if background {
            background_supported(path).await?;
        }
        // Existing profiles stay under the extension's native approval. An old
        // provider must never silently fall back to taking the user's focus.
        command.arg("--extension").env(
            "PLAYWRIGHT_MCP_BACKGROUND",
            if background { "1" } else { "0" },
        );
        if background {
            command.arg("--background");
        }
        let peer = Peer::spawn_with_output_and_audit(
            command,
            "playwright",
            Dialect::JsonRpc,
            output,
            super::events::AuditMode::MetadataOnly,
        )
        .await?;
        Self::initialize(peer, background).await.map_err(Into::into)
    }

    async fn initialize(peer: Arc<Peer>, background: bool) -> Result<Arc<Self>, String> {
        let initialized = async {
            let value = peer
                .request_discovery(
                    "initialize",
                    json!({
                        "protocolVersion":"2025-11-25", "capabilities":{},
                        "clientInfo":{"name":"ssmcp","version":env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            if !matches!(
                value["protocolVersion"].as_str(),
                Some("2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25")
            ) || !value["capabilities"]["tools"].is_object()
            {
                return Err(
                    "Playwright did not negotiate a supported native MCP tools protocol".to_owned(),
                );
            }
            peer.notify("notifications/initialized", json!({})).await?;
            Ok(value)
        }
        .await;
        match initialized {
            Ok(server) => Ok(Arc::new(Self {
                peer,
                server,
                next_progress: AtomicU64::new(1),
                background,
            })),
            Err(error) => {
                peer.shutdown().await;
                Err(error)
            }
        }
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = control_tools();
        let mut names = HashSet::new();
        let mut cursors = HashSet::new();
        let mut params = json!({});
        for _ in 0..MAX_PAGES {
            let page = self.peer.request_discovery("tools/list", params).await?;
            let entries = page["tools"]
                .as_array()
                .ok_or("Playwright tools/list returned no tools array")?;
            for entry in entries {
                let name = entry["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("Playwright returned a tool without a name")?;
                if !names.insert(name.to_owned()) {
                    return Err(format!("Playwright returned duplicate tool name {name:?}"));
                }
                if names.len() > MAX_TOOLS {
                    return Err("Playwright exceeded the native tool limit".into());
                }
                let mut entry = entry.clone();
                // Only add the routing namespace. Keep descriptions, $defs,
                // annotations, outputSchema and future native fields intact.
                entry["name"] = json!(format!("playwright.{name}"));
                tools.push(entry);
            }
            match page.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(cursor)) if !cursor.is_empty() => {
                    if !cursors.insert(cursor.clone()) {
                        return Err("Playwright repeated a tools/list cursor".into());
                    }
                    params = json!({"cursor":cursor});
                }
                _ => return Err("Playwright returned an invalid tools/list cursor".into()),
            }
        }
        Err("Playwright exceeded the tools/list pagination limit".into())
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "ssmcp.playwright.events" => {
                Ok(super::result(self.peer.events().read(arguments).await?))
            }
            "ssmcp.playwright.requests" => {
                if !arguments.as_object().is_some_and(|v| v.is_empty()) {
                    return Err("requests takes no arguments".into());
                }
                Ok(super::result(self.peer.pending().await))
            }
            "ssmcp.playwright.respond" => Ok(super::result(self.peer.respond(arguments).await?)),
            _ => {
                let native = name
                    .strip_prefix("playwright.")
                    .filter(|s| !s.is_empty())
                    .ok_or("unknown Playwright route")?;
                self.peer.request("tools/call", json!({"name":native,"arguments":arguments,
                    "_meta":{"progressToken":format!("ssmcp-playwright-{}",self.next_progress.fetch_add(1,Ordering::Relaxed))}
                })).await
            }
        }
    }

    pub fn diagnostics(&self) -> Value {
        json!({"scope":"native Playwright MCP tools", "browser_mode":"existing_browser_extension",
            "browser_connection":"on_demand_with_native_extension_approval",
            "background":self.background,"input_scope":"browser_target","moves_system_pointer":false,
            "focus_policy":if self.background {"background_after_native_pairing"} else {"explicit_foreground"},
            "pairing_note":"Initial extension approval may open a foreground page; background mode does not bypass native approval.",
            "serverInfo":self.server.get("serverInfo"),"protocolVersion":self.server["protocolVersion"],
            "browser_control":"not_checked_by_discovery"})
    }

    pub async fn shutdown(&self) {
        self.peer.shutdown().await;
    }
}

fn control_tools() -> Vec<Value> {
    vec![
        json!({"name":"ssmcp.playwright.events","description":"Read native Playwright MCP events and progress. Reuse next_cursor; discovery does not imply an approved browser connection.","inputSchema":crate::backend::events::schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
        json!({"name":"ssmcp.playwright.requests","description":"List pending native server requests without approving them. Browser extension approval remains in the extension UI.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
        json!({"name":"ssmcp.playwright.respond","description":"Respond to one reviewed pending native MCP request by its exact id. This does not replace the browser extension's connection approval.","inputSchema":{"type":"object","properties":{"id":{"type":["string","integer"]},"result":{},"error":{"type":"object","properties":{"code":{"type":"integer"},"message":{"type":"string"},"data":{}},"required":["code","message"]}},"required":["id"],"additionalProperties":false}}),
    ]
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const SERVER: &str = r#"
import sys,json
for line in sys.stdin:
 r=json.loads(line); method=r.get('method'); p=r.get('params',{}); ident=r.get('id')
 if ident is None: continue
 if method=='initialize': out={'protocolVersion':'2025-11-25','capabilities':{'tools':{'listChanged':True}},'serverInfo':{'name':'fixture','version':'1'}}
 elif method=='tools/list':
  if p.get('cursor')=='second': out={'tools':[{'name':'native_second','description':'native two','inputSchema':{'type':'object'}}]}
  else: out={'tools':[{'name':'native_first','description':'native one','inputSchema':{'type':'object','$defs':{'p':{'type':'string'}},'properties':{'p':{'$ref':'#/$defs/p'}}},'outputSchema':{'type':'object'},'annotations':{'readOnlyHint':True},'_native':{'keep':True}}],'nextCursor':'second'}
 elif method=='tools/call': out={'content':[{'type':'text','text':'native result'}],'structuredContent':{'called':p['name'],'arguments':p['arguments']},'isError':p['arguments'].get('fail',False),'_native':['keep',None]}
 else: out={}
 print(json.dumps({'jsonrpc':'2.0','id':ident,'result':out}),flush=True)
"#;

    #[tokio::test]
    async fn native_discovery_and_call_preserve_complete_protocol_values() {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", SERVER]);
        let peer = Peer::spawn(command, "playwright-fixture", Dialect::JsonRpc)
            .await
            .unwrap();
        let bridge = Playwright::initialize(peer, true).await.unwrap();
        let tools = bridge.tools().await.unwrap();
        assert_eq!(tools.len(), 5);
        let first = tools
            .iter()
            .find(|t| t["name"] == "playwright.native_first")
            .unwrap();
        assert_eq!(first["inputSchema"]["properties"]["p"]["$ref"], "#/$defs/p");
        assert_eq!(first["inputSchema"]["$defs"]["p"]["type"], "string");
        assert_eq!(first["outputSchema"], json!({"type":"object"}));
        assert_eq!(first["_native"], json!({"keep":true}));
        assert_eq!(first["annotations"], json!({"readOnlyHint":true}));
        for fail in [false, true] {
            let args = json!({"p":"现有 Chrome、工具", "fail":fail});
            let reply = bridge
                .call("playwright.native_first", args.clone())
                .await
                .unwrap();
            assert_eq!(
                reply,
                json!({"content":[{"type":"text","text":"native result"}],"structuredContent":{"called":"native_first","arguments":args},"isError":fail,"_native":["keep",null]})
            );
        }
        assert!(
            bridge.peer.pending().await["requests"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        bridge.shutdown().await;
    }

    async fn fixture(script: &str) -> Arc<Playwright> {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", script]);
        let peer = Peer::spawn(command, "playwright-fixture", Dialect::JsonRpc)
            .await
            .unwrap();
        Playwright::initialize(peer, true).await.unwrap()
    }

    #[tokio::test]
    async fn discovery_replaces_changed_tools_and_rejects_repeated_cursors() {
        let script = r#"
import sys,json
revision=0
for line in sys.stdin:
 r=json.loads(line); method=r.get('method'); ident=r.get('id'); p=r.get('params',{})
 if ident is None: continue
 if method=='initialize': out={'protocolVersion':'2025-11-25','capabilities':{'tools':{'listChanged':True}},'serverInfo':{'name':'fixture','version':'1'}}
 elif method=='tools/list':
  revision+=1
  name='before' if revision==1 else 'after'
  out={'tools':[{'name':name,'inputSchema':{'type':'object'},'_native_revision':revision}]}
 else: out={}
 print(json.dumps({'jsonrpc':'2.0','id':ident,'result':out}),flush=True)
"#;
        let bridge = fixture(script).await;
        let first = bridge.tools().await.unwrap();
        let second = bridge.tools().await.unwrap();
        assert!(first.iter().any(|tool| tool["name"] == "playwright.before"));
        assert!(
            second
                .iter()
                .any(|tool| tool["name"] == "playwright.after" && tool["_native_revision"] == 2)
        );
        assert!(
            !second
                .iter()
                .any(|tool| tool["name"] == "playwright.before")
        );
        bridge.shutdown().await;
        let repeated = script.replace(
            "out={'tools':[{'name':name,'inputSchema':{'type':'object'},'_native_revision':revision}]}",
            "out={'tools':[], 'nextCursor':'same'}",
        );
        let bridge = fixture(&repeated).await;
        assert!(
            bridge
                .tools()
                .await
                .unwrap_err()
                .contains("repeated a tools/list cursor")
        );
        bridge.shutdown().await;
    }

    #[tokio::test]
    async fn native_permission_request_stays_pending_until_an_explicit_response() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let script = r#"
import sys,json
pending=None
for line in sys.stdin:
 r=json.loads(line); method=r.get('method'); ident=r.get('id')
 if ident is None: continue
 if method=='initialize':
  assert r['params']['capabilities']=={}
  out={'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
 elif method=='tools/call':
  pending=ident
  print(json.dumps({'jsonrpc':'2.0','id':'native-approval','method':'elicitation/create','params':{'mode':'form','message':'Fixture approval','requestedSchema':{'type':'object','properties':{'confirmed':{'type':'boolean'}}}}}),flush=True)
  continue
 elif ident=='native-approval':
  assert method is None and r['result']=={'action':'decline'}
  print(json.dumps({'jsonrpc':'2.0','id':pending,'result':{'content':[{'type':'text','text':'declined'}],'isError':True,'_native_response':r['result']}}),flush=True)
  continue
 else: out={}
 print(json.dumps({'jsonrpc':'2.0','id':ident,'result':out}),flush=True)
"#;
            let bridge = fixture(script).await;
            let call = tokio::spawn({
                let bridge = bridge.clone();
                async move { bridge.call("playwright.native_permission", json!({})).await }
            });
            let pending = loop {
                let pending = bridge.call("ssmcp.playwright.requests", json!({})).await.unwrap();
                if pending["structuredContent"]["requests"].as_array().unwrap().len() == 1 {
                    break pending;
                }
                assert!(!call.is_finished(), "native request completed without an operator response");
                tokio::task::yield_now().await;
            };
            assert_eq!(pending["structuredContent"]["requests"][0], json!({
                "jsonrpc":"2.0","id":"native-approval","method":"elicitation/create",
                "params":{"mode":"form","message":"Fixture approval","requestedSchema":{
                    "type":"object","properties":{"confirmed":{"type":"boolean"}}
                }}
            }));
            assert!(!call.is_finished());
            let response = bridge.call("ssmcp.playwright.respond", json!({
                "id":"native-approval","result":{"action":"decline"}
            })).await.unwrap();
            assert_eq!(response["structuredContent"]["sent"], true);
            assert_eq!(call.await.unwrap().unwrap(), json!({
                "content":[{"type":"text","text":"declined"}],"isError":true,
                "_native_response":{"action":"decline"}
            }));
            assert!(bridge.peer.pending().await["requests"].as_array().unwrap().is_empty());
            bridge.shutdown().await;
        }).await.expect("native approval fixture stalled");
    }

    #[tokio::test]
    async fn known_package_dispatchers_and_shell_wrappers_are_never_executed() {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let mut nonce = [0u8; 16];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory =
            Directory(std::env::temp_dir().join(format!("ssmcp-playwright-entry-{nonce}")));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory.0)
            .unwrap();
        let marker = directory.0.join("executed");
        let script = format!(
            "#!/usr/bin/env node\nrequire('node:fs').writeFileSync({},'executed');\n",
            json!(marker)
        );
        let output = crate::output::OutputStore::new_connection().unwrap();
        for name in [
            "npx-cli.js",
            "npm-cli.cjs",
            "COREPACK.EXE",
            "pnpm.cmd",
            "yarn.mjs",
            "mise",
            "asdf",
            "volta",
            "bunx",
            "uvx",
        ] {
            let path = directory.0.join(name);
            std::fs::write(&path, &script).unwrap();
            let error = Playwright::start_with_output(&path, output.clone())
                .await
                .err()
                .expect("known dispatcher was accepted");
            assert!(
                error
                    .to_string()
                    .contains("known package-manager dispatcher"),
                "{name}: {error}"
            );
            assert!(!marker.exists(), "{name} was executed");
        }
        let alias = directory.0.join("playwright-mcp.js");
        symlink(directory.0.join("npx-cli.js"), &alias).unwrap();
        assert!(
            command(&alias)
                .await
                .err()
                .unwrap()
                .contains("known package-manager dispatcher")
        );
        let shell = directory.0.join("playwright-mcp");
        std::fs::write(&shell, "#!/bin/sh\nprintf ran > executed\n").unwrap();
        assert!(
            command(&shell)
                .await
                .err()
                .unwrap()
                .contains("shell wrappers")
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn background_probe_accepts_supported_cli_and_rejects_foreground_only_cli() {
        if crate::program::node().is_err() {
            return; // The package supports clients without optional Node installed.
        }
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let mut nonce = [0u8; 16];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let directory =
            Directory(std::env::temp_dir().join(format!("ssmcp-background-probe-{nonce}")));
        std::fs::create_dir(&directory.0).unwrap();
        let entry = directory.0.join("provider.cjs");
        std::fs::write(&entry,
            "if(process.argv[2]!=='--help')process.exit(7);console.log('--extension --background-color');"
        ).unwrap();
        let error = background_supported(&entry).await.unwrap_err();
        assert!(error.contains("does not support --background"));
        assert!(error.contains("ssmcp setup browser --install"));
        std::fs::write(&entry,
            "if(process.argv[2]!=='--help')process.exit(7);console.log('  --background  keep tabs inactive');"
        ).unwrap();
        background_supported(&entry).await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_protocol_is_not_registered() {
        let script = SERVER.replace("'2025-11-25'", "'2099-01-01'");
        let mut command = Command::new("python3");
        command.args(["-u", "-c", &script]);
        let peer = Peer::spawn(command, "playwright-fixture", Dialect::JsonRpc)
            .await
            .unwrap();
        assert!(Playwright::initialize(peer, true).await.is_err());
    }
}

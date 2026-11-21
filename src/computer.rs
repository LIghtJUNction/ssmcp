//! Lifecycle management for the independent Zuse CLI.
//!
//! SSMCP does not execute Zuse programs or provide desktop/browser actions.
//! Installations are private and receipt-bound; external PATH entries are never
//! replaced, downgraded, or removed.
mod archive;
mod probe;
mod release;
mod storage;

#[cfg(test)]
mod tests;

use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

pub struct Computer {
    root: PathBuf,
    operation: Mutex<()>,
    closed: AtomicBool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadArgs {
    version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallArgs {
    download_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UninstallArgs {
    installation_id: String,
}

impl Computer {
    /// Construction has no process, network, or filesystem-write side effects.
    pub fn new(_workspace: PathBuf) -> Result<Self, String> {
        Ok(Self {
            root: storage::default_root()?,
            operation: Mutex::new(()),
            closed: AtomicBool::new(false),
        })
    }

    pub fn tools() -> Vec<Value> {
        vec![
            tool(
                "version",
                "Query the actual standalone Zuse CLI with a bounded --version. Existing PATH installations are reported without taking ownership or changing them.",
                json!({}),
                &[],
                true,
                false,
            ),
            tool(
                "download",
                "Download an exact official LIghtJUNction/zuse stable release asset for this platform. Requires its published SHA256 and size. Returns a download_id; does not execute or install it.",
                json!({"version":{"type":"string","maxLength":49,"description":"Optional stable MAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH. Omit for the latest published stable release."}}),
                &[],
                false,
                false,
            ),
            tool(
                "install",
                "Verify a managed download and atomically install only its Zuse executable in private storage. Returns an installation_id and path. Does not replace an external Zuse or add anything to PATH.",
                json!({"download_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}}),
                &["download_id"],
                false,
                false,
            ),
            tool(
                "uninstall",
                "Remove only a private Zuse installation whose ownership receipt, file identity and bytes still match this manager. External, Cargo and system installations are never eligible.",
                json!({"installation_id":{"type":"string","pattern":"^[0-9a-f]{32}$"}}),
                &["installation_id"],
                false,
                true,
            ),
        ]
    }

    pub async fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        self.perform_with_child_diagnostics(name, args, true).await
    }

    pub(crate) async fn perform_with_child_diagnostics(
        &self,
        name: &str,
        args: Value,
        include_child_stderr: bool,
    ) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("Zuse CLI manager connection is closed".into());
        }
        let _operation = self.operation.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err("Zuse CLI manager connection is closed".into());
        }
        let body = match name {
            "ssmcp.computer.version" => {
                parse::<Empty>(args)?;
                self.version(include_child_stderr).await?
            }
            "ssmcp.computer.download" => {
                let args: DownloadArgs = parse(args)?;
                let (release, bytes) = release::download(args.version.as_deref()).await?;
                let store = self.store()?;
                let receipt = store.save_download(release, &bytes)?;
                json!({
                    "download_id": receipt.download_id, "release": receipt.release,
                    "managed": true, "installed": false
                })
            }
            "ssmcp.computer.install" => {
                let args: InstallArgs = parse(args)?;
                let store = self.store()?;
                let receipt = store.install(&args.download_id)?;
                json!({
                    "installation_id": receipt.installation_id,
                    "download_id": receipt.download_id,
                    "version": receipt.release.version, "path": store.executable(&receipt),
                    "sha256": receipt.binary_sha256, "size": receipt.binary_size,
                    "release": receipt.release, "managed": true, "added_to_path": false
                })
            }
            "ssmcp.computer.uninstall" => {
                let args: UninstallArgs = parse(args)?;
                let store = storage::Store::open(&self.root, false)?
                    .ok_or("This manager has no owned Zuse installations")?;
                let receipt = store.uninstall(&args.installation_id)?;
                json!({
                    "installation_id": receipt.installation_id,
                    "path": store.executable(&receipt), "removed": true, "managed": true
                })
            }
            _ => return Err(format!("Unknown Zuse CLI lifecycle tool: {name}")),
        };
        Ok(crate::backend::result(body))
    }

    fn store(&self) -> Result<storage::Store, String> {
        storage::Store::open(&self.root, true)?
            .ok_or_else(|| "Zuse manager storage is unavailable".into())
    }

    async fn version(&self, include_child_stderr: bool) -> Result<Value, String> {
        // An existing external entry always wins. A broken/unsupported entry is
        // reported as an error instead of silently falling back to another CLI.
        if let Ok(selected) = crate::platform::executable("zuse") {
            let path = crate::program::selected_entry(&selected, "zuse")?;
            let store = storage::Store::open(&self.root, false)?;
            let owned = store
                .as_ref()
                .map(|store| store.matching(&path))
                .transpose()?
                .flatten();
            let mut result = probe::version(&path, include_child_stderr).await?;
            result["managed"] = json!(owned.is_some());
            result["source"] = json!(if owned.is_some() { "ssmcp" } else { "external" });
            result["installation_id"] = json!(owned.map(|receipt| receipt.installation_id));
            return Ok(result);
        }
        let Some(store) = storage::Store::open(&self.root, false)? else {
            return Ok(json!({"available": false, "managed": false, "reason": "not_installed"}));
        };
        let Some(receipt) = store.latest()? else {
            return Ok(json!({"available": false, "managed": false, "reason": "not_installed"}));
        };
        let mut result = probe::version(&store.executable(&receipt), include_child_stderr).await?;
        if result["version"] != receipt.release.version {
            return Err("Managed Zuse --version differs from its official release receipt".into());
        }
        result["managed"] = json!(true);
        result["source"] = json!("ssmcp");
        result["installation_id"] = json!(receipt.installation_id);
        Ok(result)
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::Release);
        let _operation = self.operation.lock().await;
        Ok(())
    }
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args)
        .map_err(|error| format!("Invalid Zuse management arguments: {error}"))
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
    destructive: bool,
) -> Value {
    json!({
        "name": format!("ssmcp.computer.{name}"), "description": description,
        "inputSchema": {"type":"object", "properties":properties, "required":required, "additionalProperties":false},
        "annotations": {"readOnlyHint":read_only, "destructiveHint":destructive, "openWorldHint":name == "download"}
    })
}

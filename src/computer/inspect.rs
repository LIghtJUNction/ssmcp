//! Local, target-bound observations. Each request constructs its own fixed Zuse program.
use super::{Computer, archive::MAX_EXECUTABLE, probe, release, storage};
use crate::computer_control::ObservationPermit;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{future::Future, path::Path, pin::Pin, time::Duration};

const LIMIT: usize = 256 * 1024;
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
pub(crate) enum Request {
    Check,
    Locate {
        window: String,
        label: String,
    },
    Observe {
        port: u16,
        page: String,
        selector: String,
    },
}

impl Request {
    pub(crate) fn locate(window: &str, label: &str) -> Result<Self, String> {
        let id = window.strip_prefix("0x");
        let numeric = match id {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None if window.bytes().all(|b| b.is_ascii_digit()) => window.parse().ok(),
            None => None,
        };
        if !numeric.is_some_and(|id| id > 1) || window.len() > 18 {
            return Err("locate-window requires an explicit native window ID".into());
        }
        bounded_text(label, "accessible label")?;
        Ok(Self::Locate {
            window: window.into(),
            label: label.into(),
        })
    }

    pub(crate) fn observe(port: &str, page: &str, selector: &str) -> Result<Self, String> {
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("Expected a loopback debugging port in 1..65535")?;
        if page.is_empty()
            || page.len() > 128
            || !page
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err("observe-tab requires an exact DevTools page target ID".into());
        }
        bounded_text(selector, "CSS container selector")?;
        Ok(Self::Observe {
            port,
            page: page.into(),
            selector: selector.into(),
        })
    }
}

fn bounded_text(value: &str, name: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        Err(format!(
            "Expected a nonempty {name} of at most 1024 bytes without control characters"
        ))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
struct Schema {
    version: String,
    commands: Vec<Spec>,
}

#[derive(Deserialize)]
struct Spec {
    name: String,
    read_only: bool,
    parameters: Vec<Param>,
}

#[derive(Deserialize)]
struct Param {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    required: bool,
}

impl Schema {
    fn require(&self, name: &str, read_only: bool, params: &[(&str, &str)]) -> Result<(), String> {
        let mut matches = self.commands.iter().filter(|c| c.name == name);
        let compatible = matches.next().is_some_and(|c| {
            c.read_only == read_only
                && matches.next().is_none()
                && params.iter().all(|(n, ty)| {
                    let mut found = c.parameters.iter().filter(|p| p.name == *n);
                    found.next().is_some_and(|p| p.kind == *ty) && found.next().is_none()
                })
                && c.parameters
                    .iter()
                    .all(|p| !p.required || params.iter().any(|(n, _)| *n == p.name))
        });
        if compatible {
            Ok(())
        } else {
            Err(format!("Incompatible Zuse schema for {name}"))
        }
    }

    fn verify(&self, version: &str, request: &Request) -> Result<(), String> {
        // Page WebSocket ownership and target receipts are verified against the 0.4.2 contract.
        let stable = release::version(version)?;
        if stable != version
            || self.version != version
            || !version
                .strip_prefix("0.4.")
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n >= 2)
        {
            return Err(format!(
                "Incompatible Zuse version: executable {version}, schema {}; expected 0.4.x from 0.4.2",
                self.version
            ));
        }
        if !matches!(request, Request::Observe { .. }) {
            self.require(
                "target.locate",
                true,
                &[
                    ("window", "text"),
                    ("text", "text"),
                    ("exact", "boolean"),
                    ("case_sensitive", "boolean"),
                ],
            )?;
        }
        if !matches!(request, Request::Locate { .. }) {
            self.require("browser.connect", false, &[("endpoint", "text")])?;
            self.require("observe", true, &[("source", "text"), ("selector", "text")])?;
            self.require("browser.close", false, &[])?;
        }
        if matches!(request, Request::Check) {
            self.require("timer.start", true, &[])?;
            self.require("timer.status", true, &[])?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Receipt {
    status: String,
    completed: usize,
    results: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    step: usize,
    operation: String,
    value: Value,
}

fn receipt(bytes: &[u8], operations: &[&str]) -> Result<Vec<Value>, String> {
    let result: Receipt =
        serde_json::from_slice(bytes).map_err(|e| format!("Invalid Zuse receipt: {e}"))?;
    if result.status != "completed"
        || result.completed != operations.len()
        || result.results.len() != operations.len()
    {
        return Err("Zuse did not return a complete operation receipt".into());
    }
    result
        .results
        .into_iter()
        .zip(operations)
        .enumerate()
        .map(|(index, (step, name))| {
            if step.step != index || step.operation != *name || !step.value.is_object() {
                return Err(format!(
                    "Unexpected Zuse receipt for {name} at step {index}"
                ));
            }
            Ok(step.value)
        })
        .collect()
}

type Stop<'a> = Pin<&'a mut (dyn Future<Output = String> + Send + 'a)>;

async fn inspect(
    path: &Path,
    version: &str,
    digest: &str,
    request: &Request,
    mut stop: Stop<'_>,
) -> Result<Value, String> {
    let schema = probe::capture(path, &["schema"], None, LIMIT, DEADLINE, stop.as_mut()).await?;
    let schema: Schema =
        serde_json::from_slice(&schema).map_err(|e| format!("Invalid Zuse schema: {e}"))?;
    schema.verify(version, request)?;
    let (_, current) = storage::bytes(path, MAX_EXECUTABLE)?;
    if release::digest(&current) != digest {
        return Err("Selected Zuse executable changed during preflight".into());
    }
    drop(current);
    match request {
        Request::Check => {
            for (argv, input) in [
                (["eval", "timer.status()", "--read-only"], None),
                (["run", "-", "--read-only"], Some("timer.status()")),
            ] {
                let out = probe::capture(path, &argv, input, 8192, DEADLINE, stop.as_mut()).await?;
                if receipt(&out, &["timer.status"])?[0]["running"] != false {
                    return Err(format!("Unexpected timer receipt from Zuse {}", argv[0]));
                }
            }
            let out = probe::capture(
                path,
                &["shell", "--read-only"],
                Some("timer.start()\ntimer.status()\n"),
                8192,
                DEADLINE,
                stop,
            )
            .await?;
            let rows = out
                .trim_ascii_end()
                .split(|b| *b == b'\n')
                .collect::<Vec<_>>();
            if rows.len() != 2
                || receipt(rows[0], &["timer.start"])?[0]["running"] != true
                || receipt(rows[1], &["timer.status"])?[0]["running"] != true
            {
                return Err("Zuse shell did not retain the timer session".into());
            }
            Ok(
                json!({"version": version, "path": path, "schema_commands":schema.commands.len(), "entry_points":["eval","run","shell"],"persistent_shell":true}),
            )
        }
        Request::Locate { window, label } => {
            let source = format!(
                "target.locate(window={},text={},exact=true,case_sensitive=true)",
                json!(window),
                json!(label)
            );
            let out = probe::capture(
                path,
                &["eval", &source, "--read-only"],
                None,
                LIMIT,
                DEADLINE,
                stop,
            )
            .await?;
            let values = receipt(&out, &["target.locate"])?;
            let location = &values[0]["location"];
            if location["window"]["id"] != *window
                || location["window"]["mapped"] != true
                || !location["window"]["pid"].as_u64().is_some_and(|p| p > 0)
                || location["target"]["name"] != *label
            {
                return Err("Zuse returned a different or incomplete window target".into());
            }
            Ok(location.clone())
        }
        Request::Observe {
            port,
            page,
            selector,
        } => {
            // The page WebSocket attaches directly. A browser-wide endpoint would create a tab.
            let endpoint = format!("ws://127.0.0.1:{port}/devtools/page/{page}");
            let source = format!(
                "browser.connect(endpoint={})\nobserve(source=\"browser\",selector={})\nbrowser.close()\n",
                json!(endpoint),
                json!(selector)
            );
            let out =
                probe::capture(path, &["run", "-"], Some(&source), LIMIT, DEADLINE, stop).await?;
            let values = receipt(&out, &["browser.connect", "observe", "browser.close"])?;
            if values[2]["ok"] != true {
                return Err("Zuse did not confirm closing the page connection".into());
            }
            let scope = &values[1];
            if scope["backend"] != "dom"
                || scope["scope"] != *selector
                || scope["connected"] != true
                || !scope["nodes"].is_array()
            {
                return Err("Zuse returned a different or disconnected DOM container".into());
            }
            Ok(json!({"target_id":page,"observation":scope}))
        }
    }
}

impl Computer {
    pub(crate) async fn inspect_local(&self, request: Request) -> Result<Value, String> {
        let permit = match request {
            Request::Check => None,
            _ => Some(ObservationPermit::local()?),
        };
        let mut interrupt = crate::platform::interrupt_signal().map_err(|e| e.to_string())?;
        let mut terminate = crate::platform::terminate_signal().map_err(|e| e.to_string())?;
        let stop = async {
            let revoke = async {
                match &permit {
                    Some(p) => p.revoked().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                why = revoke => why,
                _ = interrupt.recv() => "cancelled by interrupt".into(),
                _ = terminate.recv() => "cancelled by termination".into(),
            }
        };
        tokio::pin!(stop);
        let cli = self.version(stop.as_mut()).await?;
        let path = cli["path"]
            .as_str()
            .ok_or("Standalone Zuse CLI is not installed")?;
        let value = inspect(
            Path::new(path),
            cli["version"].as_str().ok_or("Missing Zuse version")?,
            cli["sha256"].as_str().ok_or("Missing Zuse digest")?,
            &request,
            stop.as_mut(),
        )
        .await?;
        if let Some(p) = &permit {
            p.check()?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests;

//! Explicit notifications to operator-configured webhook destinations.
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    net::IpAddr,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CONFIG_BYTES: u64 = 64 * 1024;
const PAYLOAD_BYTES: usize = 32 * 1024;

pub struct Webhooks {
    config: Configuration,
    client: Option<Client>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    #[serde(default = "schema_version")]
    schema_version: u32,
    #[serde(default)]
    destinations: BTreeMap<String, Destination>,
}
fn schema_version() -> u32 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    url: String,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notification {
    destination: String,
    event: String,
    text: String,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    delivery_id: Option<String>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

fn endpoint(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "invalid webhook destination URL")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err("webhooks require HTTPS, or HTTP at a literal loopback address, without URL userinfo or fragments".into());
    }
    Ok(url)
}

impl Webhooks {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        Self::with_config(
            &root,
            std::env::var_os("SSMCP_WEBHOOK_CONFIG")
                .map(PathBuf::from)
                .as_deref(),
        )
    }

    fn with_config(workspace: &Path, path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self {
                config: Configuration::default(),
                client: None,
            });
        };
        let config = Self::configuration(workspace, path)?;
        let client = crate::http_client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION"), " webhook"))
            .build()
            .map_err(|_| "cannot initialize webhook transport")?;
        Ok(Self {
            config,
            client: Some(client),
        })
    }

    fn configuration(workspace: &Path, path: &Path) -> Result<Configuration, String> {
        if !path.is_absolute() {
            return Err("SSMCP_WEBHOOK_CONFIG must be an absolute operator-owned file".into());
        }
        let workspace = workspace
            .canonicalize()
            .map_err(|_| "webhook workspace is unavailable")?;
        let canonical = path
            .canonicalize()
            .map_err(|_| "cannot open operator webhook configuration")?;
        if canonical.starts_with(&workspace)
            || std::fs::symlink_metadata(path)
                .map_err(|_| "cannot inspect operator webhook configuration")?
                .file_type()
                .is_symlink()
        {
            return Err(
                "operator webhook configuration must be outside the workspace and not a symlink"
                    .into(),
            );
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(&canonical)
            .map_err(|_| "cannot read operator webhook configuration")?;
        let metadata = file
            .metadata()
            .map_err(|_| "cannot inspect webhook configuration")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // The process owner, or an administrator, controls the named destinations.
            if ![unsafe { libc::geteuid() }, 0].contains(&metadata.uid())
                || metadata.mode() & 0o022 != 0
            {
                return Err(
                    "operator webhook configuration has unsafe ownership or write permissions"
                        .into(),
                );
            }
        }
        if !metadata.is_file() || metadata.len() > CONFIG_BYTES {
            return Err("webhook configuration must be a file of at most 64 KiB".into());
        }
        let mut bytes = Vec::new();
        file.take(CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read webhook configuration")?;
        if bytes.len() as u64 > CONFIG_BYTES {
            return Err("webhook configuration exceeds 64 KiB".into());
        }
        let config: Configuration =
            serde_json::from_slice(&bytes).map_err(|_| "invalid operator webhook configuration")?;
        if config.schema_version != 1 || config.destinations.len() > 64 {
            return Err(
                "webhook configuration requires schema_version 1 and at most 64 destinations"
                    .into(),
            );
        }
        for (name, destination) in &config.destinations {
            if !identifier(name) {
                return Err("invalid webhook destination name".into());
            }
            endpoint(&destination.url)?;
            if destination
                .description
                .as_ref()
                .is_some_and(|v| v.len() > 256 || v.chars().any(char::is_control))
            {
                return Err(
                    "webhook descriptions must be at most 256 bytes without control characters"
                        .into(),
                );
            }
            if destination.token_env.as_ref().is_some_and(|name| {
                name.is_empty()
                    || name.len() > 128
                    || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    || name.as_bytes()[0].is_ascii_digit()
            }) {
                return Err("invalid webhook token environment variable name".into());
            }
        }
        Ok(config)
    }

    pub fn tools() -> Vec<Value> {
        vec![
            json!({
                "name":"ssmcp.webhooks.list",
                "description":"List the notification destinations configured by the operator at client startup. Endpoints and credentials are never returned. No network request is made.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
            }),
            json!({
                "name":"ssmcp.webhooks.notify",
                "description":"Send one explicit JSON notification to a named operator-configured webhook. Optional bearer credentials are read from its configured environment variable. Redirects and automatic retries are disabled. Use a stable delivery_id when the receiver deduplicates retries.",
                "inputSchema":{"type":"object","additionalProperties":false,
                    "properties":{
                        "destination":{"type":"string","minLength":1,"maxLength":128},
                        "event":{"type":"string","minLength":1,"maxLength":128},
                        "text":{"type":"string","minLength":1,"maxLength":16384},
                        "data":{},
                        "delivery_id":{"type":"string","minLength":1,"maxLength":128}
                    },"required":["destination","event","text"]},
                "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true}
            }),
        ]
    }

    pub async fn perform(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "ssmcp.webhooks.list" => {
                if !arguments.as_object().is_some_and(|v| v.is_empty()) {
                    return Err("webhooks.list accepts an empty object".into());
                }
                let destinations: Vec<_> = self.config.destinations.iter()
                    .map(|(name, value)| json!({"name":name,"description":value.description,"authenticated":value.token_env.is_some()}))
                    .collect();
                Ok(json!({"schema_version":1,"destinations":destinations}))
            }
            "ssmcp.webhooks.notify" => {
                let input: Notification = serde_json::from_value(arguments)
                    .map_err(|_| "invalid webhook notification")?;
                if !identifier(&input.destination)
                    || !identifier(&input.event)
                    || input.text.trim().is_empty()
                    || input.text.len() > 16384
                    || input.text.contains('\0')
                    || input.delivery_id.as_ref().is_some_and(|id| !identifier(id))
                    || input
                        .data
                        .as_ref()
                        .is_some_and(|data| data.to_string().len() > PAYLOAD_BYTES)
                {
                    return Err("notification requires valid destination/event IDs, 1–16384 bytes of text and at most 32 KiB of data".into());
                }
                let destination = self.config
                    .destinations
                    .get(&input.destination)
                    .ok_or("unknown webhook destination; configure SSMCP_WEBHOOK_CONFIG before starting the client")?;
                let id = if let Some(id) = input.delivery_id {
                    id
                } else {
                    let mut bytes = [0u8; 16];
                    crate::platform::random_bytes(&mut bytes)
                        .map_err(|_| "cannot allocate notification ID")?;
                    format!("notification_{}", crate::hex::encode(bytes))
                };
                let payload = json!({
                    "schema_version":1,"id":id,"event":input.event,"text":input.text,
                    "data":input.data,"created_at_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
                });
                let mut request = self
                    .client
                    .as_ref()
                    .ok_or("no operator webhook transport configured")?
                    .post(endpoint(&destination.url)?)
                    .header("X-Ssmcp-Event", &input.event)
                    .header("X-Ssmcp-Delivery-Id", &id)
                    .json(&payload);
                if let Some(variable) = &destination.token_env {
                    let token = std::env::var(variable)
                        .map_err(|_| "webhook credential environment variable is unavailable")?;
                    if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control)
                    {
                        return Err("webhook credential is invalid".into());
                    }
                    request = request.bearer_auth(token);
                }
                let response = request.send().await.map_err(|_| {
                    "webhook transport failed or timed out; delivery may have occurred, check the receiver before retrying"
                })?;
                let status = response.status();
                if !status.is_success() {
                    return Err(format!(
                        "webhook destination returned HTTP {}; redirects are not followed",
                        status.as_u16()
                    ));
                }
                Ok(
                    json!({"schema_version":1,"delivery_id":id,"destination":input.destination,"event":input.event,
                    "delivered":true,"status":status.as_u16(),"attempts":1}),
                )
            }
            _ => Err("unknown webhook operation".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut random = [0u8; 16];
            crate::platform::random_bytes(&mut random).unwrap();
            let path = std::env::temp_dir().join(format!(
                "ssmcp-webhook-{}-{:x}",
                std::process::id(),
                u128::from_le_bytes(random)
            ));
            std::fs::create_dir_all(path.join("workspace")).unwrap();
            Self(path)
        }
        fn configure(&self, url: &str) {
            std::fs::write(
                self.0.join("operator.json"),
                json!({"schema_version":1,"destinations":{"build":{"url":url}}}).to_string(),
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn unconfigured_destinations_make_no_request_and_schema_is_bounded() {
        let fixture = Fixture::new();
        let webhooks = Webhooks::with_config(&fixture.0.join("workspace"), None).unwrap();
        assert_eq!(
            webhooks
                .perform("ssmcp.webhooks.list", json!({}))
                .await
                .unwrap()["destinations"],
            json!([])
        );
        assert!(
            webhooks
                .perform(
                    "ssmcp.webhooks.notify",
                    json!({"destination":"build","event":"done","text":"ready"})
                )
                .await
                .unwrap_err()
                .contains("unknown")
        );
        assert!(webhooks.perform("ssmcp.webhooks.notify",json!({"destination":"build","event":"done","text":"ready","url":"https://unconfigured.test"})).await.is_err());
        assert!(endpoint("http://example.com/receive").is_err());
        assert!(endpoint("https://user:secret@example.com/receive").is_err());
        assert!(endpoint("https://example.com/receive#secret").is_err());
        assert!(endpoint("https://example.com/receive").is_ok());
    }

    #[tokio::test]
    async fn notifications_deliver_json_once_and_never_follow_redirects() {
        use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(log): State<Arc<Mutex<Vec<Value>>>>,
                     headers: HeaderMap,
                     Json(value): Json<Value>| async move {
                        assert_eq!(headers["x-ssmcp-delivery-id"], "delivery-1");
                        assert_eq!(headers["x-ssmcp-event"], "build.done");
                        log.lock().unwrap().push(value);
                        axum::http::StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/redirect",
                post(|| async { axum::response::Redirect::temporary("/hook") }),
            )
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let fixture = Fixture::new();
        fixture.configure(&format!("http://{address}/hook"));
        let webhooks = Webhooks::with_config(
            &fixture.0.join("workspace"),
            Some(&fixture.0.join("operator.json")),
        )
        .unwrap();
        let args = json!({"destination":"build","event":"build.done","text":"Tests passed","data":{"passed":3},"delivery_id":"delivery-1"});
        let result = webhooks
            .perform("ssmcp.webhooks.notify", args.clone())
            .await
            .unwrap();
        assert_eq!(result["status"], 204);
        assert_eq!(result["delivered"], true);
        assert_eq!(received.lock().unwrap().len(), 1);
        assert_eq!(received.lock().unwrap()[0]["data"]["passed"], 3);
        let listed = webhooks
            .perform("ssmcp.webhooks.list", json!({}))
            .await
            .unwrap();
        assert!(!listed.to_string().contains("/hook"));
        fixture.configure(&format!("http://{address}/redirect"));
        let webhooks = Webhooks::with_config(
            &fixture.0.join("workspace"),
            Some(&fixture.0.join("operator.json")),
        )
        .unwrap();
        assert!(
            webhooks
                .perform("ssmcp.webhooks.notify", args)
                .await
                .unwrap_err()
                .contains("HTTP 307")
        );
        assert_eq!(received.lock().unwrap().len(), 1);
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_files_and_symlinks_cannot_select_credentials_or_destinations() {
        let fixture = Fixture::new();
        fixture.configure("https://example.com/notify");
        let config = fixture.0.join("operator.json");
        let workspace = fixture.0.join("workspace");
        let local = workspace.join("webhooks.json");
        std::fs::copy(&config, &local).unwrap();
        assert!(Webhooks::with_config(&workspace, Some(&local)).is_err());
        let link = fixture.0.join("linked.json");
        std::os::unix::fs::symlink(&local, &link).unwrap();
        assert!(Webhooks::with_config(&workspace, Some(&link)).is_err());
    }
}

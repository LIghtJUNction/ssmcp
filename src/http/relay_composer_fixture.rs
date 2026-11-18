//! An opt-in, process-local fixture for the real relay/composer PTY acceptance test.
//! Only the upstream identity provider is synthetic; MCP, OAuth, the broker,
//! WebSocket transport, collaboration and steering use their production routes.
use crate::{
    broker::Broker,
    identity::{GitHubConfig, Identity, IdentityProviders},
};
use axum::{
    Form, Json, Router,
    extract::{Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, task::JoinHandle};

const ISSUER: &str = "https://relay-composer.example";
const CALLBACK: &str = "https://relay-composer.example/oauth/callback/github";
const GITHUB_CLIENT: &str = "relay-composer-fixture-client";
const GITHUB_SECRET: &str = "relay-composer-fixture-secret";
const MAX_GRANTS: usize = 64;

struct Grant {
    challenge: String,
    expires: Instant,
}

#[derive(Default)]
struct Provider {
    codes: Mutex<HashMap<String, Grant>>,
    tokens: Mutex<HashSet<String>>,
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("fixture random bytes");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn profile() -> Value {
    json!({"id":123,"login":"fixture","type":"User"})
}

fn rejected() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid_fixture_authorization"})),
    )
        .into_response()
}

async fn provider_authorize(
    State(provider): State<Arc<Provider>>,
    Query(parameters): Query<HashMap<String, String>>,
) -> Response {
    let Some(state) = parameters.get("state") else {
        return rejected();
    };
    let Some(challenge) = parameters.get("code_challenge") else {
        return rejected();
    };
    if parameters.get("client_id").map(String::as_str) != Some(GITHUB_CLIENT)
        || parameters.get("redirect_uri").map(String::as_str) != Some(CALLBACK)
        || parameters.get("code_challenge_method").map(String::as_str) != Some("S256")
        || !(32..=512).contains(&state.len())
        || !state
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
        || challenge.len() != 43
        || URL_SAFE_NO_PAD
            .decode(challenge)
            .map_or(true, |bytes| bytes.len() != 32)
    {
        return rejected();
    }
    let code = random();
    let mut codes = provider.codes.lock().unwrap();
    codes.retain(|_, grant| grant.expires > Instant::now());
    if codes.len() >= MAX_GRANTS {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    codes.insert(
        code.clone(),
        Grant {
            challenge: challenge.clone(),
            expires: Instant::now() + Duration::from_secs(60),
        },
    );
    let mut callback = reqwest::Url::parse(CALLBACK).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", state);
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, callback.as_str())],
    )
        .into_response()
}

async fn provider_token(
    State(provider): State<Arc<Provider>>,
    Form(parameters): Form<HashMap<String, String>>,
) -> Response {
    let Some(code) = parameters.get("code") else {
        return rejected();
    };
    let Some(verifier) = parameters.get("code_verifier") else {
        return rejected();
    };
    if parameters.get("client_id").map(String::as_str) != Some(GITHUB_CLIENT)
        || parameters.get("client_secret").map(String::as_str) != Some(GITHUB_SECRET)
        || parameters.get("redirect_uri").map(String::as_str) != Some(CALLBACK)
        || !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
    {
        return rejected();
    }
    let grant = provider.codes.lock().unwrap().remove(code);
    if !grant.is_some_and(|grant| {
        grant.expires > Instant::now()
            && grant.challenge == URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }) {
        return rejected();
    }
    let token = random();
    let mut tokens = provider.tokens.lock().unwrap();
    if tokens.len() >= MAX_GRANTS {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    tokens.insert(token.clone());
    Json(json!({"access_token":token,"token_type":"bearer","scope":""})).into_response()
}

async fn provider_user(State(provider): State<Arc<Provider>>, headers: HeaderMap) -> Response {
    let valid = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| provider.tokens.lock().unwrap().remove(token));
    if valid {
        Json(profile()).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn provider_lookup(RoutePath(login): RoutePath<String>, headers: HeaderMap) -> Response {
    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("{GITHUB_CLIENT}:{GITHUB_SECRET}"))
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if login == "fixture" {
        Json(profile()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

struct Running {
    tasks: Vec<JoinHandle<std::io::Result<()>>>,
    ready: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = fs::remove_file(&self.ready);
    }
}

fn private_directory(path: &Path) -> crate::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    if !path.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
        || fs::canonicalize(path)? != path
    {
        return Err(
            "relay fixture directory must be an absolute, owned, nonsymlink 0700 directory".into(),
        );
    }
    Ok(())
}

async fn run(directory: PathBuf) -> crate::Result<()> {
    private_directory(&directory)?;
    let ready = directory.join("ready.json");
    if ready.exists() || directory.join("stop").exists() {
        return Err("relay fixture requires fresh ready and stop paths".into());
    }
    let mut running = Running {
        tasks: Vec::new(),
        ready: ready.clone(),
    };
    let provider = Arc::new(Provider::default());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let github_test_origin = format!("http://{}", listener.local_addr()?);
    let provider_routes = Router::new()
        .route("/login/oauth/authorize", get(provider_authorize))
        .route("/login/oauth/access_token", post(provider_token))
        .route("/user", get(provider_user))
        .route("/users/{login}", get(provider_lookup))
        .with_state(provider);
    running.tasks.push(tokio::spawn(async move {
        axum::serve(listener, provider_routes).await
    }));

    let mut broker = Broker::new(ISSUER.into());
    broker.identities = IdentityProviders::with_github_test_endpoints(
        ISSUER,
        GitHubConfig {
            client_id: GITHUB_CLIENT.into(),
            client_secret: GITHUB_SECRET.into(),
        },
        &github_test_origin,
    )?;
    broker.portal = crate::portal::Portal::open(&directory.join("accounts"))?;
    broker
        .portal
        .storage(|store| {
            store.login(&Identity {
                provider: "github".into(),
                subject: "123".into(),
                login: "fixture".into(),
            })?;
            store.claim_handle("123", "relayfixture")?;
            Ok(())
        })
        .await
        .map_err(|_| "could not seed the isolated relay account")?;
    let (account_token, expires_in) = broker
        .portal
        .client_storage(|credentials| credentials.login("123".into()))
        .await
        .map_err(|_| "could not issue the isolated account credential")?;
    broker.configure_user_policies(8 * 1024 * 1024).await?;
    let broker = Arc::new(broker);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_url = format!("http://{}", listener.local_addr()?);
    let routes = super::router(broker.clone());
    running.tasks.push(tokio::spawn(
        async move { axum::serve(listener, routes).await },
    ));

    let data = json!({
        "schema_version":1,
        "server_url":server_url,
        "issuer":ISSUER,
        "identity":{
            "provider":"github","subject":"123","login":"fixture",
            "principal":"fixture@github","username":"relayfixture"
        },
        "account_token":account_token,
        "account_expires_at":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + expires_in,
        "github_test_origin":github_test_origin
    });
    let temporary = directory.join("ready.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(&data)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, ready)?;

    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tick.tick().await;
        if running.tasks.iter().any(JoinHandle::is_finished) {
            return Err("isolated relay fixture listener exited early".into());
        }
        match fs::symlink_metadata(directory.join("stop")) {
            Ok(metadata) => {
                // SAFETY: geteuid has no preconditions.
                if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
                    return Err("relay fixture stop must be an owned regular file".into());
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    broker.disconnect_all().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "private loopback service for scripts/e2e_relay_composer.py"]
async fn relay_composer_server_fixture() -> crate::Result<()> {
    let directory = std::env::var_os("SSMCP_RELAY_COMPOSER_FIXTURE")
        .map(PathBuf::from)
        .ok_or("SSMCP_RELAY_COMPOSER_FIXTURE must name a caller-created private directory")?;
    tokio::time::timeout(Duration::from_secs(120), run(directory))
        .await
        .map_err(|_| "isolated relay composer fixture exceeded 120 seconds")?
}

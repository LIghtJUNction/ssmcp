//! Adversarial OAuth tests against the public HTTP routes and a real Unix attach.
//! No browser, external callback, installed coding agent, or model is contacted.
use reqwest::{Client, Response, StatusCode, Url, header};
use serde_json::{Value, json};
use ssmcp::{
    broker::Broker,
    http,
    wire::{self, Frame, Mode},
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{
    io::BufReader,
    net::{TcpListener, UnixStream},
    task::JoinHandle,
    time::timeout,
};

const ISSUER: &str = "https://mcp.example.test";
const CALLBACK: &str = "https://client.example.test/callback?existing=retained";
// RFC 7636 Appendix B vector: verify the server against an independent value.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

struct Attachment {
    stream: BufReader<UnixStream>,
    task: JoinHandle<ssmcp::Result<()>>,
    login_code: String,
    resource: String,
}

struct Harness {
    broker: Arc<Broker>,
    server: JoinHandle<()>,
    base: String,
    client: Client,
    attachment: Option<Attachment>,
}

struct Page {
    transaction: String,
    cookie: String,
    html: String,
    headers: header::HeaderMap,
}

#[derive(Clone)]
struct Registration {
    id: String,
    secret: Option<String>,
    method: String,
}

impl Harness {
    async fn new() -> Self {
        let broker = Arc::new(Broker::new(ISSUER.into()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = http::router(broker.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let mut harness = Self {
            broker,
            server,
            base,
            client,
            attachment: None,
        };
        harness.connect().await;
        harness
    }

    async fn connect(&mut self) {
        assert!(self.attachment.is_none());
        let (local, remote) = UnixStream::pair().unwrap();
        let task = tokio::spawn(self.broker.clone().attach(remote));
        let mut stream = BufReader::new(local);
        wire::write_frame(
            stream.get_mut(),
            &Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Shell,
                metadata: wire::ClientMetadata {
                    name: "oauth-test".into(),
                    cwd: "/test".into(),
                    hostname: "localhost".into(),
                },
            },
        )
        .await
        .unwrap();
        let ready = timeout(Duration::from_secs(5), wire::read_frame(&mut stream))
            .await
            .unwrap()
            .unwrap();
        let Some(Frame::Ready {
            url,
            login_code,
            login_expires_in,
            ..
        }) = ready
        else {
            panic!("Unix attachment did not return credentials");
        };
        assert_eq!(login_expires_in, 600);
        self.attachment = Some(Attachment {
            stream,
            task,
            login_code,
            resource: url,
        });
    }

    async fn disconnect(&mut self) {
        let attached = self.attachment.take().unwrap();
        drop(attached.stream);
        timeout(Duration::from_secs(5), attached.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    fn resource(&self) -> &str {
        &self.attachment.as_ref().unwrap().resource
    }
    fn login_code(&self) -> &str {
        &self.attachment.as_ref().unwrap().login_code
    }

    async fn register(&self, method: &str, name: &str) -> Registration {
        let response = self.client.post(format!("{}/oauth/register", self.base))
            .json(&json!({"client_name":name,"redirect_uris":[CALLBACK],
                "token_endpoint_auth_method":method,"grant_types":["authorization_code","refresh_token"]}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let value: Value = response.json().await.unwrap();
        Registration {
            id: value["client_id"].as_str().unwrap().into(),
            secret: value["client_secret"].as_str().map(str::to_owned),
            method: method.into(),
        }
    }

    fn authorize_parameters(&self, client: &Registration) -> Vec<(String, String)> {
        [
            ("response_type", "code"),
            ("client_id", &client.id),
            ("redirect_uri", CALLBACK),
            ("resource", self.resource()),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", "space + Unicode 中文 & %"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    async fn authorize(&self, parameters: &[(String, String)]) -> Response {
        self.client
            .get(format!("{}/oauth/authorize", self.base))
            .query(parameters)
            .send()
            .await
            .unwrap()
    }

    async fn page(&self, client: &Registration) -> Page {
        let response = self.authorize(&self.authorize_parameters(client)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        let cookie = headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .into();
        let html = response.text().await.unwrap();
        let marker = "name=transaction_id value=\"";
        let transaction = html
            .split_once(marker)
            .expect("hidden transaction field")
            .1
            .split('"')
            .next()
            .unwrap()
            .into();
        Page {
            transaction,
            cookie,
            html,
            headers,
        }
    }

    async fn login(
        &self,
        page: &Page,
        code: &str,
        cookie: Option<&str>,
        origin: Option<&str>,
    ) -> Response {
        let mut request = self
            .client
            .post(format!("{}/oauth/authorize", self.base))
            .form(&[
                ("transaction_id", page.transaction.as_str()),
                ("login_code", code),
            ]);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        request.send().await.unwrap()
    }

    async fn code(&self, client: &Registration) -> String {
        let page = self.page(client).await;
        let response = self
            .login(&page, self.login_code(), Some(&page.cookie), Some(ISSUER))
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let callback = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let query: BTreeMap<_, _> = callback.query_pairs().into_owned().collect();
        assert_eq!(query.get("iss").unwrap(), ISSUER);
        assert_eq!(query.get("state").unwrap(), "space + Unicode 中文 & %");
        assert_eq!(query.get("existing").unwrap(), "retained");
        assert!(!callback.as_str().contains(self.login_code()));
        query.get("code").unwrap().clone()
    }

    fn exchange_parameters(&self, client: &Registration, code: &str) -> Vec<(String, String)> {
        [
            ("grant_type", "authorization_code"),
            ("client_id", &client.id),
            ("code", code),
            ("code_verifier", VERIFIER),
            ("resource", self.resource()),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    fn refresh_parameters(&self, client: &Registration, refresh: &str) -> Vec<(String, String)> {
        [
            ("grant_type", "refresh_token"),
            ("client_id", &client.id),
            ("refresh_token", refresh),
            ("resource", self.resource()),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    async fn token(&self, parameters: &[(String, String)], client: &Registration) -> Response {
        let mut body = parameters.to_vec();
        let mut request = self.client.post(format!("{}/oauth/token", self.base));
        match client.method.as_str() {
            "client_secret_basic" => {
                request = request.basic_auth(&client.id, client.secret.as_ref())
            }
            "client_secret_post" => {
                body.push(("client_secret".into(), client.secret.clone().unwrap()))
            }
            "none" => {}
            _ => unreachable!(),
        }
        request.form(&body).send().await.unwrap()
    }

    async fn access_status(&self, access: &str) -> StatusCode {
        self.client
            .post(format!(
                "{}{}",
                self.base,
                Url::parse(self.resource()).unwrap().path()
            ))
            .bearer_auth(access)
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .send()
            .await
            .unwrap()
            .status()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(attachment) = &self.attachment {
            attachment.task.abort();
        }
        self.server.abort();
    }
}

fn replace(parameters: &[(String, String)], key: &str, value: &str) -> Vec<(String, String)> {
    parameters
        .iter()
        .map(|(name, current)| {
            (
                name.clone(),
                if name == key {
                    value.into()
                } else {
                    current.clone()
                },
            )
        })
        .collect()
}

async fn invalid_grant(response: Response) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["error"], "invalid_grant");
}

async fn tokens(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["token_type"], "Bearer");
    assert!((1..=3600).contains(&value["expires_in"].as_u64().unwrap()));
    assert!(value["access_token"].as_str().unwrap().starts_with("at_"));
    assert!(value["refresh_token"].as_str().unwrap().starts_with("rt_"));
    value
}

#[tokio::test]
async fn discovery_and_login_form_expose_no_reusable_ssh_bearer() {
    let harness = Harness::new().await;
    assert_eq!(
        harness.access_status(harness.login_code()).await,
        StatusCode::UNAUTHORIZED
    );
    let resource = Url::parse(harness.resource()).unwrap();
    let response = harness
        .client
        .get(format!("{}{}", harness.base, resource.path()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let metadata_path = format!("/.well-known/oauth-protected-resource{}", resource.path());
    assert!(
        response.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains(&format!("{ISSUER}{metadata_path}"))
    );
    let metadata: Value = harness
        .client
        .get(format!("{}{metadata_path}", harness.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["resource"], harness.resource());
    assert_eq!(metadata["authorization_servers"], json!([ISSUER]));
    let server: Value = harness
        .client
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            harness.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(server["issuer"], ISSUER);
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        server["authorization_response_iss_parameter_supported"],
        true
    );
    assert_eq!(server["client_id_metadata_document_supported"], false);
    let registration = harness
        .register("none", "<script>alert(1)</script> \" & client")
        .await;
    let page = harness.page(&registration).await;
    assert!(!page.html.contains("<script>"));
    assert!(page.html.contains("&lt;script&gt;"));
    let visible: Vec<_> = page
        .html
        .split("<input")
        .skip(1)
        .map(|tag| tag.split('>').next().unwrap())
        .filter(|tag| !tag.contains("type=hidden"))
        .collect();
    assert_eq!(visible.len(), 1);
    assert!(visible[0].contains("name=login_code"));
    assert!(!page.html.contains(harness.login_code()));
    assert!(page.html.contains(harness.resource()));
    assert!(page.html.contains("https://client.example.test"));
    let set_cookie = page.headers[header::SET_COOKIE].to_str().unwrap();
    for flag in [
        "__Host-ssmcp-oauth=",
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Lax",
    ] {
        assert!(set_cookie.contains(flag));
    }
    assert_eq!(page.headers["referrer-policy"], "same-origin");
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    assert!(
        page.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self' https://client.example.test;")
    );
    assert!(
        page.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
}

#[tokio::test]
async fn redirect_registration_and_pkce_errors_never_create_an_open_redirect() {
    let harness = Harness::new().await;
    for uri in [
        "http://client.example.test/callback",
        "javascript:alert(1)",
        "data:text/plain,test",
        "https://user@client.example.test/callback",
        "https://client.example.test/callback#fragment",
        "https://client.example.test/callback?code=preexisting",
        "https://*.example.test/callback",
        "https://client.example;form-action.example/callback",
        "https://client.example%3B.example/callback",
    ] {
        let response = harness
            .client
            .post(format!("{}/oauth/register", harness.base))
            .json(&json!({"redirect_uris":[uri],"token_endpoint_auth_method":"none"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!response.headers().contains_key(header::LOCATION));
    }
    let client = harness.register("none", "client").await;
    let parameters = harness.authorize_parameters(&client);
    for uri in [
        "https://attacker.example/callback",
        "https://client.example.test/callback?existing=retained&x=1",
        "https://CLIENT.example.test/callback?existing=retained",
    ] {
        let response = harness
            .authorize(&replace(&parameters, "redirect_uri", uri))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!response.headers().contains_key(header::LOCATION));
    }
    for (key, value) in [
        ("code_challenge_method", "plain"),
        ("code_challenge", "short"),
        ("resource", "https://attacker.example/terminal"),
        ("response_type", "token"),
    ] {
        let response = harness.authorize(&replace(&parameters, key, value)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let callback = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let query: BTreeMap<_, _> = callback.query_pairs().into_owned().collect();
        assert!(query.contains_key("error"));
        assert!(!query.contains_key("code"));
        assert_eq!(query.get("iss").unwrap(), ISSUER);
        assert_eq!(query.get("state").unwrap(), "space + Unicode 中文 & %");
        assert_eq!(callback.host_str(), Some("client.example.test"));
    }
    let mut duplicate = parameters.clone();
    duplicate.push((
        "redirect_uri".into(),
        "https://attacker.example/callback".into(),
    ));
    let response = harness.authorize(&duplicate).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key(header::LOCATION));
}

#[tokio::test]
async fn csrf_failures_and_duplicate_forms_do_not_consume_the_login_code() {
    let harness = Harness::new().await;
    let client = harness.register("none", "client").await;
    let first = harness.page(&client).await;
    let other = harness.page(&client).await;
    for (cookie, origin) in [
        (None, Some(ISSUER)),
        (Some(other.cookie.as_str()), Some(ISSUER)),
        (Some(first.cookie.as_str()), None),
        (Some(first.cookie.as_str()), Some("null")),
        (
            Some(first.cookie.as_str()),
            Some("https://attacker.example"),
        ),
    ] {
        let response = harness
            .login(&first, harness.login_code(), cookie, origin)
            .await;
        assert!(response.status().is_client_error());
        assert!(!response.headers().contains_key(header::LOCATION));
        assert!(
            !response
                .text()
                .await
                .unwrap()
                .contains(harness.login_code())
        );
    }
    let response = harness
        .client
        .post(format!("{}/oauth/authorize", harness.base))
        .header(header::ORIGIN, ISSUER)
        .header(header::COOKIE, &first.cookie)
        .form(&[
            ("transaction_id", first.transaction.as_str()),
            ("login_code", harness.login_code()),
            ("login_code", harness.login_code()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = harness
        .login(&first, &"0".repeat(64), Some(&first.cookie), Some(ISSUER))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self' https://client.example.test;")
    );
    let response = harness
        .login(
            &first,
            harness.login_code(),
            Some(&first.cookie),
            Some(ISSUER),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let response = harness
        .login(
            &other,
            harness.login_code(),
            Some(&other.cookie),
            Some(ISSUER),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn code_is_bound_to_client_resource_redirect_and_pkce_before_consumption_or_replay_revocation()
 {
    let harness = Harness::new().await;
    let client = harness.register("none", "client").await;
    let other = harness.register("none", "different client").await;
    let code = harness.code(&client).await;
    let parameters = harness.exchange_parameters(&client, &code);
    let bad_verifier = replace(&parameters, "code_verifier", &"x".repeat(43));
    let bad_resource = replace(&parameters, "resource", &format!("{ISSUER}/different-user"));
    let bad_client = replace(&parameters, "client_id", &other.id);
    let mut bad_redirect = parameters.clone();
    bad_redirect.push((
        "redirect_uri".into(),
        "https://attacker.example/callback".into(),
    ));
    for values in [&bad_verifier, &bad_resource, &bad_client, &bad_redirect] {
        invalid_grant(harness.token(values, &client).await).await;
    }
    let mut duplicate = parameters.clone();
    duplicate.push(("code_verifier".into(), VERIFIER.into()));
    let response = harness.token(&duplicate, &client).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "invalid_request"
    );
    // OAuth 2.1 permits redirect_uri omission at the token endpoint.
    let grant = tokens(harness.token(&parameters, &client).await).await;
    let access = grant["access_token"].as_str().unwrap();
    assert_eq!(harness.access_status(access).await, StatusCode::OK);
    for values in [&bad_verifier, &bad_resource, &bad_client, &bad_redirect] {
        invalid_grant(harness.token(values, &client).await).await;
    }
    assert_eq!(harness.access_status(access).await, StatusCode::OK);
    invalid_grant(harness.token(&parameters, &client).await).await;
    assert_eq!(
        harness.access_status(access).await,
        StatusCode::UNAUTHORIZED
    );
    invalid_grant(
        harness
            .token(
                &harness.refresh_parameters(&client, grant["refresh_token"].as_str().unwrap()),
                &client,
            )
            .await,
    )
    .await;
}

#[tokio::test]
async fn refresh_rotation_retains_replay_detection_without_cross_client_or_resource_revocation() {
    let harness = Harness::new().await;
    let client = harness.register("none", "client").await;
    let other = harness.register("none", "different client").await;
    let code = harness.code(&client).await;
    let grant = tokens(
        harness
            .token(&harness.exchange_parameters(&client, &code), &client)
            .await,
    )
    .await;
    let parameters = harness.refresh_parameters(&client, grant["refresh_token"].as_str().unwrap());
    for values in [
        replace(&parameters, "client_id", &other.id),
        replace(&parameters, "resource", &format!("{ISSUER}/different-user")),
    ] {
        invalid_grant(harness.token(&values, &client).await).await;
    }
    let next = tokens(harness.token(&parameters, &client).await).await;
    assert!(next["refresh_token"] != grant["refresh_token"]);
    assert!(next["access_token"] != grant["access_token"]);
    let access = next["access_token"].as_str().unwrap();
    for values in [
        replace(&parameters, "client_id", &other.id),
        replace(&parameters, "resource", &format!("{ISSUER}/different-user")),
    ] {
        invalid_grant(harness.token(&values, &client).await).await;
    }
    assert_eq!(harness.access_status(access).await, StatusCode::OK);
    invalid_grant(harness.token(&parameters, &client).await).await;
    assert_eq!(
        harness.access_status(access).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        harness
            .access_status(grant["access_token"].as_str().unwrap())
            .await,
        StatusCode::UNAUTHORIZED
    );
    invalid_grant(
        harness
            .token(
                &harness.refresh_parameters(&client, next["refresh_token"].as_str().unwrap()),
                &client,
            )
            .await,
    )
    .await;
}

#[tokio::test]
async fn simultaneous_code_and_refresh_redemptions_cannot_create_independent_valid_grants() {
    let mut harness = Harness::new().await;
    let client = harness.register("none", "client").await;
    let code = harness.code(&client).await;
    let parameters = harness.exchange_parameters(&client, &code);
    let (first, second) = tokio::join!(
        harness.token(&parameters, &client),
        harness.token(&parameters, &client)
    );
    let (accepted, replayed) = if first.status() == StatusCode::OK {
        (first, second)
    } else {
        (second, first)
    };
    let grant = tokens(accepted).await;
    invalid_grant(replayed).await;
    assert_eq!(
        harness
            .access_status(grant["access_token"].as_str().unwrap())
            .await,
        StatusCode::UNAUTHORIZED
    );
    harness.disconnect().await;
    harness.connect().await;
    let code = harness.code(&client).await;
    let grant = tokens(
        harness
            .token(&harness.exchange_parameters(&client, &code), &client)
            .await,
    )
    .await;
    let parameters = harness.refresh_parameters(&client, grant["refresh_token"].as_str().unwrap());
    let (first, second) = tokio::join!(
        harness.token(&parameters, &client),
        harness.token(&parameters, &client)
    );
    let (accepted, replayed) = if first.status() == StatusCode::OK {
        (first, second)
    } else {
        (second, first)
    };
    let grant = tokens(accepted).await;
    invalid_grant(replayed).await;
    assert_eq!(
        harness
            .access_status(grant["access_token"].as_str().unwrap())
            .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn reconnecting_the_same_username_does_not_inherit_old_authority() {
    let mut harness = Harness::new().await;
    let client = harness.register("none", "client").await;
    let code = harness.code(&client).await;
    let grant = tokens(
        harness
            .token(&harness.exchange_parameters(&client, &code), &client)
            .await,
    )
    .await;
    let access = grant["access_token"].as_str().unwrap();
    let original_resource = harness.resource().to_owned();
    harness.disconnect().await;
    assert!(
        harness
            .broker
            .authenticate(
                Url::parse(&original_resource)
                    .unwrap()
                    .path()
                    .trim_start_matches('/'),
                access
            )
            .await
            .is_none()
    );
    harness.connect().await;
    assert_eq!(harness.resource(), original_resource);
    assert_eq!(
        harness.access_status(access).await,
        StatusCode::UNAUTHORIZED
    );
    invalid_grant(
        harness
            .token(
                &harness.refresh_parameters(&client, grant["refresh_token"].as_str().unwrap()),
                &client,
            )
            .await,
    )
    .await;
    invalid_grant(
        harness
            .token(&harness.exchange_parameters(&client, &code), &client)
            .await,
    )
    .await;
    let fresh_code = harness.code(&client).await;
    let fresh = tokens(
        harness
            .token(&harness.exchange_parameters(&client, &fresh_code), &client)
            .await,
    )
    .await;
    assert_eq!(
        harness
            .access_status(fresh["access_token"].as_str().unwrap())
            .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn confidential_client_authentication_cannot_be_downgraded_or_combined() {
    for method in ["client_secret_basic", "client_secret_post"] {
        let harness = Harness::new().await;
        let client = harness.register(method, "confidential client").await;
        let code = harness.code(&client).await;
        let parameters = harness.exchange_parameters(&client, &code);
        let unauthenticated = Registration {
            method: "none".into(),
            ..client.clone()
        };
        assert_eq!(
            harness.token(&parameters, &unauthenticated).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut wrong = client.clone();
        wrong.secret = Some("wrong-secret".into());
        assert_eq!(
            harness.token(&parameters, &wrong).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut mixed = parameters.clone();
        mixed.push(("client_secret".into(), client.secret.clone().unwrap()));
        let response = harness
            .client
            .post(format!("{}/oauth/token", harness.base))
            .basic_auth(&client.id, client.secret.as_ref())
            .form(&mixed)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let grant = tokens(harness.token(&parameters, &client).await).await;
        assert_eq!(
            harness
                .access_status(grant["access_token"].as_str().unwrap())
                .await,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn invalid_user_paths_are_not_interpolated_into_the_authentication_challenge() {
    let harness = Harness::new().await;
    for path in [
        "/bad%22%2C%20resource_metadata%3D%22https%3A%2F%2Fattacker.example",
        "/bad%0D%0AInjected%3Ayes",
        "/bad%2Fuser",
    ] {
        let response = harness
            .client
            .get(format!("{}{path}", harness.base))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
        assert!(!response.headers().contains_key("injected"));
    }
}

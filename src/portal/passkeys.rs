//! Bind discoverable credentials to existing GitHub accounts and issue portal sessions.
use super::*;
use axum::{Json, extract::DefaultBodyLimit};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use webauthn_rs::prelude::*;

const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CEREMONIES: usize = 1024;

pub(super) struct Pending {
    pub(super) expires: Instant,
    ceremony: Ceremony,
}

enum Ceremony {
    Register {
        subject: String,
        handle: Uuid,
        state: PasskeyRegistration,
    },
    Login(DiscoverableAuthentication),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    csrf: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finish {
    csrf: String,
    credential: Value,
}

struct ApiError(Problem);
impl From<Problem> for ApiError {
    fn from(value: Problem) -> Self {
        Self(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.0.status(),
            Json(json!({"error":{"message": self.0.message()}})),
        )
            .into_response();
        if self.0 == Problem::Busy {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}

pub(super) fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/account/passkeys/register/start", post(register_start))
        .route("/account/passkeys/register/finish", post(register_finish))
        .route("/account/passkeys/login/start", post(login_start))
        .route("/account/passkeys/login/finish", post(login_finish))
        .layer(DefaultBodyLimit::max(65536))
}

fn relying_party(broker: &Broker) -> Result<Webauthn, Problem> {
    if !broker.portal.enabled(broker) {
        return Err(Problem::Disabled);
    }
    let origin = Url::parse(&broker.public_url).map_err(|_| Problem::Disabled)?;
    let rp = origin.domain().ok_or(Problem::Disabled)?;
    if origin.scheme() != "https" && !(origin.scheme() == "http" && rp == "localhost") {
        return Err(Problem::Disabled);
    }
    WebauthnBuilder::new(rp, &origin)
        .and_then(|builder| builder.rp_name("SSMCP").timeout(CEREMONY_TTL).build())
        .map_err(|_| Problem::Disabled)
}

fn verified(
    broker: &Broker,
    headers: &HeaderMap,
    csrf: String,
) -> Result<(DigestKey, Option<String>), Problem> {
    broker.portal.verify(
        headers,
        &HashMap::from([("csrf".to_owned(), csrf)]),
        &broker.public_url,
    )
}

fn charge_start(portal: &Portal, key: DigestKey) -> Result<(), Problem> {
    let mut browser = portal.browser.lock().map_err(|_| Problem::Storage)?;
    browser.charge(format!("passkey:{}", URL_SAFE_NO_PAD.encode(key)), 10)?;
    browser.charge("passkey:global".into(), 600)
}

fn begin(
    portal: &Portal,
    key: DigestKey,
    subject: Option<&str>,
    ceremony: Ceremony,
) -> Result<(), Problem> {
    let mut browser = portal.browser.lock().map_err(|_| Problem::Storage)?;
    browser.prune();
    if browser.sessions.get(&key).map(|s| s.subject.as_deref()) != Some(subject) {
        return Err(Problem::Unauthorized);
    }
    if !browser.passkeys.contains_key(&key) && browser.passkeys.len() >= MAX_CEREMONIES {
        return Err(Problem::Busy);
    }
    browser.passkeys.insert(
        key,
        Pending {
            expires: Instant::now() + CEREMONY_TTL,
            ceremony,
        },
    );
    Ok(())
}

fn take(portal: &Portal, key: DigestKey) -> Result<Ceremony, Problem> {
    let mut browser = portal.browser.lock().map_err(|_| Problem::Storage)?;
    browser.prune();
    browser
        .passkeys
        .remove(&key)
        .map(|p| p.ceremony)
        .ok_or(Problem::PasskeyExpired)
}

fn payload<T: DeserializeOwned>(
    input: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, Problem> {
    input
        .map(|Json(data)| data)
        .map_err(|_| Problem::PasskeyInvalid)
}

async fn register_start(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    input: Result<Json<Start>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (key, subject) = verified(&broker, &headers, payload(input)?.csrf)?;
    let subject = subject.ok_or(Problem::Unauthorized)?;
    let server = relying_party(&broker)?;
    charge_start(&broker.portal, key)?;
    let owner = subject.clone();
    let (name, handle, exclude) = broker
        .portal
        .storage(move |store| {
            let name = store.account(&owner)?.github_login;
            let handle = store.registration_handle(&owner)?;
            let exclude = store
                .passkeys_for(&owner)?
                .iter()
                .map(|p| p.cred_id().clone())
                .collect();
            Ok((name, handle, exclude))
        })
        .await?;
    let (options, state) = server
        .start_passkey_registration(handle, &name, &name, Some(exclude))
        .map_err(|_| Problem::PasskeyInvalid)?;
    let mut options = serde_json::to_value(options).map_err(|_| Problem::Storage)?;
    // Login starts without a username, so the credential must be discoverable.
    let selection = &mut options["publicKey"]["authenticatorSelection"];
    selection["residentKey"] = json!("required");
    selection["requireResidentKey"] = json!(true);
    begin(
        &broker.portal,
        key,
        Some(&subject),
        Ceremony::Register {
            subject: subject.clone(),
            handle,
            state,
        },
    )?;
    Ok(Json(options))
}

async fn register_finish(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    input: Result<Json<Finish>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Finish { csrf, credential } = payload(input)?;
    let (key, subject) = verified(&broker, &headers, csrf)?;
    let subject = subject.ok_or(Problem::Unauthorized)?;
    let Ceremony::Register {
        subject: owner,
        handle,
        state,
    } = take(&broker.portal, key)?
    else {
        return Err(Problem::PasskeyExpired.into());
    };
    if owner != subject {
        return Err(Problem::Unauthorized.into());
    }
    let server = relying_party(&broker)?;
    let credential: RegisterPublicKeyCredential =
        serde_json::from_value(credential).map_err(|_| Problem::PasskeyInvalid)?;
    let _policy = broker.user_policy_updates.lock().await;
    let live = Arc::clone(&broker);
    broker
        .portal
        .storage(move |store| {
            let passkey = server
                .finish_passkey_registration(&credential, &state)
                .map_err(|_| Problem::PasskeyInvalid)?;
            // An overlapping logout or ban must prevent a late enrollment.
            let mut browser = live.portal.browser.lock().map_err(|_| Problem::Storage)?;
            browser.prune();
            if browser
                .sessions
                .get(&key)
                .and_then(|s| s.subject.as_deref())
                != Some(subject.as_str())
            {
                return Err(Problem::Unauthorized);
            }
            store.bind_passkey(&subject, handle, passkey)
        })
        .await?;
    Ok(Json(json!({"redirect":"/#account"})))
}

async fn login_start(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    input: Result<Json<Start>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (key, subject) = verified(&broker, &headers, payload(input)?.csrf)?;
    if subject.is_some() {
        return Err(Problem::Forbidden.into());
    }
    charge_start(&broker.portal, key)?;
    let (options, state) = relying_party(&broker)?
        .start_discoverable_authentication()
        .map_err(|_| Problem::Storage)?;
    // The button is a user-initiated account picker, so use default browser mediation.
    let options = json!({"publicKey": options.public_key});
    begin(&broker.portal, key, None, Ceremony::Login(state))?;
    Ok(Json(options))
}

async fn login_finish(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    input: Result<Json<Finish>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Finish { csrf, credential } = payload(input)?;
    let (key, subject) = verified(&broker, &headers, csrf)?;
    if subject.is_some() {
        return Err(Problem::Forbidden.into());
    }
    let Ceremony::Login(state) = take(&broker.portal, key)? else {
        return Err(Problem::PasskeyExpired.into());
    };
    let server = relying_party(&broker)?;
    let proof: PublicKeyCredential =
        serde_json::from_value(credential).map_err(|_| Problem::PasskeyInvalid)?;
    let _policy = broker.user_policy_updates.lock().await;
    let identity = broker
        .portal
        .storage(move |store| {
            let (handle, id) = server
                .identify_discoverable_authentication(&proof)
                .map_err(|_| Problem::PasskeyInvalid)?;
            let (owner, expected, passkey) = store.passkey_owner(id).map_err(|p| {
                if p == Problem::Unauthorized {
                    Problem::PasskeyInvalid
                } else {
                    p
                }
            })?;
            if expected != handle {
                return Err(Problem::PasskeyInvalid);
            }
            let result = server
                .finish_discoverable_authentication_v2(&proof, state, &passkey)
                .map_err(|_| Problem::PasskeyInvalid)?;
            store.update_passkey(&owner, &result)?;
            let account = store.account(&owner)?;
            Ok(Identity {
                provider: "github".into(),
                subject: account.subject,
                login: account.github_login,
            })
        })
        .await?;
    let mut issued =
        issue_identity_session(&broker.portal, key, identity, LoginMethod::Passkey).await?;
    let cookie = issued
        .headers_mut()
        .remove(header::SET_COOKIE)
        .ok_or(Problem::Storage)?;
    let mut response = Json(json!({"redirect":"/#account"})).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ceremony() -> Ceremony {
        let origin = Url::parse("https://portal.test").unwrap();
        let server = WebauthnBuilder::new("portal.test", &origin)
            .unwrap()
            .build()
            .unwrap();
        Ceremony::Login(server.start_discoverable_authentication().unwrap().1)
    }

    #[test]
    fn passkey_challenges_belong_to_one_live_browser_and_are_consumed_once() {
        let portal = Portal::default();
        let first = digest("first");
        let other = digest("other");
        for key in [first, other] {
            portal.browser.lock().unwrap().sessions.insert(
                key,
                Session {
                    subject: None,
                    csrf: random().unwrap(),
                    expires: Instant::now() + LOGIN_TTL,
                },
            );
        }
        begin(&portal, first, None, ceremony()).unwrap();
        assert!(matches!(take(&portal, other), Err(Problem::PasskeyExpired)));
        assert!(matches!(take(&portal, first), Ok(Ceremony::Login(_))));
        assert!(matches!(take(&portal, first), Err(Problem::PasskeyExpired)));
        assert_eq!(
            begin(&portal, first, Some("other-account"), ceremony()),
            Err(Problem::Unauthorized)
        );
        begin(&portal, first, None, ceremony()).unwrap();
        portal
            .browser
            .lock()
            .unwrap()
            .passkeys
            .get_mut(&first)
            .unwrap()
            .expires = Instant::now();
        assert!(matches!(take(&portal, first), Err(Problem::PasskeyExpired)));
        begin(&portal, other, None, ceremony()).unwrap();
        portal
            .browser
            .lock()
            .unwrap()
            .remove_session(other)
            .unwrap();
        assert!(matches!(take(&portal, other), Err(Problem::PasskeyExpired)));
    }
}

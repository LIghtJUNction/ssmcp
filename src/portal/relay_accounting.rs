//! Service-to-service accounting for externally authenticated relay users.
use super::{Problem, store::external::MonthlyLedger};
use crate::{
    broker::Broker,
    user_policy::{FREE_MONTHLY_TRAFFIC_BYTES, TrafficError, TrafficFrame, TrafficQuota},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(super) state_dir: PathBuf,
    pub(super) issuer: String,
    service_token_sha256: String,
    pub(super) users: BTreeMap<String, String>,
    pub(super) utc_offset_minutes: i32,
    #[serde(default = "default_allowance")]
    pub(super) free_monthly_bytes: u64,
    #[serde(default = "default_allowance")]
    pub(super) paid_monthly_bytes: u64,
}

fn default_allowance() -> u64 {
    FREE_MONTHLY_TRAFFIC_BYTES
}

pub(super) fn valid_user_id(id: &str) -> bool {
    id.parse::<i64>()
        .is_ok_and(|v| v > 0 && v.to_string() == id)
}

fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Config {
    pub(crate) fn validate(
        &self,
        principals: &BTreeMap<String, String>,
        ingress: &str,
    ) -> crate::Result<()> {
        let issuer = reqwest::Url::parse(&self.issuer)?;
        if issuer.scheme() != "https"
            || issuer.origin().ascii_serialization() != self.issuer
            || !self.state_dir.is_absolute()
            || !crate::user_policy::valid_traffic_utc_offset(self.utc_offset_minutes)
            || !hex64(&self.service_token_sha256)
            || self.service_token_sha256 == ingress
            || principals.values().any(|v| v == &self.service_token_sha256)
            || self.users.keys().ne(principals.keys())
            || self.users.values().any(|id| !valid_user_id(id))
            || self.users.values().collect::<BTreeSet<_>>().len() != self.users.len()
        {
            return Err(
                "invalid external relay accounting binding, issuer or service credential".into(),
            );
        }
        Ok(())
    }
}

pub(crate) struct Accounting {
    token_digest: String,
    user_ids: BTreeMap<String, String>,
    names: BTreeMap<String, String>,
    ledger: Arc<Mutex<MonthlyLedger>>,
}

impl Accounting {
    pub(crate) fn open(config: Config) -> crate::Result<Self> {
        let ledger = MonthlyLedger::open(&config)?;
        let names = config
            .users
            .iter()
            .map(|(name, id)| (id.clone(), name.clone()))
            .collect();
        Ok(Self {
            token_digest: config.service_token_sha256,
            user_ids: config.users,
            names,
            ledger: Arc::new(Mutex::new(ledger)),
        })
    }

    async fn storage<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut MonthlyLedger) -> Result<T, Problem> + Send + 'static,
    ) -> Result<T, Problem> {
        let ledger = self.ledger.clone();
        tokio::task::spawn_blocking(move || {
            let mut ledger = ledger.lock().map_err(|_| Problem::Storage)?;
            op(&mut ledger)
        })
        .await
        .map_err(|_| Problem::Storage)?
    }

    pub(crate) async fn reserve(
        &self,
        principal: &str,
        uploaded: bool,
        bytes: usize,
    ) -> Result<(), TrafficError> {
        let id = self
            .user_ids
            .get(principal)
            .ok_or(TrafficError::AccountRequired)?
            .clone();
        let bytes = u64::try_from(bytes).map_err(|_| TrafficError::Exhausted)?;
        self.storage(move |ledger| ledger.reserve(&id, TrafficFrame { uploaded, bytes }))
            .await
            .map_err(|e| match e {
                Problem::TrafficQuotaExceeded | Problem::Limit => TrafficError::Exhausted,
                _ => TrafficError::Storage,
            })
    }

    async fn read<T: DeserializeOwned>(&self, req: Request) -> Result<T, Problem> {
        let mut values = req.headers().get_all(header::AUTHORIZATION).iter();
        let token = values
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|v| hex64(v))
            .ok_or(Problem::Unauthorized)?;
        let digest = crate::hex::encode(Sha256::digest(token.as_bytes()));
        if values.next().is_some()
            || !bool::from(self.token_digest.as_bytes().ct_eq(digest.as_bytes()))
        {
            return Err(Problem::Unauthorized);
        }
        if !req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| {
                s.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
            })
        {
            return Err(Problem::Invalid);
        }
        let bytes = to_bytes(req.into_body(), 2048)
            .await
            .map_err(|_| Problem::Invalid)?;
        serde_json::from_slice(&bytes).map_err(|_| Problem::Invalid)
    }
}

pub(crate) fn routes(enabled: bool) -> Router<Arc<Broker>> {
    if !enabled {
        return Router::new();
    }
    Router::new()
        .route("/internal/v1/ssmcp/quota", post(quota))
        .route("/internal/v1/ssmcp/entitlements", post(entitlements))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    user_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaidOrder {
    user_id: String,
    order_id: String,
    period: String,
    bytes: String,
}

fn response(result: Result<Value, Problem>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => {
            let (code, status) = match e {
                Problem::Unauthorized => ("unauthorized", StatusCode::UNAUTHORIZED),
                Problem::SessionNotFound => ("unknown_user", StatusCode::NOT_FOUND),
                Problem::Invalid => ("invalid_request", StatusCode::BAD_REQUEST),
                Problem::TrafficPeriodMismatch => ("period_mismatch", StatusCode::CONFLICT),
                Problem::TrafficGrantConflict => ("order_conflict", StatusCode::CONFLICT),
                Problem::Limit => ("ledger_limit", StatusCode::CONFLICT),
                _ => ("accounting_unavailable", StatusCode::SERVICE_UNAVAILABLE),
            };
            (status, Json(json!({"error": code}))).into_response()
        }
    }
}

fn quota_json(id: &str, username: &str, q: TrafficQuota) -> Value {
    json!({
        "user_id": id, "username": username, "period": q.period, "utc_offset_minutes": q.utc_offset_minutes,
        "reset_at": q.reset_at, "free_bytes": q.free_bytes.to_string(), "paid_bytes": q.paid_bytes.to_string(),
        "used_bytes": q.used_bytes.to_string(), "uploaded_bytes": q.uploaded_bytes.to_string(),
        "downloaded_bytes": q.downloaded_bytes.to_string(), "legacy_used_bytes": q.legacy_used_bytes.to_string(),
        "limit_bytes": q.limit_bytes.map(|n| n.to_string()),
        "remaining_bytes": q.remaining_bytes.map(|n| n.to_string())
    })
}

async fn quota(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    response(
        async {
            let accounting = broker.relay_accounting.as_ref().ok_or(Problem::Disabled)?;
            let User { user_id } = accounting.read(request).await?;
            if !valid_user_id(&user_id) {
                return Err(Problem::Invalid);
            }
            let name = accounting
                .names
                .get(&user_id)
                .ok_or(Problem::SessionNotFound)?;
            let id = user_id.clone();
            let q = accounting
                .storage(move |ledger| ledger.snapshot(&id))
                .await?;
            Ok(quota_json(&user_id, name, q))
        }
        .await,
    )
}

async fn entitlements(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    response(
        async {
            let accounting = broker.relay_accounting.as_ref().ok_or(Problem::Disabled)?;
            let order: PaidOrder = accounting.read(request).await?;
            let bytes = order.bytes.parse::<u64>().map_err(|_| Problem::Invalid)?;
            if !valid_user_id(&order.user_id)
                || bytes == 0
                || bytes.to_string() != order.bytes
                || !crate::user_policy::valid_traffic_period(&order.period)
                || !(1..=128).contains(&order.order_id.len())
                || !order
                    .order_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                return Err(Problem::Invalid);
            }
            let active = accounting.names.contains_key(&order.user_id);
            let id = order.user_id.clone();
            let period = order.period.clone();
            let key = format!("lmm:{}", order.order_id);
            let granted = accounting
                .storage(move |ledger| ledger.grant(&id, &period, &key, bytes, active))
                .await?;
            Ok(json!({
                "status": if granted { "granted" } else { "replayed" }, "user_id": order.user_id,
                "order_id": order.order_id, "period": order.period, "bytes": order.bytes
            }))
        }
        .await,
    )
}

#[cfg(test)]
mod tests;

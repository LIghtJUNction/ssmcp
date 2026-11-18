//! Authenticated, server-priced traffic checkout. Browser return pages never settle an order.
use super::{Portal, Problem, form, only, view};
use crate::{
    broker::Broker,
    commerce::{
        self, Commerce, CommerceConfig, Error,
        store::{Order, OrderState, Settlement},
    },
};
use axum::{
    Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use std::{path::Path as FsPath, sync::Arc};

impl Portal {
    /// Configure only on the broker that owns the durable account/quota store.
    pub fn configure_commerce(
        &mut self,
        config: CommerceConfig,
        state_dir: &FsPath,
    ) -> crate::Result<()> {
        if config.enabled && self.store.is_none() {
            return Err("commerce requires portal account storage".into());
        }
        self.commerce = Commerce::open(config, state_dir)?.map(Arc::new);
        Ok(())
    }
}
impl Broker {
    /// Recovery joins the durable paid-order intent with the quota ledger's idempotent grant.
    /// Run after portal/quota setup and before accepting traffic; it performs no gateway request.
    pub async fn reconcile_commerce(&self) -> crate::Result<()> {
        let Some(commerce) = &self.portal.commerce else {
            return Ok(());
        };
        if commerce.config.environment == "prod" {
            let subjects = commerce.storage(|store| store.recharged_subjects()).await?;
            for subject in subjects {
                self.record_recharge(&subject)
                    .await
                    .map_err(|_| "could not restore a verified recharge benefit")?;
            }
        }
        let orders = commerce.storage(|store| store.pending_grants()).await?;
        for order in orders {
            grant(self, commerce, order)
                .await
                .map_err(|_| "could not reconcile a verified traffic payment")?;
        }
        Ok(())
    }
}
pub(super) fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/account/traffic", get(traffic))
        .route("/account/traffic/orders", post(purchase))
        .route("/account/traffic/orders/{id}", get(order))
        .route("/account/traffic/orders/{id}/checkout", post(resume))
        .route("/account/traffic/orders/{id}/reconcile", post(reconcile))
        .route("/payments/waffo/webhook", post(webhook))
}
async fn authenticate(portal: &Portal, headers: &HeaderMap) -> Result<(String, String), Problem> {
    let (_, csrf, subject) = portal.session(headers)?;
    let subject = subject.ok_or(Problem::Unauthorized)?;
    let account = portal.storage(move |store| store.account(&subject)).await?;
    portal.charge_management(&account.subject)?;
    if !commerce::valid_subject(&account.subject) {
        return Err(Problem::Unauthorized);
    }
    Ok((account.subject, csrf))
}
fn error(error: Error) -> Response {
    let (status, message) = match error {
        Error::SoldOut => (
            StatusCode::CONFLICT,
            "当月剩余流量不足，请调整数量或下月再购买。",
        ),
        Error::PendingOrder => (
            StatusCode::CONFLICT,
            "你有一笔等待付款的当月订单，请在订单列表继续支付。",
        ),
        Error::CancellationUnconfirmed => (
            StatusCode::CONFLICT,
            "支付方尚未确认可释放的取消终态。库存仍保留；请在收银台取消待付订单后重试核对，或联系管理员。",
        ),
        Error::Expired => (
            StatusCode::CONFLICT,
            "此付款入口已到期，或已接近自然月结束。请勿继续付款。",
        ),
        Error::Disabled | Error::Configuration => {
            (StatusCode::SERVICE_UNAVAILABLE, "流量购买暂未开放。")
        }
        Error::NotFound => (StatusCode::NOT_FOUND, "未找到你的订单。"),
        Error::Invalid => (StatusCode::BAD_REQUEST, "购买数量或确认信息无效。"),
        Error::PriceOutOfRange => (
            StatusCode::BAD_REQUEST,
            "此数量对应的金额超出支付方可受理范围，请调整数量。",
        ),
        Error::Conflict => (StatusCode::CONFLICT, "订单状态已变化，请重新查看订单。"),
        Error::Gateway => (
            StatusCode::BAD_GATEWAY,
            "支付服务暂时不可用。订单已保留，请从订单列表重试。",
        ),
        Error::Storage | Error::TooManyOrders => (
            StatusCode::SERVICE_UNAVAILABLE,
            "订单存储暂不可用，请稍后重新查看。",
        ),
        Error::Signature | Error::PaymentMismatch => {
            (StatusCode::BAD_REQUEST, "支付通知未通过核验。")
        }
    };
    (status,Html(view::page("流量购买",&format!(
        "<section class=card><h1>暂未完成</h1><p role=alert>{message}</p><a class=button href=/account/traffic>查看流量与订单</a></section>"
    )))).into_response()
}
fn state(order: &Order) -> &'static str {
    match order.state {
        OrderState::Reserved => "等待支付确认",
        OrderState::Cancelled => "已核验取消，库存已释放",
        OrderState::Paid => "支付已核验，等待流量入账",
        OrderState::Granted => "流量已入账",
        OrderState::PaidExpired => "跨月到账，待联系管理员核对退款",
    }
}
async fn traffic(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (subject, csrf) = authenticate(&broker.portal, &headers).await?;
    let quota = broker.traffic_quota(&subject).await?;
    let mut body = format!(
        "<section class=card><h1>流量与订单</h1>{}<p>1 GB = 1,000,000,000 字节。</p></section>",
        view::traffic_panel(&quota)
    );
    let Some(commerce) = &broker.portal.commerce else {
        body.push_str("<section class=card><h2>购买流量</h2><p>暂未开放购买。</p></section>");
        return Ok(Html(view::page("流量与订单", &body)).into_response());
    };
    let (period, month_end) = commerce::month(commerce::now(), commerce.config.utc_offset_minutes)
        .map_err(|_| Problem::Storage)?;
    let expires = commerce::expiry_label(month_end, commerce.config.utc_offset_minutes)
        .map_err(|_| Problem::Storage)?;
    body.push_str(&format!("<section class=card><p>本次购买到期时间：<strong>{}</strong>。到时未用完的流量清零。</p></section>", view::escape(&expires)));
    let capacity = commerce.config.monthly_sellable_gb;
    let copy = subject.clone();
    let stock_period = period.clone();
    let (available, orders) = match commerce
        .storage(move |store| {
            Ok((
                store.available_gb(&stock_period, capacity)?,
                store.list(&copy)?,
            ))
        })
        .await
    {
        Ok(value) => value,
        Err(e) => return Ok(error(e)),
    };
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Problem::Storage)?;
    let nonce = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    body.push_str(&format!(
        "<section class=card><h2>购买当月流量</h2><p>{} · 每 GB {} {}（税前）· 当前可售 {} GB</p>",
        view::escape(&period),
        commerce::amount(
            commerce.config.price_per_gb_minor.unwrap_or(0),
            commerce.config.currency_decimals
        ),
        view::escape(&commerce.config.currency),
        available
    ));
    if commerce.config.environment == "test" {
        body.push_str("<p>支付测试环境：此入口仅用于测试。</p>");
    }
    body.push_str("<p>收银台如计税，会在付款前显示税额及含税应付金额。</p>");
    if let Some((minimum, maximum)) = commerce.config.quantity_range(available)
        && quota.remaining_bytes.is_some()
        && month_end.saturating_sub(commerce::now()) >= commerce::CHECKOUT_SECONDS + 60
    {
        body.push_str(&format!(
            "<form method=post action=/account/traffic/orders><input type=hidden name=csrf value=\"{}\"><input type=hidden name=request_nonce value=\"{nonce}\"><input type=hidden name=period value=\"{}\"><label>流量（GB）<input type=number name=quantity_gb value={minimum} min={minimum} max={maximum} required></label><label><input type=checkbox name=acknowledge_month value=yes required>我已了解：本次流量只在 {} 自然月有效，未用完部分下月清零。</label><button type=submit>创建订单</button></form>",
            view::escape(&csrf),view::escape(&period),view::escape(&period)
        ));
    } else if quota.remaining_bytes.is_none() {
        body.push_str("<p>你的流量额度为无限，无需购买。</p>");
    } else {
        body.push_str("<p>当月库存不足或已接近月底，购买已关闭。</p>");
    }
    body.push_str("</section><section class=card><h2>我的订单</h2><ul>");
    for order in orders {
        body.push_str(&format!(
            "<li><a href=\"/account/traffic/orders/{}\">{} · {} GB · {} {} · {}</a></li>",
            view::escape(&order.id),
            view::escape(&order.period),
            order.quantity_gb,
            commerce::amount(order.base_minor, order.currency_decimals),
            view::escape(&order.currency),
            state(&order)
        ));
    }
    body.push_str("</ul></section>");
    Ok(Html(view::page("流量与订单", &body)).into_response())
}
async fn checked_form(
    broker: &Broker,
    request: Request,
    fields: &[&str],
) -> Result<(String, std::collections::HashMap<String, String>), Problem> {
    let (headers, values) = form(request).await?;
    only(&values, fields)?;
    let (_, subject) = broker
        .portal
        .verify(&headers, &values, &broker.public_url)?;
    let (authenticated, _) = authenticate(&broker.portal, &headers).await?;
    if subject.as_deref() != Some(authenticated.as_str()) {
        return Err(Problem::Unauthorized);
    }
    Ok((authenticated, values))
}
async fn purchase(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let (subject, values) = checked_form(
        &broker,
        request,
        &[
            "csrf",
            "quantity_gb",
            "request_nonce",
            "acknowledge_month",
            "period",
        ],
    )
    .await?;
    let Some(commerce) = &broker.portal.commerce else {
        return Ok(error(Error::Disabled));
    };
    if values.get("acknowledge_month").map(String::as_str) != Some("yes") {
        return Ok(error(Error::Invalid));
    }
    if values.get("period").map(String::as_str) != Some(broker.traffic_period().as_str()) {
        return Ok(error(Error::Expired));
    }
    let Some(quantity) = values
        .get("quantity_gb")
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return Ok(error(Error::Invalid));
    };
    let quota = broker.traffic_quota(&subject).await?;
    if quota.limit_bytes.is_none() {
        return Err(Problem::Forbidden);
    }
    let Some(nonce) = values.get("request_nonce") else {
        return Ok(error(Error::Invalid));
    };
    let order = match commerce.reserve(subject, nonce.clone(), quantity).await {
        Ok(o) => o,
        Err(e) => return Ok(error(e)),
    };
    Ok(open_checkout(&broker, commerce, order).await)
}
async fn resume(
    State(broker): State<Arc<Broker>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, Problem> {
    let (subject, _) = checked_form(&broker, request, &["csrf"]).await?;
    let Some(commerce) = &broker.portal.commerce else {
        return Ok(error(Error::Disabled));
    };
    let order = match commerce
        .storage(move |store| store.owned(&subject, &id))
        .await
    {
        Ok(o) => o,
        Err(e) => return Ok(error(e)),
    };
    Ok(open_checkout(&broker, commerce, order).await)
}
async fn open_checkout(broker: &Broker, commerce: &Commerce, order: Order) -> Response {
    match commerce.checkout(order.clone(), &broker.public_url).await {
        Ok(url) => checkout_page(&order, &url),
        Err(e) => error(e),
    }
}
fn checkout_page(order: &Order, url: &str) -> Response {
    let expires = match commerce::expiry_label(order.month_end, order.utc_offset_minutes) {
        Ok(value) => value,
        Err(e) => return error(e),
    };
    Html(view::page("继续付款", &format!(
        "<section class=card><h1>继续付款</h1><p>{} GB · 税前 {} {} · 到期时间 {}</p><p>上下行合计，按 ssmcp 序列化数据帧字节计量，包含协议封装与重试。未用完流量到期清零。</p><p>页面价格为税前基础价；收银台如计税，会在付款前显示税额及含税应付金额。</p><p>流量在支付通知核验后入账。完成付款后可返回查看订单。</p><a class=button href=\"{}\" rel=\"noreferrer noopener\" referrerpolicy=no-referrer>前往收银台</a><a href=\"/account/traffic/orders/{}\">查看订单</a></section>",
        order.quantity_gb, commerce::amount(order.base_minor, order.currency_decimals),
        view::escape(&order.currency), view::escape(&expires), view::escape(url), view::escape(&order.id)
    ))).into_response()
}
async fn reconcile(
    State(broker): State<Arc<Broker>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, Problem> {
    let (subject, _) = checked_form(&broker, request, &["csrf"]).await?;
    let Some(commerce) = &broker.portal.commerce else {
        return Ok(error(Error::Disabled));
    };
    let order = match commerce
        .storage(move |store| store.owned(&subject, &id))
        .await
    {
        Ok(order) => order,
        Err(e) => return Ok(error(e)),
    };
    let id = order.id.clone();
    match commerce.reconcile_cancelled(order).await {
        Ok(()) => Ok(
            axum::response::Redirect::to(&format!("/account/traffic/orders/{id}")).into_response(),
        ),
        Err(e) => Ok(error(e)),
    }
}

async fn order(
    State(broker): State<Arc<Broker>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (subject, csrf) = authenticate(&broker.portal, &headers).await?;
    let Some(commerce) = &broker.portal.commerce else {
        return Ok(error(Error::Disabled));
    };
    let order = match commerce
        .storage(move |store| store.owned(&subject, &id))
        .await
    {
        Ok(o) => o,
        Err(e) => return Ok(error(e)),
    };
    let mut body = format!(
        "<section class=card><h1>流量订单</h1><p>状态：{}</p><p>{} GB · 税前 {} {} · 仅限 {} 自然月使用</p><p>返回本页不会确认付款。到账结果以支付服务的已验证通知为准。</p>",
        state(&order),
        order.quantity_gb,
        commerce::amount(order.base_minor, order.currency_decimals),
        view::escape(&order.currency),
        view::escape(&order.period)
    );
    if let Some(receipt) = &order.receipt {
        body.push_str(&format!(
            "<p>已核验税额：{} {}；含税实收：{} {}</p>",
            commerce::amount(receipt.tax_minor, order.currency_decimals),
            view::escape(&order.currency),
            commerce::amount(receipt.charged_minor, order.currency_decimals),
            view::escape(&order.currency)
        ));
    }
    if order.state == OrderState::Reserved && commerce::now() < order.checkout_deadline {
        body.push_str(&format!(
            "<form method=post action=\"/account/traffic/orders/{}/checkout\"><input type=hidden name=csrf value=\"{}\"><button type=submit>继续付款</button></form>",
            view::escape(&order.id),view::escape(&csrf)
        ));
    } else if order.state == OrderState::Reserved {
        body.push_str(
            "<p>付款入口已到期。若已付款，请等待通知或联系管理员核对；不要重复付款。</p>",
        );
    }
    if order.state == OrderState::Reserved
        && order
            .session_expires_at
            .is_some_and(|end| commerce::now() >= end.saturating_add(60))
    {
        body.push_str(&format!("<form method=post action=\"/account/traffic/orders/{}/reconcile\"><input type=hidden name=csrf value=\"{}\"><button type=submit>核对已取消订单并释放库存</button></form><p>仅在支付方确认取消且没有待付或成功付款时释放库存。查询失败或信息不足会保留订单。</p>",
            view::escape(&order.id), view::escape(&csrf)));
    }
    body.push_str("<a href=/account/traffic>返回流量与订单</a></section>");
    Ok(Html(view::page("流量订单", &body)).into_response())
}
async fn grant(broker: &Broker, commerce: &Commerce, order: Order) -> Result<(), Problem> {
    match broker
        .grant_paid_traffic(&order.subject, &order.period, &order.id, order.bytes)
        .await
    {
        Ok(_) => {
            if commerce.config.environment == "prod" {
                broker.record_recharge(&order.subject).await?;
            }
            commerce
                .storage(move |store| store.mark_granted(&order.id))
                .await
                .map_err(|_| Problem::Storage)
        }
        Err(Problem::TrafficPeriodMismatch) => commerce
            .storage(move |store| store.mark_expired(&order.id))
            .await
            .map_err(|_| Problem::Storage),
        Err(e) => Err(e),
    }
}
async fn webhook(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    let Some(commerce) = &broker.portal.commerce else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let mut headers = request.headers().get_all("x-waffo-signature").iter();
    let Some(signature) = headers
        .next()
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if headers.next().is_some() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(body) = to_bytes(request.into_body(), commerce::pancake::MAX_WEBHOOK).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    match commerce.accept_webhook(&body, &signature).await {
        Ok(Settlement::Grant(order)) => match grant(&broker, commerce, *order).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        Ok(Settlement::AlreadyGranted | Settlement::Ignored | Settlement::Review) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Err(Error::Storage) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

#[cfg(test)]
#[path = "commerce_tests.rs"]
mod tests;

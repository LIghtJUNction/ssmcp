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
        "<section class=\"card payment-error\"><p class=eyebrow>流量与订单</p><h1>暂未完成</h1><p role=alert>{message}</p><a class=button href=/account/traffic>查看流量与订单</a></section>"
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
// Presentation only: these messages follow persisted order states.
fn state_help(order: &Order) -> &'static str {
    match order.state {
        OrderState::Reserved => {
            "尚未收到已验证的支付通知。若已付款，请等待通知或联系管理员核对，避免重复付款。"
        }
        OrderState::Cancelled => "支付方已确认取消，订单预留的库存已释放。",
        OrderState::Paid => "付款已经核验，流量尚待入账。请稍后查看；状态长时间不变时联系管理员。",
        OrderState::Granted => "本订单流量已记入所属自然月额度。到期时未用完的部分不结转。",
        OrderState::PaidExpired => {
            "付款跨月到达，未授予当前月份流量。请联系管理员核对退款；此状态不代表退款已完成。"
        }
    }
}

async fn traffic(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (subject, csrf) = authenticate(&broker.portal, &headers).await?;
    let quota = broker.traffic_quota(&subject).await?;
    let mut body = format!(
        "<div class=commerce-page><nav class=account-breadcrumb aria-label=\"当前位置\"><a href=/>账户</a><span aria-hidden=true>/</span><span>流量与订单</span></nav><header class=commerce-heading><p class=eyebrow>TRAFFIC &amp; BILLING</p><h1>流量与订单</h1><p>先看本月用量，再按需要购买。</p></header><section class=\"quota-overview account-traffic\" aria-label=\"当前账户用量\">{}<p class=billing-unit>1 GB = 1,000,000,000 字节。</p></section>",
        view::traffic_panel(&quota)
    );
    let Some(commerce) = &broker.portal.commerce else {
        body.push_str("<section class=commerce-unavailable><h2>购买流量</h2><p>暂未开放购买。</p><a href=/>返回账户</a></section></div>");
        return Ok(Html(view::page("流量与订单", &body)).into_response());
    };
    let (period, month_end) = commerce::month(commerce::now(), commerce.config.utc_offset_minutes)
        .map_err(|_| Problem::Storage)?;
    let expires = commerce::expiry_label(month_end, commerce.config.utc_offset_minutes)
        .map_err(|_| Problem::Storage)?;
    body.push_str(&format!("<aside class=purchase-notice aria-label=\"流量有效期\"><p>本次购买到期时间：<strong>{}</strong></p><p>仅限所属自然月使用，到时未用完的流量清零。</p></aside>", view::escape(&expires)));
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
        "<div class=commerce-grid><section class=purchase-card aria-labelledby=\"purchase-title\"><p class=eyebrow>购买流量</p><h2 id=purchase-title>购买当月流量</h2><dl class=price-facts><div><dt>适用月份</dt><dd>{}</dd></div><div><dt>每 GB 税前单价</dt><dd>{} {}</dd></div><div><dt>当前可售</dt><dd>{} GB</dd></div></dl>",
        view::escape(&period),
        commerce::amount(
            commerce.config.price_per_gb_minor.unwrap_or(0),
            commerce.config.currency_decimals
        ),
        view::escape(&commerce.config.currency),
        available
    ));
    if commerce.config.environment == "test" {
        body.push_str(
            "<p class=payment-test-notice role=note>支付测试环境：此入口仅用于测试。</p>",
        );
    }
    body.push_str("<p class=billing-note>收银台如计税，会在付款前显示税额及含税应付金额。</p>");
    if let Some((minimum, maximum)) = commerce.config.quantity_range(available)
        && quota.remaining_bytes.is_some()
        && month_end.saturating_sub(commerce::now()) >= commerce::CHECKOUT_SECONDS + 60
    {
        let unit_price = commerce
            .config
            .price_per_gb_minor
            .expect("validated unit price");
        let decimals = commerce.config.currency_decimals;
        let min_amount = commerce::amount(minimum * unit_price, decimals);
        let max_amount = commerce::amount(maximum * unit_price, decimals);
        let step = commerce::amount(unit_price, decimals);
        let currency = view::escape(&commerce.config.currency);
        body.push_str(&format!(
            "<form method=post action=/account/traffic/orders><input type=hidden name=csrf value=\"{}\"><input type=hidden name=request_nonce value=\"{nonce}\"><input type=hidden name=period value=\"{}\"><div class=amount-field><label>税前金额（{currency}）<input type=number name=amount value={min_amount} min={min_amount} max={max_amount} step={step} required></label><p class=field-help>每增加 {step} {currency} 对应 1 GB。可选金额：{min_amount}–{max_amount} {currency}。</p></div><label class=month-consent><input type=checkbox name=acknowledge_month value=yes required>我已了解：本次流量只在 {} 自然月有效，未用完部分下月清零。</label><div class=purchase-submit><button type=submit>创建订单</button><p>下一步核对信息，再前往收银台付款。</p></div></form>",
            view::escape(&csrf),view::escape(&period),view::escape(&period)
        ));
    } else if quota.remaining_bytes.is_none() {
        body.push_str("<p class=purchase-unavailable>你的流量额度为无限，无需购买。</p>");
    } else {
        body.push_str("<p class=purchase-unavailable>当月库存不足或已接近月底，购买已关闭。</p>");
    }
    body.push_str("</section><aside class=purchase-explainer aria-labelledby=\"payment-flow-title\"><h2 id=payment-flow-title>付款会经过三步</h2><ol><li><strong>确认金额</strong><p>核对数量、币种和本月到期时间。</p></li><li><strong>前往收银台</strong><p>如有税费，在付款前查看含税应付金额。</p></li><li><strong>查看入账状态</strong><p>服务端核验支付通知后入账，返回页面不等于支付成功。</p></li></ol><a href=\"/guide/zh#boundaries\">了解计费与状态</a></aside></div><section class=orders-section aria-labelledby=\"orders-title\"><div class=orders-heading><h2 id=orders-title>我的订单</h2><p>查看真实支付与入账进度。</p></div>");
    if orders.is_empty() {
        body.push_str("<p class=orders-empty>还没有订单。需要更多流量时，可以在上方创建。</p>");
    }
    body.push_str("<ul class=order-list>");
    for order in orders {
        body.push_str(&format!(
            "<li><a class=order-row href=\"/account/traffic/orders/{}\"><span class=order-period><span class=order-label>适用月份</span><strong>{}</strong></span><span class=order-quantity><strong>{} GB</strong><span class=order-label>流量额度</span></span><span class=order-amount><strong>{} {}</strong><span class=order-label>税前金额</span></span><span class=order-status>{}</span><span class=order-arrow aria-hidden=true>↗</span></a></li>",
            view::escape(&order.id),
            view::escape(&order.period),
            order.quantity_gb,
            commerce::amount(order.base_minor, order.currency_decimals),
            view::escape(&order.currency),
            state(&order)
        ));
    }
    body.push_str("</ul></section></div>");
    Ok(Html(view::page("流量与订单", &body)).into_response())
}
async fn checked_form(
    broker: &Broker,
    request: Request,
    field_sets: &[&[&str]],
) -> Result<(String, std::collections::HashMap<String, String>), Problem> {
    let (headers, values) = form(request).await?;
    if !field_sets
        .iter()
        .any(|fields| only(&values, fields).is_ok())
    {
        return Err(Problem::Invalid);
    }
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
            &[
                "csrf",
                "amount",
                "request_nonce",
                "acknowledge_month",
                "period",
            ],
            &[
                "csrf",
                "quantity_gb",
                "request_nonce",
                "acknowledge_month",
                "period",
            ],
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
    let quantity = match (values.get("amount"), values.get("quantity_gb")) {
        (Some(amount), None) => {
            let unit_price = commerce
                .config
                .price_per_gb_minor
                .expect("validated unit price");
            commerce::parse_amount(amount, commerce.config.currency_decimals)
                .filter(|minor| *minor > 0 && minor % unit_price == 0)
                .map(|minor| minor / unit_price)
        }
        (None, Some(quantity)) => quantity.parse::<u64>().ok(),
        _ => None,
    };
    let Some(quantity) = quantity else {
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
    let (subject, _) = checked_form(&broker, request, &[&["csrf"]]).await?;
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
        "<section class=\"card checkout-card\"><p class=eyebrow>订单已创建 · 等待支付确认</p><h1>核对订单，继续付款</h1><ol class=payment-steps aria-label=\"付款流程\"><li>1 确认金额</li><li aria-current=step>2 前往收银台</li><li>3 查看入账</li></ol><dl class=checkout-summary><div><dt>流量额度</dt><dd>{} GB</dd></div><div class=checkout-total><dt>税前金额</dt><dd>{} {}</dd></div><div><dt>到期时间</dt><dd>{}</dd></div></dl><p class=billing-note>上下行合计，按 ssmcp 序列化数据帧字节计量，包含协议封装与重试。未用完流量到期清零。</p><p>页面价格为税前基础价；收银台如计税，会在付款前显示税额及含税应付金额。</p><p class=payment-confirmation-note>流量在支付通知核验后入账。完成付款后可返回查看订单；回到此页本身不会确认付款。</p><div class=checkout-actions><a class=button href=\"{}\" rel=\"noreferrer noopener\" referrerpolicy=no-referrer>前往收银台 <span aria-hidden=true>↗</span></a><a href=\"/account/traffic/orders/{}\">查看订单状态</a></div></section>",
        order.quantity_gb, commerce::amount(order.base_minor, order.currency_decimals),
        view::escape(&order.currency), view::escape(&expires), view::escape(url), view::escape(&order.id)
    ))).into_response()
}
async fn reconcile(
    State(broker): State<Arc<Broker>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, Problem> {
    let (subject, _) = checked_form(&broker, request, &[&["csrf"]]).await?;
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
        "<section class=\"card order-detail\"><p class=eyebrow>流量与订单</p><h1>流量订单</h1><p class=order-state><span>当前状态</span><strong>{}</strong></p><dl class=checkout-summary><div><dt>流量额度</dt><dd>{} GB</dd></div><div class=checkout-total><dt>税前金额</dt><dd>{} {}</dd></div><div><dt>适用月份</dt><dd>{} 自然月</dd></div></dl><p class=payment-confirmation-note>返回本页不会确认付款。到账结果以支付服务的已验证通知为准。</p>",
        state(&order),
        order.quantity_gb,
        commerce::amount(order.base_minor, order.currency_decimals),
        view::escape(&order.currency),
        view::escape(&order.period)
    );
    body.push_str(&format!(
        "<p class=order-state-help>{}</p>",
        state_help(&order)
    ));
    if let Some(receipt) = &order.receipt {
        body.push_str(&format!(
            "<dl class=verified-payment><div><dt>已核验税额</dt><dd>{} {}</dd></div><div><dt>含税实收</dt><dd>{} {}</dd></div></dl>",
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
    body.push_str(
        "<div class=order-return><a href=/account/traffic>返回流量与订单</a></div></section>",
    );
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

//! The executable's private HTTPS/WebSocket transport. The parent owns the
//! shell and tool processes; this child only relays bounded protocol frames.
use super::account::Credential;
use crate::wire::{Frame, MAX_FRAME, MAX_INFLIGHT, read_frame, write_frame};
use futures_util::{SinkExt, StreamExt};
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::{io::BufReader, process::Command, sync::mpsc, time::timeout};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        self, Message,
        client::IntoClientRequest,
        http::{HeaderValue, StatusCode},
        protocol::WebSocketConfig,
    },
};

pub(super) const AUTH_EXPIRED: &str = "ssmcp authorization expired or revoked";
pub(super) const LEGACY_ACCOUNT_ALLOW: &str =
    "allow entries must use username@github; only GitHub is supported";
const UNSUPPORTED: &str = "server does not support account clients; upgrade ssmcpd";

const QUOTA_CODE: &str = "traffic_quota_exceeded:";

fn is_quota_failure(message: &str) -> bool {
    message.starts_with(QUOTA_CODE)
        || message
            .strip_prefix("SSH server rejected the connection: ")
            .is_some_and(|message| message.starts_with(QUOTA_CODE))
}

fn quota_rejection(server: &str, _body: Option<&[u8]>) -> crate::Result<String> {
    // A 402 from this authenticated endpoint means the connection has no quota.
    // Error-body prose and URLs are untrusted display input. Construct both the
    // explanation and recharge link locally, including for older bodyless replies.
    let origin = super::account::normalize_server(server)?;
    Ok(format!(
        "traffic_quota_exceeded: 本月剩余流量不足。请求未发送到客户端，未执行。补充流量后可继续发起新请求，或等待下月额度恢复；本次请求不会自动重放。流量与充值：{origin}/account/traffic"
    ))
}

#[derive(Debug)]
pub(super) struct PermanentFailure(pub String);

impl std::fmt::Display for PermanentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PermanentFailure {}

pub(super) fn failure(message: String) -> crate::Error {
    if message == AUTH_EXPIRED
        || message == UNSUPPORTED
        || message == LEGACY_ACCOUNT_ALLOW
        || is_quota_failure(&message)
    {
        Box::new(PermanentFailure(message))
    } else {
        message.into()
    }
}

/// Secrets are passed in this one child's environment, never argv, session
/// manifests, connection output or inherited native-tool environments.
pub(super) fn command(executable: &Path, server: &str, credential: Option<&Credential>) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("__relay")
        .env("SSMCP_RELAY_SERVER", server)
        .env_remove("SSMCP_RELAY_TOKEN")
        .env_remove(super::link_metrics::ENV)
        .env_remove(super::link_metrics::ID_ENV)
        .env(
            "SSMCP_RELAY_CAPABILITIES",
            format!(
                "{},{},{},{},{},{},{}",
                crate::wire::SESSION_ACCESS_CAPABILITY,
                crate::wire::CONTROL_ATTRIBUTION_CAPABILITY,
                crate::wire::messages::CAPABILITY,
                crate::wire::versions::CAPABILITY,
                crate::wire::wait_display::CAPABILITY,
                crate::wire::wait_display::TIME_CAPABILITY,
                crate::wire::openai::CAPABILITY
            ),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(capability) = crate::peer_port::offered_capability() {
        let offer = parent_offer(&command).expect("built-in relay capability offer");
        command.env("SSMCP_RELAY_CAPABILITIES", format!("{offer},{capability}"));
    }
    if let Some(credential) = credential {
        command.env("SSMCP_RELAY_TOKEN", &credential.token);
    }
    command
}

/// Old self-only account connections must not negotiate delegation authority.
pub(super) fn restrict_to_self(command: &mut Command) {
    command.env(
        "SSMCP_RELAY_CAPABILITIES",
        crate::wire::CONTROL_ATTRIBUTION_CAPABILITY,
    );
}

pub(super) fn offer_chat_disabled(command: &mut Command) {
    let offer = command
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == "SSMCP_RELAY_CAPABILITIES")
        .and_then(|(_, value)| value)
        .and_then(|value| value.to_str())
        .map_or_else(String::new, |value| format!("{value},"));
    command.env(
        "SSMCP_RELAY_CAPABILITIES",
        format!("{offer}{}", crate::wire::CHAT_DISABLED_CAPABILITY),
    );
}

/// The actual parent-generated offer, including the self-only override.
fn parent_offer(command: &Command) -> Option<&str> {
    command
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == "SSMCP_RELAY_CAPABILITIES")
        .and_then(|(_, value)| value)
        .and_then(|value| value.to_str())
        .filter(|value| access_capability_header(Some(value)).is_some())
}

pub(super) fn peer_ports_offer(command: &Command) -> Option<&'static str> {
    let offer = parent_offer(command)?;
    [
        crate::wire::peer_port::CAPABILITY,
        crate::wire::peer_port::AUTO_CAPABILITY,
    ]
    .into_iter()
    .find(|wanted| offer.split(',').any(|name| name == *wanted))
}

pub(super) fn negotiated_peer_ports(
    offer: Option<&'static str>,
    capabilities: &[String],
) -> Option<&'static str> {
    use crate::wire::peer_port::{AUTO_CAPABILITY, CAPABILITY};
    let has = |name: &str| capabilities.iter().any(|value| value == name);
    if !has(crate::wire::SESSION_ACCESS_CAPABILITY)
        || !has(crate::wire::CONTROL_ATTRIBUTION_CAPABILITY)
    {
        return None;
    }
    offer.filter(|offered| {
        has(offered)
            && match *offered {
                CAPABILITY => !has(AUTO_CAPABILITY),
                AUTO_CAPABILITY => !has(CAPABILITY),
                _ => false,
            }
    })
}

pub(super) fn openai_context_offer(command: &Command) -> bool {
    parent_offer(command).is_some_and(|offer| {
        offer
            .split(',')
            .any(|value| value.trim() == crate::wire::openai::CAPABILITY)
    })
}

pub(super) fn timing_offer(command: &Command) -> (bool, bool) {
    let Some(offer) = parent_offer(command) else {
        return (false, false);
    };
    let has = |name| offer.split(',').any(|value| value.trim() == name);
    (
        has(crate::wire::wait_display::TIME_CAPABILITY),
        has(crate::wire::wait_display::CAPABILITY),
    )
}

pub(super) fn negotiated_timing(offer: (bool, bool), capabilities: &[String]) -> (bool, bool) {
    let has = |name| capabilities.iter().any(|value| value == name);
    let current_identity = has(crate::wire::SESSION_ACCESS_CAPABILITY)
        && has(crate::wire::CONTROL_ATTRIBUTION_CAPABILITY);
    (
        current_identity && offer.0 && has(crate::wire::wait_display::TIME_CAPABILITY),
        current_identity && offer.1 && has(crate::wire::wait_display::CAPABILITY),
    )
}

fn access_capability_header(parent_offer: Option<&str>) -> Option<HeaderValue> {
    let offer = parent_offer.filter(|offer| offer.len() <= 256)?;
    let capabilities: Vec<_> = offer.split(',').map(str::trim).collect();
    if capabilities.iter().enumerate().any(|(index, value)| {
        !matches!(
            *value,
            crate::wire::SESSION_ACCESS_CAPABILITY
                | crate::wire::CONTROL_ATTRIBUTION_CAPABILITY
                | crate::wire::messages::CAPABILITY
                | crate::wire::versions::CAPABILITY
                | crate::wire::wait_display::CAPABILITY
                | crate::wire::wait_display::TIME_CAPABILITY
                | crate::wire::CHAT_DISABLED_CAPABILITY
                | crate::wire::openai::CAPABILITY
                | crate::wire::peer_port::CAPABILITY
                | crate::wire::peer_port::AUTO_CAPABILITY
        ) || capabilities[..index].contains(value)
    }) {
        return None;
    }
    if capabilities.contains(&crate::wire::peer_port::CAPABILITY)
        && capabilities.contains(&crate::wire::peer_port::AUTO_CAPABILITY)
    {
        return None;
    }
    HeaderValue::from_str(&capabilities.join(",")).ok()
}

fn websocket_url(server: &str) -> crate::Result<String> {
    let origin = super::account::normalize_server(server)?;
    let mut url = reqwest::Url::parse(&origin)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).map_err(|_| "invalid relay scheme")?;
    url.set_path("/api/client/connect");
    Ok(url.into())
}

fn decode_frame(bytes: &[u8]) -> io::Result<Frame> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay frame exceeds size limit",
        ));
    }
    serde_json::from_slice(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid relay protocol frame"))
}

pub(super) async fn run() -> crate::Result<()> {
    let mut metrics = super::link_metrics::Recorder::from_environment();
    let server = std::env::var("SSMCP_RELAY_SERVER")
        .map_err(|_| "private relay must be started by ssmcp")?;
    let token = std::env::var("SSMCP_RELAY_TOKEN").ok();
    let mut request = websocket_url(&server)?.into_client_request()?;
    if let Some(token) = &token {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "invalid client credential")?;
        authorization.set_sensitive(true);
        request.headers_mut().insert("authorization", authorization);
    }
    // The parent, not just this helper, must understand the optional Ready fields.
    // This also supports an old running client that starts a newer relay after update.
    // Older servers ignore the HTTP offer and keep the strict v7 Ready shape.
    if let Some(capability) =
        access_capability_header(std::env::var("SSMCP_RELAY_CAPABILITIES").ok().as_deref())
    {
        request
            .headers_mut()
            .insert(crate::wire::CAPABILITIES_HEADER, capability);
    }
    let configuration = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let connected = timeout(
        Duration::from_secs(30),
        connect_async_tls_with_config(
            request,
            Some(configuration),
            true,
            super::trust::tls_config(&server)?.map(Connector::Rustls),
        ),
    )
    .await
    .map_err(|_| "relay connection timed out")?;
    let websocket = match connected {
        Ok((socket, _)) => socket,
        Err(tungstenite::Error::Http(response))
            if response.status() == StatusCode::PAYMENT_REQUIRED =>
        {
            write_frame(
                &mut tokio::io::stdout(),
                &Frame::Reject {
                    message: quota_rejection(&server, response.body().as_deref())?,
                },
            )
            .await?;
            return Ok(());
        }
        Err(tungstenite::Error::Http(response))
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) =>
        {
            write_frame(
                &mut tokio::io::stdout(),
                &Frame::Reject {
                    message: if token.is_some() {
                        AUTH_EXPIRED.into()
                    } else {
                        UNSUPPORTED.into()
                    },
                },
            )
            .await?;
            return Ok(());
        }
        Err(tungstenite::Error::Http(response))
            if matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ) =>
        {
            write_frame(
                &mut tokio::io::stdout(),
                &Frame::Reject {
                    message: UNSUPPORTED.into(),
                },
            )
            .await?;
            return Ok(());
        }
        Err(error) => return Err(format!("encrypted relay connection failed: {error}").into()),
    };
    let (mut outgoing, mut incoming) = websocket.split();
    // Own the whole partial stdin frame in one task. Cancelling a read_frame
    // inside select would discard bytes already consumed from the pipe.
    let (sender, mut frames) = mpsc::channel(MAX_INFLIGHT);
    let stdin_task = tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin());
        loop {
            let value = read_frame(&mut stdin).await;
            let done = !matches!(&value, Ok(Some(_)));
            if sender.send(value).await.is_err() || done {
                break;
            }
        }
    });
    let mut stdout = tokio::io::stdout();
    let mut metrics_tick = tokio::time::interval(Duration::from_secs(1));
    metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = async {
        loop {
            tokio::select! {
                frame = frames.recv() => {
                    match frame {
                        Some(Ok(Some(frame))) => {
                            send_application(&mut outgoing, &frame, &mut metrics).await?;
                        }
                        Some(Err(error)) => return Err(error.into()),
                        Some(Ok(None)) | None => {
                            let _ = timeout(Duration::from_secs(2), outgoing.close()).await;
                            return Ok(());
                        }
                    }
                }
                message = incoming.next() => {
                    let frame = match message {
                        Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                            application_frame(message, &mut metrics)?
                        }
                        Some(Ok(Message::Ping(bytes))) => {
                            timeout(Duration::from_secs(15), outgoing.send(Message::Pong(bytes)))
                                .await.map_err(|_| "relay pong timed out")??;
                            None
                        }
                        Some(Ok(Message::Close(reason))) => {
                            if reason.is_some_and(|reason| u16::from(reason.code) == 4001) {
                                write_frame(&mut stdout, &Frame::Reject {message: AUTH_EXPIRED.into()}).await?;
                            }
                            return Ok(());
                        }
                        Some(Ok(_)) => None,
                        Some(Err(tungstenite::Error::ConnectionClosed)) | None => return Ok(()),
                        Some(Err(error)) => return Err(format!("relay disconnected: {error}").into()),
                    };
                    if let Some(frame) = frame {
                        timeout(Duration::from_secs(15), write_frame(&mut stdout, &frame))
                            .await.map_err(|_| "local relay output timed out")??;
                    }
                }
                _ = metrics_tick.tick(), if metrics.enabled() => metrics.publish(),
                _ = heartbeat.tick() => {
                    timeout(Duration::from_secs(15), outgoing.send(Message::Ping(Vec::new().into())))
                        .await.map_err(|_| "relay heartbeat timed out")??;
                }
            }
        }
    }.await;
    stdin_task.abort();
    result
}

async fn send_application<S>(
    outgoing: &mut S,
    frame: &Frame,
    metrics: &mut super::link_metrics::Recorder,
) -> crate::Result<()>
where
    S: futures_util::Sink<Message, Error = tungstenite::Error> + Unpin,
{
    let mut encoded = serde_json::to_vec(frame)?;
    encoded.push(b'\n');
    if encoded.len() > MAX_FRAME {
        return Err("outgoing relay frame exceeds size limit".into());
    }
    let bytes = encoded.len();
    timeout(
        Duration::from_secs(15),
        outgoing.send(Message::Binary(encoded.into())),
    )
    .await
    .map_err(|_| "relay send timed out")??;
    metrics.sent(bytes);
    Ok(())
}

fn application_frame(
    message: Message,
    metrics: &mut super::link_metrics::Recorder,
) -> io::Result<Option<Frame>> {
    match message {
        Message::Text(text) => {
            metrics.received(text.len());
            decode_frame(text.as_bytes()).map(Some)
        }
        Message::Binary(bytes) => {
            metrics.received(bytes.len());
            decode_frame(&bytes).map(Some)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
#[path = "relay/metrics_tests.rs"]
mod metrics_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_capability_header_requires_explicit_support_from_the_parent() {
        assert!(access_capability_header(None).is_none());
        assert!(access_capability_header(Some("session_access_v10")).is_none());
        assert!(access_capability_header(Some("")).is_none());
        assert_eq!(
            access_capability_header(Some(crate::wire::SESSION_ACCESS_CAPABILITY)).unwrap(),
            crate::wire::SESSION_ACCESS_CAPABILITY
        );
        let both = format!(
            "{},{}",
            crate::wire::SESSION_ACCESS_CAPABILITY,
            crate::wire::CONTROL_ATTRIBUTION_CAPABILITY
        );
        assert_eq!(access_capability_header(Some(&both)).unwrap(), both);
        let all = format!("{both},{}", crate::wire::messages::CAPABILITY);
        assert_eq!(access_capability_header(Some(&all)).unwrap(), all);
        let duplicate_messages = format!("{all},{}", crate::wire::messages::CAPABILITY);
        assert!(access_capability_header(Some(&duplicate_messages)).is_none());
        let versions = format!("{all},{}", crate::wire::versions::CAPABILITY);
        assert_eq!(access_capability_header(Some(&versions)).unwrap(), versions);
        assert!(
            access_capability_header(Some(&format!(
                "{versions},{}",
                crate::wire::versions::CAPABILITY
            )))
            .is_none()
        );
        for invalid in [
            "control_attribution_v10",
            "control_attribution_v1,admin",
            "control_attribution_v1,control_attribution_v1",
        ] {
            assert!(access_capability_header(Some(invalid)).is_none());
        }
    }

    #[test]
    fn peer_candidates_require_exact_mode_and_current_authority_echo() {
        use crate::wire::peer_port::{AUTO_CAPABILITY, CAPABILITY};
        let mut parent = Command::new("/fixture/ssmcp");
        for capability in [CAPABILITY, AUTO_CAPABILITY] {
            let offer = format!(
                "{},{},{capability}",
                crate::wire::SESSION_ACCESS_CAPABILITY,
                crate::wire::CONTROL_ATTRIBUTION_CAPABILITY
            );
            parent.env("SSMCP_RELAY_CAPABILITIES", &offer);
            assert_eq!(peer_ports_offer(&parent), Some(capability));
            let capabilities: Vec<String> = offer.split(',').map(String::from).collect();
            assert_eq!(
                negotiated_peer_ports(peer_ports_offer(&parent), &capabilities),
                Some(capability)
            );
            assert_eq!(
                negotiated_peer_ports(peer_ports_offer(&parent), &capabilities[..2]),
                None
            );
            assert_eq!(
                negotiated_peer_ports(peer_ports_offer(&parent), &capabilities[2..]),
                None
            );
            let other = if capability == CAPABILITY {
                AUTO_CAPABILITY
            } else {
                CAPABILITY
            };
            assert_eq!(negotiated_peer_ports(Some(other), &capabilities), None);
            let mut mixed = capabilities.clone();
            mixed.push(other.into());
            assert_eq!(negotiated_peer_ports(Some(capability), &mixed), None);
            assert_eq!(negotiated_peer_ports(Some(other), &mixed), None);
            restrict_to_self(&mut parent);
            assert_eq!(peer_ports_offer(&parent), None);
            assert_eq!(
                negotiated_peer_ports(peer_ports_offer(&parent), &capabilities),
                None
            );
        }
        assert!(
            access_capability_header(Some(&format!("{CAPABILITY},{AUTO_CAPABILITY}"))).is_none()
        );
        assert!(access_capability_header(Some("peer_ports_auto_v10")).is_none());
    }

    #[test]
    fn quota_http_rejection_uses_only_the_validated_server_origin() {
        let server = "https://relay.example";
        let expected = quota_rejection(server, None).unwrap();
        assert!(expected.starts_with(QUOTA_CODE));
        assert!(expected.contains("未执行"));
        assert!(expected.ends_with("https://relay.example/account/traffic"));
        for recharge in [
            "https://relay.example/account/traffic",
            "https://account.example/account/traffic",
            "https://user:password@relay.example/account/traffic",
            "https://relay.example/unknown",
            "https://relay.example/account/traffic?token=secret",
            "javascript:alert(1)",
        ] {
            let body = serde_json::to_vec(&serde_json::json!({
                "error": "traffic_quota_exceeded",
                "message": "traffic_quota_exceeded: visit https://attacker.example/unknown\n\u{1b}[31m",
                "recharge_url": recharge,
            })).unwrap();
            assert_eq!(quota_rejection(server, Some(&body)).unwrap(), expected);
        }
        assert_eq!(
            quota_rejection(server, Some(b"not json")).unwrap(),
            expected
        );
        assert_eq!(
            quota_rejection("HTTPS://RELAY.EXAMPLE:443/", None).unwrap(),
            expected
        );
        assert!(quota_rejection("https://user:password@relay.example", None).is_err());
        #[cfg(target_os = "linux")]
        assert_eq!(
            expected,
            crate::user_policy::TrafficError::Exhausted
                .notice(server, crate::user_policy::TrafficOutcome::NotDispatched)
        );
    }

    #[test]
    fn quota_rejection_stops_reconnects_without_treating_other_failures_as_quota() {
        for message in [
            quota_rejection("https://relay.example/", None).unwrap(),
            format!(
                "SSH server rejected the connection: {}",
                quota_rejection("https://relay.example", None).unwrap()
            ),
        ] {
            assert!(
                failure(message)
                    .downcast_ref::<PermanentFailure>()
                    .is_some()
            );
        }
        assert!(
            failure("encrypted relay connection failed".into())
                .downcast_ref::<PermanentFailure>()
                .is_none()
        );
    }

    #[test]
    fn relay_transport_requires_an_origin_and_secure_remote_connection() {
        assert_eq!(
            websocket_url("https://example.com").unwrap(),
            "wss://example.com/api/client/connect"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080/api/client/connect"
        );
        for origin in [
            "http://example.com",
            "https://token@example.com",
            "https://example.com/path",
            "https://example.com?token=secret",
            "https://example.com/#secret",
            "file:///tmp/socket",
        ] {
            assert!(websocket_url(origin).is_err(), "{origin}");
        }
    }

    #[test]
    fn timing_requires_the_real_parent_offer_and_explicit_current_ready_echo() {
        let mut parent = command(Path::new("/fixture/ssmcp"), "https://fixture.test", None);
        let offered = timing_offer(&parent);
        assert_eq!(offered, (true, true));
        let capabilities: Vec<String> = [
            crate::wire::SESSION_ACCESS_CAPABILITY,
            crate::wire::CONTROL_ATTRIBUTION_CAPABILITY,
            crate::wire::wait_display::TIME_CAPABILITY,
            crate::wire::wait_display::CAPABILITY,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(negotiated_timing(offered, &[]), (false, false));
        assert_eq!(negotiated_timing(offered, &capabilities), (true, true));
        assert_eq!(
            negotiated_timing(offered, &capabilities[2..]),
            (false, false)
        );
        assert_eq!(
            negotiated_timing((false, false), &capabilities),
            (false, false)
        );
        restrict_to_self(&mut parent);
        assert_eq!(timing_offer(&parent), (false, false));
        assert_eq!(
            negotiated_timing(timing_offer(&parent), &capabilities),
            (false, false)
        );
    }

    #[test]
    fn relay_messages_cannot_smuggle_multiple_frames() {
        let frame = serde_json::to_vec(&Frame::Reject {
            message: "test".into(),
        })
        .unwrap();
        assert!(matches!(
            decode_frame(&frame).unwrap(),
            Frame::Reject { .. }
        ));
        let mut doubled = frame.clone();
        doubled.push(b'\n');
        doubled.extend(frame);
        assert!(decode_frame(&doubled).is_err());
        assert!(decode_frame(&vec![b' '; MAX_FRAME + 1]).is_err());
    }
}

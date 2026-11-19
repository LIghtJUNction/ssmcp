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
        "traffic_quota_exceeded: 本月剩余流量不足。请求未发送到客户端，未执行。充值后请重新连接客户端，或等待下月额度恢复。流量与充值：{origin}/account/traffic"
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
    if message == AUTH_EXPIRED || message == UNSUPPORTED || is_quota_failure(&message) {
        Box::new(PermanentFailure(message))
    } else {
        message.into()
    }
}

/// Secrets are passed in this one child's environment, never argv, session
/// manifests, connection output or inherited native-tool environments.
pub(super) fn command(executable: &Path, credential: &Credential) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("__relay")
        .env("SSMCP_RELAY_SERVER", &credential.server)
        .env("SSMCP_RELAY_TOKEN", &credential.token)
        .env(
            "SSMCP_RELAY_CAPABILITIES",
            crate::wire::SESSION_ACCESS_CAPABILITY,
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn access_capability_header(parent_offer: Option<&str>) -> Option<HeaderValue> {
    (parent_offer == Some(crate::wire::SESSION_ACCESS_CAPABILITY))
        .then(|| HeaderValue::from_static(crate::wire::SESSION_ACCESS_CAPABILITY))
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
    let server = std::env::var("SSMCP_RELAY_SERVER")
        .map_err(|_| "private relay must be started by ssmcp")?;
    let token = std::env::var("SSMCP_RELAY_TOKEN")
        .map_err(|_| "private relay requires a client credential")?;
    let mut request = websocket_url(&server)?.into_client_request()?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "invalid client credential")?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("authorization", authorization);
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
                    message: AUTH_EXPIRED.into(),
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
    let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = async {
        loop {
            tokio::select! {
                frame = frames.recv() => {
                    match frame {
                        Some(Ok(Some(frame))) => {
                            let mut encoded = serde_json::to_vec(&frame)?;
                            encoded.push(b'\n');
                            if encoded.len() > MAX_FRAME {
                                return Err("outgoing relay frame exceeds size limit".into());
                            }
                            timeout(Duration::from_secs(15), outgoing.send(Message::Binary(encoded.into())))
                                .await.map_err(|_| "relay send timed out")??;
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
                        Some(Ok(Message::Text(text))) => Some(decode_frame(text.as_bytes())?),
                        Some(Ok(Message::Binary(bytes))) => Some(decode_frame(&bytes)?),
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

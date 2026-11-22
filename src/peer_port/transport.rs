//! TLS 1.3 mutual authentication with a fresh local identity per offer. Only
//! the authenticated partner's ephemeral certificate is trusted for this mapping.
use super::{CandidatePolicy, Pending, Policy, Result, Role, UNREACHABLE};
use crate::wire::peer_port::Offer;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{StreamExt, future::select_all, stream};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName},
    server::WebPkiClientVerifier,
};
use serde_json::{Value, json};
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub(super) const DNS: &str = "ssmcp-peer.invalid";
const ATTEMPT: Duration = Duration::from_secs(2);
const PROBE: u8 = 0;
const STREAM: u8 = 1;
const READY: u8 = 2;
const MAX_STREAMS: usize = 2;

#[derive(Clone)]
enum Side {
    Listen(TlsConnector, SocketAddr),
    Target(TlsAcceptor, SocketAddr),
}

pub(super) async fn activate(
    policy: &Policy,
    pending: Pending,
    peer: Offer,
    lease: watch::Receiver<Instant>,
) -> Result<(Value, JoinHandle<()>)> {
    let mut roots = RootCertStore::empty();
    let certificate = STANDARD
        .decode(peer.certificate)
        .map_err(|_| "invalid peer certificate")?;
    roots
        .add(CertificateDer::from(certificate))
        .map_err(|_| "invalid peer certificate")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = vec![pending.identity.cert.der().clone()];
    let key = PrivatePkcs8KeyDer::from(pending.identity.signing_key.serialize_der()).into();
    let (listeners, side, description) = match policy.role {
        Role::Target => {
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                    .build()
                    .map_err(|e| e.to_string())?;
            let config = ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .map_err(|e| e.to_string())?;
            let listeners = match &policy.candidates {
                CandidatePolicy::Fixed { bind, .. } => {
                    vec![TcpListener::bind(bind).await.map_err(|e| match e.kind() {
                        io::ErrorKind::AddrInUse | io::ErrorKind::AddrNotAvailable => UNREACHABLE,
                        _ => "configured peer listener unavailable",
                    })?]
                }
                CandidatePolicy::Automatic(_) => {
                    if pending.sockets.is_empty() {
                        return Err(UNREACHABLE.into());
                    }
                    pending
                        .sockets
                        .into_iter()
                        .map(|socket| {
                            socket
                                .listen(16)
                                .map_err(|_| "automatic peer listener unavailable".into())
                        })
                        .collect::<Result<Vec<_>>>()?
                }
            };
            (
                listeners,
                Side::Target(TlsAcceptor::from(Arc::new(config)), policy.target),
                Value::Null,
            )
        }
        Role::Listen => {
            let config = ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_client_auth_cert(certs, key)
                .map_err(|e| e.to_string())?;
            let connector = TlsConnector::from(Arc::new(config));
            // No application socket is opened, read, or replayed during selection.
            let candidate = match &policy.candidates {
                CandidatePolicy::Fixed { address, .. } => {
                    timeout(ATTEMPT, connect(&connector, *address, PROBE))
                        .await
                        .map_err(|_| UNREACHABLE)??;
                    *address
                }
                CandidatePolicy::Automatic(_) => {
                    probe_candidates(&connector, peer.candidates.unwrap_or_default()).await?
                }
            };
            let listener = TcpListener::bind(policy.listen)
                .await
                .map_err(|_| "application listener unavailable")?;
            let address = listener.local_addr().map_err(|e| e.to_string())?;
            let description = json!({"address":address.to_string(),"host":address.ip().to_string(),"port":address.port()});
            (
                vec![listener],
                Side::Listen(connector, candidate),
                description,
            )
        }
    };
    Ok((description, tokio::spawn(serve(listeners, side, lease))))
}

async fn probe_candidates(
    connector: &TlsConnector,
    addresses: Vec<SocketAddr>,
) -> Result<SocketAddr> {
    let mut attempts = stream::iter(addresses)
        .map(|address| async move {
            // Two slots, up to four candidates, and a single two-second deadline.
            let result = timeout(ATTEMPT / 2, connect(connector, address, PROBE))
                .await
                .map_err(|_| UNREACHABLE.to_string())
                .and_then(|result| result);
            (address, result)
        })
        .buffer_unordered(MAX_STREAMS);
    let mut selected = None;
    let result = timeout(ATTEMPT, async {
        while let Some((address, result)) = attempts.next().await {
            match result {
                Ok(_) => {
                    selected.get_or_insert(address);
                }
                Err(error) if error == UNREACHABLE => {}
                // An observed authentication failure never becomes relay fallback.
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })
    .await;
    if let Ok(Err(error)) = result {
        return Err(error);
    }
    // Dropping attempts cancels every remaining probe before opening the app.
    selected.ok_or_else(|| UNREACHABLE.into())
}

async fn connect(
    connector: &TlsConnector,
    candidate: SocketAddr,
    purpose: u8,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let socket = TcpStream::connect(candidate)
        .await
        .map_err(|_| UNREACHABLE)?;
    let name = ServerName::try_from(DNS).expect("static peer TLS name");
    let mut tls = connector
        .connect(name, socket)
        .await
        .map_err(|_| "peer TLS authentication failed")?;
    tls.write_u8(purpose)
        .await
        .map_err(|_| "peer did not accept the session")?;
    if tls.read_u8().await.ok() != Some(READY) {
        return Err("peer did not confirm the session".into());
    }
    Ok(tls)
}

async fn serve(listeners: Vec<TcpListener>, side: Side, mut lease: watch::Receiver<Instant>) {
    let mut streams = JoinSet::new();
    loop {
        let expires = *lease.borrow_and_update();
        if expires <= Instant::now() {
            break;
        }
        tokio::select! {
            biased;
            changed = lease.changed() => if changed.is_err() { break; },
            _ = tokio::time::sleep_until(expires) => break,
            Some(_) = streams.join_next(), if !streams.is_empty() => {},
            (result, _, _) = select_all(listeners.iter().map(|listener| Box::pin(listener.accept()))) => {
                let Ok((socket,_)) = result else { break; };
                if streams.len() < MAX_STREAMS {
                    streams.spawn(handle(side.clone(),socket));
                }
            }
        }
    }
    drop(listeners);
    streams.shutdown().await;
}

async fn handle(side: Side, mut socket: TcpStream) -> Result<()> {
    match side {
        Side::Listen(connector, candidate) => {
            let mut tls = timeout(ATTEMPT, connect(&connector, candidate, STREAM))
                .await
                .map_err(|_| UNREACHABLE)??;
            copy_bidirectional(&mut socket, &mut tls)
                .await
                .map_err(|e| e.to_string())?;
        }
        Side::Target(acceptor, target) => {
            let (mut tls, app) = timeout(ATTEMPT, async {
                let mut tls = acceptor.accept(socket).await?;
                let app = match tls.read_u8().await? {
                    PROBE => None,
                    STREAM => Some(TcpStream::connect(target).await?),
                    _ => return Err(io::Error::other("unknown peer stream request")),
                };
                tls.write_u8(READY).await?;
                Ok::<_, io::Error>((tls, app))
            })
            .await
            .map_err(|_| "peer handshake expired")?
            .map_err(|e| e.to_string())?;
            if let Some(mut app) = app {
                copy_bidirectional(&mut tls, &mut app)
                    .await
                    .map_err(|e| e.to_string())?;
            } else {
                tls.shutdown().await.map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

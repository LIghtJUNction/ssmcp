//! TLS 1.3 mutual authentication with a fresh local identity per offer. Only
//! the authenticated partner's ephemeral certificate is trusted for this mapping.
use super::{Pending, Policy, Result, Role, UNREACHABLE};
use crate::wire::peer_port::Offer;
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    pending: &Pending,
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
    let (listener, side, description) = match policy.role {
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
            let listener = TcpListener::bind(policy.bind)
                .await
                .map_err(|e| match e.kind() {
                    io::ErrorKind::AddrInUse | io::ErrorKind::AddrNotAvailable => UNREACHABLE,
                    _ => "configured peer listener unavailable",
                })?;
            (
                listener,
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
            timeout(ATTEMPT, connect(&connector, policy.candidate, PROBE))
                .await
                .map_err(|_| UNREACHABLE)??;
            let listener = TcpListener::bind(policy.listen)
                .await
                .map_err(|_| "application listener unavailable")?;
            let address = listener.local_addr().map_err(|e| e.to_string())?;
            let description = json!({"address":address.to_string(),"host":address.ip().to_string(),"port":address.port()});
            (
                listener,
                Side::Listen(connector, policy.candidate),
                description,
            )
        }
    };
    Ok((description, tokio::spawn(serve(listener, side, lease))))
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

async fn serve(listener: TcpListener, side: Side, mut lease: watch::Receiver<Instant>) {
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
            result = listener.accept() => {
                let Ok((socket,_)) = result else { break; };
                if streams.len() < MAX_STREAMS {
                    streams.spawn(handle(side.clone(),socket));
                }
            }
        }
    }
    drop(listener);
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

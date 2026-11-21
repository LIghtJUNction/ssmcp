//! Both live ACLs are checked before setup and on each renewal. An uncertain
//! setup outcome always fails closed. Only a confirmed unreachable path falls back.
use super::*;
use crate::wire::peer_port::{Command, Mapping, Offer, Role};
use std::net::{Ipv4Addr, SocketAddr};

pub(super) const NAMESPACE: &str = "ports.peer_control";
pub(super) enum Outcome {
    Closed,
    Fallback(&'static str),
}

fn address(end: &PortEnd) -> Result<SocketAddr, String> {
    let ip = if end.host == "localhost" {
        Ipv4Addr::LOCALHOST.into()
    } else {
        end.host.parse().map_err(|_| "invalid loopback address")?
    };
    Ok(SocketAddr::new(ip, end.port))
}

async fn control(access: &Access, session: &Session, command: Command) -> Result<Value, String> {
    let (_, auth) = access.authorized().await?;
    access
        .native(
            session,
            &auth,
            Operation::PeerPort {
                command: Box::new(command),
            },
        )
        .await
}

async fn prepare(
    access: &Access,
    task: &Task,
    session: Arc<Session>,
    mapping: &Mapping,
    role: Role,
) -> Result<(Remote, Offer), String> {
    let value = control(
        access,
        &session,
        Command::Prepare {
            mapping: mapping.clone(),
            role,
        },
    )
    .await?;
    let offer: Offer = serde_json::from_value(value).map_err(|_| "invalid peer offer")?;
    if offer.id.len() != 32 || offer.certificate.len() > 2048 || offer.proof.len() != 44 {
        return Err("invalid peer offer".into());
    }
    let remote = Remote {
        client_id: session.client_id().into(),
        id: offer.id.clone(),
        namespace: NAMESPACE,
        session,
        metadata: Value::Null,
    };
    task.track(&remote);
    Ok((remote, offer))
}

pub(super) async fn run(
    access: &Access,
    task: &Task,
    cancel: &mut watch::Receiver<bool>,
    options: &PortOptions,
    deadline: Instant,
) -> Result<Outcome, String> {
    let left = access.session(&options.listen.client_id).await?;
    let right = access.session(&options.target.client_id).await?;
    if !left.peer_ports || !right.peer_ports || left.client_id() == right.client_id() {
        return Ok(Outcome::Fallback(
            "two distinct opted-in clients are required",
        ));
    }
    let mapping = Mapping {
        id: task.id.clone(),
        listen_client: left.client_id().into(),
        target_client: right.client_id().into(),
        listen: address(&options.listen)?,
        target: address(&options.target)?,
        offer_until_ms: crate::peer_port::now_ms() + 5000,
        ttl_ms: options.ttl_ms,
    };
    let (left, left_offer) = prepare(access, task, left, &mapping, Role::Listen).await?;
    let (right, right_offer) = prepare(access, task, right, &mapping, Role::Target).await?;
    access.session(&left.client_id).await?;
    access.session(&right.client_id).await?;
    let peer_available = match control(
        access,
        &right.session,
        Command::Activate {
            id: right.id.clone(),
            peer: left_offer,
            peer_available: true,
        },
    )
    .await
    {
        Ok(_) => true,
        Err(error) if error == crate::peer_port::UNREACHABLE => false,
        Err(error) => return Err(error),
    };
    if *cancel.borrow() || Instant::now() >= deadline {
        return Ok(Outcome::Closed);
    }
    access.session(&right.client_id).await?;
    let opened = control(
        access,
        &left.session,
        Command::Activate {
            id: left.id.clone(),
            peer: right_offer,
            peer_available,
        },
    )
    .await;
    let result = match opened {
        Ok(value) => value,
        Err(error) if error == crate::peer_port::UNREACHABLE => {
            // Require both close acknowledgements before any relay listener opens.
            tokio::try_join!(task.close_remote(&left), task.close_remote(&right))?;
            return Ok(Outcome::Fallback(crate::peer_port::UNREACHABLE));
        }
        Err(error) => return Err(error),
    };
    access.session(&left.client_id).await?;
    access.session(&right.client_id).await?;
    {
        let mut value = task.value.lock().unwrap();
        value["transport"] = json!("direct");
        value["listener"] = result
            .get("listener")
            .filter(|v| v["port"].as_u64().is_some())
            .ok_or("peer omitted application listener")?
            .clone();
    }
    task.update("running", None, None);
    loop {
        if *cancel.borrow() || Instant::now() >= deadline {
            return Ok(Outcome::Closed);
        }
        tokio::select! {
            _ = cancel.changed() => return Ok(Outcome::Closed),
            _ = tokio::time::sleep_until(deadline) => return Ok(Outcome::Closed),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        access.session(&left.client_id).await?;
        access.session(&right.client_id).await?;
        tokio::try_join!(
            control(
                access,
                &left.session,
                Command::Renew {
                    id: left.id.clone()
                }
            ),
            control(
                access,
                &right.session,
                Command::Renew {
                    id: right.id.clone()
                }
            )
        )?;
    }
}

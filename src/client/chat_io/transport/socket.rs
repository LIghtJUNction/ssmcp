use super::{READ_LIMIT, Sender};
use crate::{local_socket, wire::MAX_USER_MESSAGE_BYTES};
use serde_json::json;
use std::{
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    task::JoinSet,
};

pub(super) async fn bind(path: Option<&Path>) -> Result<local_socket::Bound, String> {
    let default;
    let path = match path {
        Some(path) => path,
        None => {
            // SAFETY: geteuid only reads the current process credentials.
            let uid = unsafe { libc::geteuid() };
            let parent = PathBuf::from(format!("/tmp/ssmcp-{uid}"));
            match crate::platform::private_directory(&parent) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.to_string()),
            }
            default = parent.join("chat-input.sock");
            &default
        }
    };
    let bound = local_socket::Bound::bind(path)
        .await
        .map_err(|error| format!("cannot bind chat input socket: {error}"))?;
    crate::client::render::status("chat input", "Listening", &path.display().to_string());
    Ok(bound)
}

async fn request(stream: UnixStream, sender: Sender) -> Result<(), io::Error> {
    local_socket::peer(&stream)?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut frame = Vec::new();
    let count = tokio::time::timeout(
        READ_LIMIT,
        (&mut reader)
            .take((MAX_USER_MESSAGE_BYTES + 2) as u64)
            .read_until(b'\n', &mut frame),
    )
    .await
    .map_err(io::Error::other)??;
    if count == 0 {
        return Ok(());
    }
    let outcome = if frame.last() != Some(&b'\n') {
        Err("expected one bounded UTF-8 line terminated by a newline".into())
    } else {
        frame.pop();
        if frame.last() == Some(&b'\r') {
            frame.pop();
        }
        match std::str::from_utf8(&frame) {
            Ok(text) => {
                let invalid = async {
                    let mut trailing = [0; 1];
                    // A writer may half-close after its complete request and
                    // continue reading the receipt. A second frame is invalid.
                    if matches!(reader.read(&mut trailing).await, Ok(0)) {
                        // Linux reports POLLHUP for a fully closed Unix peer,
                        // while shutdown(Write) only closes the incoming half.
                        let mut peer = libc::pollfd {
                            fd: reader.get_ref().as_ref().as_raw_fd(),
                            events: 0,
                            revents: 0,
                        };
                        loop {
                            // SAFETY: the read half owns this fd for the whole
                            // check. poll reads and writes one stack entry.
                            if unsafe { libc::poll(&mut peer, 1, 0) } > 0
                                && peer.revents & (libc::POLLHUP | libc::POLLERR) != 0
                            {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = invalid => return Ok(()),
                    result = sender.dispatch(text, None) => result,
                }
            }
            Err(_) => Err("chat input must be UTF-8".into()),
        }
    };
    let value = match outcome {
        Ok(receipt) => json!(receipt),
        Err(error) => json!({"error":error}),
    };
    let mut bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.shutdown().await
}

pub(super) async fn serve(bound: local_socket::Bound, sender: Sender) -> Result<(), String> {
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            result = bound.listener.accept(), if requests.len() < 8 => {
                let (stream, _) = result.map_err(|error| error.to_string())?;
                let sender = sender.clone();
                requests.spawn(async move { let _ = tokio::time::timeout(Duration::from_secs(150), request(stream, sender)).await; });
            }
            _ = requests.join_next(), if !requests.is_empty() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn newline_then_write_eof_still_delivers_one_receipt() {
        let (mut peer, server) = UnixStream::pair().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(request(
            server,
            Sender {
                queue: tx,
                save_exchange: false,
            },
        ));
        peer.write_all(b"question\n").await.unwrap();
        peer.shutdown().await.unwrap();
        let accepted = tokio::time::timeout(READ_LIMIT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.text, "question");
        accepted
            .reply
            .send(Ok(crate::client::chat_io::Receipt::new(
                "queued",
                Some("request_1".into()),
            )))
            .unwrap();
        let mut line = String::new();
        BufReader::new(peer).read_line(&mut line).await.unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(receipt, json!({"status":"queued","request_id":"request_1"}));
        task.await.unwrap().unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn second_socket_frame_cancels_inflight_confirmation() {
        let (mut peer, server) = UnixStream::pair().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(request(
            server,
            Sender {
                queue: tx,
                save_exchange: false,
            },
        ));
        peer.write_all(b"question\n").await.unwrap();
        let mut accepted = tokio::time::timeout(READ_LIMIT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        peer.write_all(b"second frame\n").await.unwrap();
        tokio::time::timeout(READ_LIMIT, accepted.reply.closed())
            .await
            .unwrap();
        assert_eq!(peer.read(&mut [0; 1]).await.unwrap(), 0);
        task.await.unwrap().unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn closing_both_halves_cancels_pending_socket_dispatch() {
        let (mut peer, server) = UnixStream::pair().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(request(
            server,
            Sender {
                queue: tx,
                save_exchange: false,
            },
        ));
        peer.write_all(b"question\n").await.unwrap();
        peer.shutdown().await.unwrap();
        let mut accepted = tokio::time::timeout(READ_LIMIT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        drop(peer);
        tokio::time::timeout(READ_LIMIT, accepted.reply.closed())
            .await
            .unwrap();
        task.await.unwrap().unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn launch_does_not_replace_a_live_socket() {
        use std::os::unix::fs::PermissionsExt;

        let mut random = [0; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let parent = PathBuf::from(format!(
            "/tmp/ssmcp-chat-test-{}",
            crate::hex::encode(random)
        ));
        crate::platform::private_directory(&parent).unwrap();
        let path = parent.join("input.sock");
        let unrelated = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(bind(Some(&path)).await.is_err());
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(unrelated);
        std::fs::remove_file(&path).unwrap();

        let first = bind(Some(&path)).await.unwrap();
        assert!(bind(Some(&path)).await.is_err());
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(first);
        assert!(!path.exists());
        std::fs::remove_file(parent.join("input.sock.lock")).unwrap();
        std::fs::remove_dir(parent).unwrap();
    }
}

use super::{READ_LIMIT, Sender};
use crate::{local_socket, wire::MAX_USER_MESSAGE_BYTES};
use serde_json::json;
use std::{
    io,
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
                let mut trailing = [0; 1];
                tokio::select! {
                    biased;
                    _ = reader.read(&mut trailing) => return Ok(()),
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
    async fn launch_does_not_replace_a_live_socket() {
        use std::os::unix::fs::PermissionsExt;

        let mut random = [0; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let parent = PathBuf::from(format!(
            "/tmp/ssmcp-chat-test-{}",
            crate::hex::encode(&random)
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

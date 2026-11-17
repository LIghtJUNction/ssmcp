//! Single public-key lines validated by the installed OpenSSH implementation.
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub(crate) const MAX_PUBLIC_KEY: usize = 16 * 1024;

pub(crate) fn parse(raw: &str) -> Result<(String, String), String> {
    if raw.len() > MAX_PUBLIC_KEY {
        return Err("Public key exceeds 16 KiB.".into());
    }
    let raw = raw.trim_ascii();
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return Err("Expected one SSH public key line, without authorized_keys options or private key data.".into());
    }
    let mut fields = raw.split_ascii_whitespace();
    let algorithm = fields.next().ok_or("Missing public key type")?;
    let encoded = fields.next().ok_or("Missing public key data")?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid public key base64")?;
    let size = bytes.get(..4).ok_or("Invalid SSH key blob")?;
    let size = u32::from_be_bytes(size.try_into().map_err(|_| "Invalid SSH key blob")?) as usize;
    let end = 4usize.checked_add(size).ok_or("Invalid SSH key type")?;
    if bytes.get(4..end) != Some(algorithm.as_bytes())
        || bytes
            .get(end..)
            .is_none_or(|payload| payload.iter().all(|byte| *byte == 0))
        || STANDARD.encode(&bytes) != encoded
    {
        return Err("Public key type and blob do not match.".into());
    }
    let canonical = format!("{algorithm} {encoded}");
    let program = crate::program::executable("ssh-keygen")
        .map_err(|_| "OpenSSH ssh-keygen is required to validate public keys")?;
    let mut child = Command::new(program)
        .args(["-l", "-f", "-", "-E", "sha256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Could not start ssh-keygen")?;
    let input = child.stdin.take().ok_or("Missing ssh-keygen input")?;
    let written = {
        let mut input = input;
        writeln!(input, "{canonical}")
    };
    if written.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not send public key to ssh-keygen".into());
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err("OpenSSH rejected this public key.".into()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("OpenSSH public key validation failed or timed out.".into());
            }
        }
    }
    Ok((
        canonical,
        format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_validates_security_keys_without_a_hardware_token() {
        let algorithm = "sk-ssh-ed25519@openssh.com";
        let mut blob = Vec::new();
        for field in [algorithm.as_bytes(), &[1u8; 32], b"ssh:test"] {
            blob.extend_from_slice(&(field.len() as u32).to_be_bytes());
            blob.extend_from_slice(field);
        }
        let raw = format!("{algorithm} {}", STANDARD.encode(&blob));
        assert!(parse(&raw).is_ok());
        assert!(parse(&format!("command=\"id\" {raw}")).is_err());
        blob.push(1);
        assert!(parse(&format!("{algorithm} {}", STANDARD.encode(blob))).is_err());
    }
}

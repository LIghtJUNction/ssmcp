//! Chat input, with stdout reserved for a receipt or requested actual replies.
use super::*;
use directory::{Directory, Kind, Selected};

const HELP: &str = "Usage: ssmcp send [process|thread] ID|NUMBER [TEXT | --stdin] [--server URL]\n                  [--request-id ID] [--wait] [--json]\n\nA process sends the same input to its fixed main and registered thread recipients.\nA thread sends only to that exact ID. Numbers come from the last ssmcp list and expire in 5 minutes.\nWithout TEXT, piped stdin is read as UTF-8 (at most 16 KiB). A receipt JSON is written to stdout.\n--wait writes actual final replies instead; delivery acknowledgement is not completion.\nReuse --request-id only with the exact same text and complete recipient set. No task is cancelled by detaching.\nExample: printf 'Review this change' | ssmcp send thread 3 --stdin | ssmcp wait --all";

pub(super) fn request_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    platform::random_bytes(&mut bytes)?;
    Ok(format!("io_{}", crate::hex::encode(bytes)))
}
pub(super) fn valid_text(text: &str) -> Result<()> {
    if text.trim().is_empty()
        || text.len() > 16 * 1024
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("chat input must be nonempty UTF-8, at most 16 KiB, with no control characters except newline or tab".into());
    }
    Ok(())
}
pub(super) async fn submit(
    api: &Api,
    credential: &Credential,
    selection: &Selected,
    text: &str,
    id: &str,
) -> Result<Value> {
    valid_text(text)?;
    crate::wire::messages::validate_request_id(id)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    if selection.recipients.is_empty() {
        return Err("this server did not advertise chat thread recipients; update the relay/client or use explicit --terminal for a PTY".into());
    }
    if selection.recipients.len() > 32 {
        return Err(
            "process fanout exceeds 32 recipients; select individual threads explicitly".into(),
        );
    }
    let mut receipt=api.post("/api/client/io",Some(&credential.token),json!({"action":"send","request_id":id,"text":text,
        "recipients":selection.recipients.iter().map(|target|target.recipient()).collect::<Vec<_>>()})).await?;
    if receipt["schema_version"] != 1
        || receipt["request_id"].as_str() != Some(id)
        || !receipt["recipients"].is_array()
    {
        return Err(
            "server returned an invalid chat receipt; inspect status before retrying".into(),
        );
    }
    receipt["origin"] = json!(credential.server);
    if let Some(subject) = &credential.subject
        && receipt["account"]["subject"].as_str() != Some(subject)
    {
        return Err("chat receipt account differs from the saved login".into());
    }
    receipt["account"]["principal"] = json!(credential.principal);
    Ok(receipt)
}
pub(super) async fn run(args: &[OsString]) -> Result<()> {
    let mut args = args.iter();
    let mut kind = None;
    let mut target = None;
    let mut text = None;
    let mut server = None;
    let mut stdin = false;
    let mut wait = false;
    let mut json = false;
    let mut id = None;
    let mut literal = false;
    while let Some(arg) = args.next() {
        let arg = arg.to_str().ok_or("send arguments must be UTF-8")?;
        let mut value = || -> Result<String> {
            Ok(args
                .next()
                .ok_or("send option requires a value")?
                .to_str()
                .ok_or("send arguments must be UTF-8")?
                .into())
        };
        match arg {
            "--help" | "-h" if !literal => {
                println!("{HELP}");
                return Ok(());
            }
            "--" if !literal => literal = true,
            "--server" if !literal => {
                if server.is_some() {
                    return Err("--server may be supplied once".into());
                }
                server = Some(normalize_server(&value()?)?);
            }
            "--request-id" if !literal => {
                if id.is_some() {
                    return Err("--request-id may be supplied once".into());
                }
                id = Some(value()?);
            }
            "--stdin" if !literal => stdin = true,
            "--wait" if !literal => wait = true,
            "--json" if !literal => json = true,
            _ if !literal && target.is_none() && kind.is_none() && Kind::parse(arg).is_some() => {
                kind = Kind::parse(arg)
            }
            _ if !literal && arg.starts_with('-') => {
                return Err("unknown send option; see ssmcp send --help".into());
            }
            _ if target.is_none() => target = Some(arg.to_owned()),
            _ if text.is_none() => text = Some(arg.to_owned()),
            _ => {
                return Err(
                    "send accepts one target and one quoted text argument (or --stdin)".into(),
                );
            }
        }
    }
    let target = target.ok_or("send requires a process/thread ID or listed number")?;
    if stdin && text.is_some() {
        return Err("choose text or --stdin, not both".into());
    }
    let text = if let Some(text) = text {
        text
    } else {
        if !stdin && io::stdin().is_terminal() {
            return Err("send needs TEXT or piped --stdin".into());
        }
        tokio::task::spawn_blocking(|| -> Result<String> {
            let mut bytes = Vec::new();
            io::stdin().take(16 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 16 * 1024 {
                return Err("chat input exceeds 16 KiB".into());
            }
            Ok(String::from_utf8(bytes).map_err(|_| "chat input must be UTF-8")?)
        })
        .await??
    };
    valid_text(&text)?;
    let credential = require_credential(server.as_deref()).await?;
    let api = Api::new(&credential.server)?;
    let live = api.get("/api/client/sessions", &credential.token).await?;
    let directory = Directory::parse(&live)?;
    let selection = directory.select(&credential, &target, kind)?;
    let id = id.map_or_else(request_id, Ok)?;
    eprintln!(
        "Submitting {id} to {} fixed recipient(s).",
        selection.recipients.len()
    );
    let receipt = submit(&api, &credential, &selection, &text, &id).await?;
    if wait {
        eprintln!("Queued {id}; waiting for actual replies.");
        if json {
            wait::for_receipt_with_json(&api, &credential, receipt, true).await
        } else {
            wait::for_receipt(&api, &credential, receipt).await
        }
    } else {
        println!("{}", serde_json::to_string(&receipt)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pipe_text_preserves_unicode_newlines_and_rejects_terminal_controls() {
        assert!(valid_text("中文 request\nsecond line\tvalue").is_ok());
        for text in ["", " \n", "\u{1b}[2J", "bad\0value"] {
            assert!(valid_text(text).is_err());
        }
        assert!(valid_text(&"界".repeat(6000)).is_err());
    }
}

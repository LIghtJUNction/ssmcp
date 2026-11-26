//! A local /handoff authorizes one clipboard write on this client.
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

pub(crate) const TOOL: &str = "ssmcp.ui.handoff";
const MAX_TEXT_BYTES: usize = 8 * 1024;
const REQUEST_LIFETIME: Duration = Duration::from_secs(30 * 60);
static PENDING: Mutex<Option<Request>> = Mutex::new(None);

struct Request {
    token: String,
    expires: Instant,
    writing: bool,
}

pub(crate) fn request(note: &str) -> Result<String, String> {
    super::validate_note(note)?;
    let mut nonce = [0; 16];
    crate::platform::random_bytes(&mut nonce).map_err(|_| "Could not create a handoff request")?;
    let token = crate::hex::encode(nonce);
    let mut prompt = format!(
        "I ran /handoff. Write a standalone prompt I can paste into a new agent. Include the goal, \
         relevant constraints, verified progress, unfinished work, and what to do next. Include \
         paths or receipt IDs where useful. Keep it in my language and omit secrets, credentials, \
         and this handoff token. Keep the prompt within {MAX_TEXT_BYTES} UTF-8 bytes. Discover and call \
         {TOOL} on this SSMCP client with token {token} and text set to the finished prompt. \
         On a copied receipt, send a short SSMCP reply telling me I can paste it into the next \
         agent. If copying cannot be confirmed, show the handoff text in an SSMCP reply for \
         manual copying."
    );
    if !note.is_empty() {
        prompt.push_str("\n\nMy handoff note:\n");
        prompt.push_str(note);
    }
    if prompt.len() > crate::wire::MAX_USER_MESSAGE_BYTES {
        return Err("The handoff note is too long for a queued request".into());
    }
    let mut pending = PENDING.lock().unwrap();
    if pending.as_ref().is_some_and(|request| request.writing) {
        return Err("A handoff clipboard write is in progress".into());
    }
    *pending = Some(Request {
        token,
        expires: Instant::now() + REQUEST_LIFETIME,
        writing: false,
    });
    Ok(prompt)
}

pub(crate) fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Copy the agent-generated handoff prompt to the user's local clipboard after /handoff. Requires the one-use token from that request. Accepts plain text or Markdown up to 8192 UTF-8 bytes.",
        "inputSchema": {
            "type": "object", "additionalProperties": false, "required": ["token", "text"],
            "properties": {
                "token": {"type": "string", "minLength": 32, "maxLength": 32},
                "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT_BYTES}
            }
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false}
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    token: String,
    text: String,
}

// The spawned task owns the claim so a disconnected caller cannot accidentally
// release it while the clipboard provider is still writing.
struct WriteClaim(String);

impl Drop for WriteClaim {
    fn drop(&mut self) {
        if let Some(request) = PENDING.lock().unwrap().as_mut()
            && request.token == self.0
        {
            request.writing = false;
        }
    }
}

pub(crate) async fn call(arguments: Value) -> Result<Value, String> {
    let Arguments { token, text } =
        serde_json::from_value(arguments).map_err(|_| "handoff requires token and text")?;
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES || text.contains(&token) {
        return Err("Provide 1–8192 UTF-8 bytes of handoff text without the token".into());
    }
    super::validate_note(&text)?;
    let claim = {
        let mut pending = PENDING.lock().unwrap();
        let request = pending
            .as_mut()
            .filter(|request| request.token == token && Instant::now() < request.expires)
            .ok_or("No matching active /handoff request on this client")?;
        if request.writing {
            return Err("This handoff clipboard write is already in progress".into());
        }
        request.writing = true;
        WriteClaim(token)
    };
    tokio::spawn(async move {
        let _claim = claim;
        if let Err(error) = crate::client::clipboard::write_text(&text).await {
            crate::client::terminal::notice(
                "Could not copy the handoff. Ask the agent for its text.",
            );
            return Err(error);
        }
        *PENDING.lock().unwrap() = None;
        crate::client::terminal::notice("Handoff copied. Paste it into the next agent.");
        Ok(json!({"copied": true, "bytes": text.len()}))
    })
    .await
    .map_err(|_| "Handoff clipboard worker stopped")?
}

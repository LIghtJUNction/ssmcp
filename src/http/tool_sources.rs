//! Tool-source preparation is metadata, not eligible user work or queue delivery.
use super::*;
use crate::tool_repository::{Bundle, URI};

pub(super) struct Loaded {
    pub bundle: Bundle,
    client: Option<Arc<Session>>,
    binding: Option<code_mode::RunBinding>,
    mode: &'static str,
    hint: Option<&'static str>,
}

fn select_client(explicit: Option<&str>, bound: Option<&str>, available: &[&str]) -> Result<Option<String>, String> {
    let selected = explicit.or(bound).or_else(|| (available.len() == 1).then(|| available[0]));
    let Some(selected) = selected else { return Ok(None) };
    if selected.is_empty() || selected.len() > 64 || selected.chars().any(char::is_control)
        || !available.contains(&selected)
    {
        return Err("tool_client_id must identify a client authorized for this request".into());
    }
    Ok(Some(selected.to_owned()))
}

impl Loaded {
    pub(super) async fn verify(&self, broker: &Broker, user: &str, token: &str) -> Result<(), String> {
        let Some(session) = &self.client else { return Ok(()) };
        let authorization = broker.authenticate(user, token).await.ok_or("authorization expired before tool-source delivery")?;
        let current = broker.client(user, &authorization, session.client_id()).await?;
        if !Arc::ptr_eq(&current, session) {
            return Err("tool-source client connection changed; start a new call".into());
        }
        if let Some(binding) = &self.binding {
            if crate::bridge::owner(user, &authorization)? != binding.owner {
                return Err("tool-source thread owner changed".into());
            }
            session.collaboration.bind(&binding.owner, &binding.agent_id, &binding.token)
                .map_err(|_| "tool-source thread binding expired; start a new authorized call")?;
        }
        Ok(())
    }

    pub(super) fn append(&self, mut response: Value) -> Value {
        let mut provenance = self.bundle.provenance();
        provenance["mode"] = json!(self.mode);
        provenance["client_id"] = self.client.as_ref().map(|s| json!(s.client_id())).unwrap_or(Value::Null);
        provenance["hint"] = json!(self.hint);
        if let Some(fields) = response.get_mut("structuredContent").and_then(Value::as_object_mut) {
            fields.insert("tool_sources".into(), provenance.clone());
        }
        if let Some(content) = response.get_mut("content").and_then(Value::as_array_mut) {
            content.push(json!({"type":"text","text":format!(
                "TOOL SOURCES — host-selected source identities for this run\n{provenance}"
            )}));
        }
        response
    }
}

pub(super) async fn load(
    broker: &Broker,
    user: &str,
    token: &str,
    run: &code_mode::AuditRun,
    arguments: &CodeArguments,
    sessions: &[Arc<Session>],
) -> Result<Loaded, String> {
    let available = sessions.iter().map(|session| session.client_id()).collect::<Vec<_>>();
    let selected = select_client(
        arguments.tool_client_id.as_deref(),
        arguments.agent.as_ref().map(|agent| agent.client_id.as_str()),
        &available,
    )?;
    let Some(client_id) = selected else {
        return Ok(Loaded {
            bundle:crate::tool_repository::official_seed().await?,
            client:None, binding:None, mode:"read_only_official_seed",
            hint:Some("No client tool sources were selected. Pass tool_client_id to load that authorized client's RC and local overrides."),
        });
    };
    let authorization = broker.authenticate(user, token).await.ok_or("authorization expired before tool-source read")?;
    let session = broker.client(user, &authorization, &client_id).await?;
    let binding = run.binding(&client_id);
    if let Some(binding) = &binding {
        if crate::bridge::owner(user, &authorization)? != binding.owner {
            return Err("tool-source thread owner changed".into());
        }
        session.collaboration.bind(&binding.owner, &binding.agent_id, &binding.token)?;
    }
    // Do not enter AUDIT_RUN here: preloading this fixed resource is not the
    // user's work and must not qualify a passive run to consume Next caller.
    // Native access and attribution still use the same authenticated account,
    // captured connection and exact verified thread, rechecked after the await.
    let response = session.call_with_bound_authorization(
        Operation::CallTool {
            name:"ssmcp.resources.read".into(),
            arguments:json!({"uri":URI}),
        }, arguments.controller_context.clone(), &authorization, binding.clone(),
    ).await?;
    let mut loaded = Loaded {
        bundle:Bundle { schema:String::new(), rc:None, sources:vec![] },
        client:Some(session), binding, mode:"authorized_client", hint:None,
    };
    loaded.verify(broker, user, token).await?;
    if response["isError"] == true {
        let detail = response["content"].as_array().and_then(|items| items.iter()
            .find_map(|item| item["text"].as_str())).unwrap_or("client resource failed");
        return Err(format!(
            "Selected client tool sources are unavailable: {}. Upgrade the client or repair its ssmcprc.toml; local overrides were not replaced by a fallback.",
            detail.chars().take(1024).collect::<String>()
        ));
    }
    let result = response.get("structuredContent").unwrap_or(&response);
    let contents = result["contents"].as_array().filter(|items| items.len() == 1)
        .ok_or("client does not implement the tool-source resource; upgrade the selected client")?;
    let content = &contents[0];
    if content["uri"] != URI || content["mimeType"] != "application/json" {
        return Err("selected client returned a different tool-source resource".into());
    }
    let text = content["text"].as_str().filter(|text| text.len() <= 512 * 1024)
        .ok_or("client tool-source bundle is missing or exceeds 512 KiB")?;
    loaded.bundle = serde_json::from_str::<Bundle>(text)
        .map_err(|e| format!("invalid client tool-source bundle: {e}"))?.validate()?;
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_selection_requires_an_authorized_exact_client_and_never_chooses_first_of_many() {
        assert_eq!(select_client(None, None, &[]).unwrap(), None);
        assert_eq!(select_client(None, None, &["a"]).unwrap(), Some("a".into()));
        assert_eq!(select_client(None, None, &["a","b"]).unwrap(), None);
        assert_eq!(select_client(None, Some("b"), &["a","b"]).unwrap(), Some("b".into()));
        assert_eq!(select_client(Some("a"), Some("b"), &["a","b"]).unwrap(), Some("a".into()));
        for value in ["", "foreign", "a\n"] {
            assert!(select_client(Some(value), Some("a"), &["a","b"]).is_err());
        }
    }
}

//! Generic client-resource forwarding. No native provider catalog or document cache.
use super::*;

fn relay_uri(client_id: &str, native_uri: &str) -> Result<String, ResourceError> {
    if native_uri.is_empty()
        || native_uri.len() > MAX_URI_BYTES
        || native_uri.chars().any(char::is_control)
    {
        return Err(ResourceError::Unavailable);
    }
    let query = serde_urlencoded::to_string([("uri", native_uri)])
        .map_err(|_| ResourceError::Unavailable)?;
    let uri = format!("ssmcp://clients/{client_id}/resource?{query}");
    if uri.len() > MAX_URI_BYTES {
        return Err(ResourceError::TooLarge);
    }
    Ok(uri)
}

fn forwarded_page(
    mut page: Value,
    id: &str,
    kind: &str,
    base: &str,
) -> Result<Value, ResourceError> {
    let field = if kind == "resources" {
        "resources"
    } else {
        "resourceTemplates"
    };
    let entries = page[field]
        .as_array_mut()
        .ok_or(ResourceError::Unavailable)?;
    if entries.len() > PAGE_SIZE {
        return Err(ResourceError::TooLarge);
    }
    for entry in entries {
        if kind == "resources" {
            let native_uri = entry["uri"]
                .as_str()
                .ok_or(ResourceError::Unavailable)?
                .to_owned();
            entry["uri"] = json!(relay_uri(id, &native_uri)?);
            entry["nativeUri"] = json!(native_uri);
        } else {
            let template = entry["uriTemplate"]
                .as_str()
                .ok_or(ResourceError::Unavailable)?;
            if template.is_empty()
                || template.len() > MAX_URI_BYTES
                || template.chars().any(char::is_control)
            {
                return Err(ResourceError::Unavailable);
            }
            // Native template syntax remains intact. Resolve it on the client, then
            // pass that URI to the generic relay resource template.
            entry["relayReadUriTemplate"] = json!(format!("ssmcp://clients/{id}/resource{{?uri}}"));
        }
    }
    if let Some(next) = page["nextCursor"].as_str() {
        if next.is_empty() || next.len() > 128 || next.chars().any(char::is_control) {
            return Err(ResourceError::Unavailable);
        }
        page["nextUri"] = json!(with_cursor(base, next));
    } else if !page["nextCursor"].is_null() {
        return Err(ResourceError::Unavailable);
    }
    Ok(page)
}

pub(super) async fn read(
    broker: &Broker,
    user: &str,
    authorization: &Authorization,
    session: &Session,
    kind: &str,
    (uri, base): (&str, &str),
    query: &Query,
) -> Result<Value, ResourceError> {
    let (name, arguments) = match kind {
        "capabilities" => ("ssmcp.client.capabilities", json!({})),
        "resources" | "resource-templates" => {
            let mut args = json!({});
            if let Some(cursor) = query.get("cursor") {
                args["cursor"] = json!(cursor);
            }
            (
                if kind == "resources" {
                    "ssmcp.resources.list"
                } else {
                    "ssmcp.resources.templates"
                },
                args,
            )
        }
        "resource" => (
            "ssmcp.resources.read",
            json!({
                "uri": query.get("uri").ok_or(ResourceError::InvalidParams("client resource read requires uri"))?
            }),
        ),
        _ => return Err(ResourceError::NotFound),
    };
    let response = session
        .call_with_authorization(
            Operation::CallTool {
                name: name.into(),
                arguments,
            },
            None,
            authorization,
        )
        .await;
    // A response cannot preserve a grant that expired or disconnected during I/O.
    broker
        .client(user, authorization, session.client_id())
        .await
        .map_err(|_| ResourceError::NotFound)?;
    let response = response.map_err(|_| ResourceError::Unavailable)?;
    if response
        .get("isError")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(ResourceError::Unavailable);
    }
    let mut value = response["structuredContent"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .ok_or(ResourceError::Unavailable)?;
    if serde_json::to_vec(&value)
        .map_err(|_| ResourceError::Unavailable)?
        .len()
        > MAX_RESOURCE_BYTES
    {
        return Err(ResourceError::TooLarge);
    }
    if kind == "resource" {
        let contents = value["contents"]
            .as_array_mut()
            .ok_or(ResourceError::Unavailable)?;
        if contents.is_empty() || contents.len() > PAGE_SIZE {
            return Err(ResourceError::Unavailable);
        }
        for entry in contents {
            let native_uri = entry["uri"]
                .as_str()
                .ok_or(ResourceError::Unavailable)?
                .to_owned();
            if !(entry["text"].is_string() ^ entry["blob"].is_string()) {
                return Err(ResourceError::Unavailable);
            }
            entry["nativeUri"] = json!(native_uri);
            entry["uri"] = json!(uri);
        }
        if serde_json::to_vec(&value)
            .map_err(|_| ResourceError::Unavailable)?
            .len()
            > MAX_RESOURCE_BYTES
        {
            return Err(ResourceError::TooLarge);
        }
        return Ok(value);
    }
    if matches!(kind, "resources" | "resource-templates") {
        value = forwarded_page(value, session.client_id(), kind, base)?;
    }
    value["client"] = client_info(session);
    json_content(uri, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_client_resource_types_and_descriptors_pass_through() {
        let input = json!({"resources":[{"uri":"future://widget/1?x=a+b","name":"Future widget","mimeType":"application/x-future","annotations":{"audience":["assistant"]},"x-future":{"v":2}}],"nextCursor":"opaque:two","custom":42});
        let page = forwarded_page(input, "a", "resources", "ssmcp://clients/a/resources").unwrap();
        assert_eq!(page["resources"][0]["nativeUri"], "future://widget/1?x=a+b");
        assert_eq!(page["resources"][0]["x-future"]["v"], 2);
        assert_eq!(page["custom"], 42);
        assert_eq!(page["nextCursor"], "opaque:two");
        assert!(
            page["resources"][0]["uri"]
                .as_str()
                .unwrap()
                .contains("uri=future%3A%2F%2F")
        );
        assert!(
            page["nextUri"]
                .as_str()
                .unwrap()
                .contains("cursor=opaque%3Atwo")
        );
        let templates = forwarded_page(
            json!({"resourceTemplates":[{"uriTemplate":"future://{kind}{?id}","name":"Future"}]}),
            "a",
            "resource-templates",
            "x",
        )
        .unwrap();
        assert_eq!(
            templates["resourceTemplates"][0]["uriTemplate"],
            "future://{kind}{?id}"
        );
    }
    #[test]
    fn malformed_or_unbounded_pages_do_not_escape_the_relay() {
        for input in [
            json!({"resources":null}),
            json!({"resources":[{}]}),
            json!({"resources":[{"uri":""}]}),
            json!({"resources":[],"nextCursor":5}),
        ] {
            assert!(forwarded_page(input, "a", "resources", "x").is_err());
        }
        assert!(
            forwarded_page(
                json!({"resources":vec![json!({"uri":"future://a"});PAGE_SIZE+1]}),
                "a",
                "resources",
                "x"
            )
            .is_err()
        );
        assert!(relay_uri("a", &"x".repeat(MAX_URI_BYTES)).is_err());
    }
}

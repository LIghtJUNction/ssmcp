//! Select from actual native registrations; caller-owned browser plugins stay external.
use crate::registry::{Provider, Registry};
use serde_json::{Value, json};

pub(crate) fn control_routes(registry: &Registry) -> Result<Value, String> {
    let catalog = registry.provider_catalog_summary(Provider::Playwright)?;
    let native = catalog["tool_names"].as_array().is_some_and(|names| {
        names.iter().any(|name| {
            name.as_str()
                .is_some_and(|name| name.starts_with("playwright."))
        })
    });
    Ok(json!({
        "selection_order":["caller_browser_plugin_if_available","registered_playwright_extension","explicit_cdp"],
        "selected_browser":null,
        "preferred_client_provider":native.then_some("playwright_extension"),
        "chatgpt_browser":{
            "scope":"caller_application",
            "available_through_client":false,
            "availability_in_caller":"not_observed",
            "next_step":"Use the authorized ChatGPT Browser plugin when the calling application actually exposes it for the requested browser. Call its native API outside ssmcp code_mode; this client cannot invoke that public browser JavaScript API."
        },
        "playwright_extension":{
            "state":if native {"native_tools_registered"} else if catalog["tool_count"].as_u64().unwrap_or(0)>0 {"control_tools_only"} else {"not_registered"},
            "catalog":catalog,
            "browser_connection":"not_inferred_from_catalog",
            "approval":"native_extension",
            "next_step":{"helper":"tools.search","options":{"query":"playwright","limit":20,"include_schemas":true}},
            "call":"Use an exact registered tool name and its unchanged native inputSchema."
        },
        "cdp":{
            "automatic_fallback":false,
            "observe":{"tool":"ssmcp.browser.status","arguments":{}},
            "connect":"Use ssmcp.browser.connect only for an explicitly selected, observed loopback endpoint; preserve the requested browser family and session."
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_extension_route_requires_native_registered_tools() {
        let registry = Registry::new();
        let routes = control_routes(&registry).unwrap();
        assert!(routes["preferred_client_provider"].is_null());
        assert_eq!(routes["chatgpt_browser"]["available_through_client"], false);
        assert_eq!(routes["cdp"]["automatic_fallback"], false);

        let control = json!({"name":"ssmcp.playwright.events",
            "inputSchema":{"type":"object","properties":{}}});
        registry
            .replace(Provider::Playwright, vec![control])
            .unwrap();
        assert_eq!(
            control_routes(&registry).unwrap()["playwright_extension"]["state"],
            "control_tools_only"
        );

        let native = json!({
            "name":"playwright.browser_snapshot",
            "description":"native fixture",
            "inputSchema":{"type":"object","properties":{"opaque":{"type":["string","null"]}}},
            "annotations":{"readOnlyHint":true},
            "x-native":{"future":"unchanged"}
        });
        registry
            .replace(Provider::Playwright, vec![native.clone()])
            .unwrap();
        let routes = control_routes(&registry).unwrap();
        assert_eq!(routes["preferred_client_provider"], "playwright_extension");
        assert_eq!(
            routes["playwright_extension"]["catalog"]["tool_names"],
            json!(["playwright.browser_snapshot"])
        );
        assert_eq!(
            routes["playwright_extension"]["browser_connection"],
            "not_inferred_from_catalog"
        );
        assert_eq!(registry.catalog().unwrap(), vec![native]);

        registry.invalidate(Provider::Playwright);
        assert!(control_routes(&registry).unwrap()["preferred_client_provider"].is_null());
        registry.close();
        assert!(control_routes(&registry).is_err());
    }
}

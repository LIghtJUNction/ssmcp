//! Report the same engine choices used by desktop dispatch.
use super::{Session, capability, host_control};
use serde_json::{Value, json};

const OPERATIONS: [&str; 14] = [
    "monitors",
    "windows",
    "focus",
    "screenshot",
    "move",
    "move_path",
    "click",
    "drag",
    "key",
    "keyboard_state",
    "num_lock",
    "type",
    "scroll",
    "watch_until_click",
];

pub(super) fn platform_input(platform: &str, name: &str) -> bool {
    matches!(platform, "windows" | "macos")
        && !matches!(
            name,
            "ssmcp.computer.move_path"
                | "ssmcp.computer.num_lock"
                | "ssmcp.computer.watch_until_click"
        )
        && host_control::input_tool(name)
}

fn engine(platform: &str, session: Session, operation: &str, cursor: bool) -> &'static str {
    if operation == "monitors"
        || platform_input(platform, &format!("ssmcp.computer.{operation}"))
        || operation == "screenshot"
            && cursor
            && (matches!(platform, "windows" | "macos")
                || matches!(session, Session::Hyprland | Session::Wayland))
    {
        "ssmcp-native"
    } else {
        "zuse"
    }
}

pub(super) fn status(
    platform: &str,
    session: Session,
    library: Value,
    native: Option<Value>,
) -> Value {
    let hybrid = native.is_some();
    let mut status = if let Some(mut native) = native {
        // Default capture and window enumeration use zuse even when input retains
        // platform-specific identity checks and held-input release guarantees.
        let cursor = native["capabilities"]["screenshot"].clone();
        for operation in ["windows", "screenshot"] {
            native["capabilities"][operation] = library["capabilities"]
                .get(operation)
                .cloned()
                .unwrap_or_else(|| {
                    capability(
                        false,
                        "zuse",
                        library["reason"]
                            .as_str()
                            .or(Some("zuse capability probe failed")),
                    )
                });
        }
        native["capabilities"]["screenshot_with_cursor"] = cursor;
        native
    } else {
        library.clone()
    };
    status["capabilities"]["watch_until_click"] = json!({
        "available":true,"backend":"cdp","engine":"zuse","endpoint_probed":false,
        "requires":"explicit loopback endpoint, existing tab, exact origin, bounded scope and the local host-input switch"
    });
    let mut routes = serde_json::Map::new();
    for operation in OPERATIONS {
        let selected = engine(platform, session, operation, false);
        routes.insert(operation.into(), selected.into());
        if let Some(capability) = status
            .get_mut("capabilities")
            .and_then(|capabilities| capabilities.get_mut(operation))
            .and_then(Value::as_object_mut)
        {
            capability.insert("engine".into(), selected.into());
        }
    }
    let cursor = engine(platform, session, "screenshot", true);
    routes.insert("screenshot_with_cursor".into(), cursor.into());
    routes.insert(
        "status".into(),
        if hybrid { "mixed" } else { "zuse" }.into(),
    );
    if let Some(capability) = status
        .get_mut("capabilities")
        .and_then(|capabilities| capabilities.get_mut("screenshot_with_cursor"))
        .and_then(Value::as_object_mut)
    {
        capability.insert("engine".into(), cursor.into());
    }
    status["engine_routes"] = routes.into();
    status["zuse"] = json!({
        "version": zuse::VERSION,
        "backend": library["backend"],
        "available": library["available"],
        "reason": library["reason"],
    });
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_input_guards_and_cursor_routes_are_preserved() {
        for platform in ["windows", "macos"] {
            for operation in ["focus", "move", "click", "drag", "key", "type", "scroll"] {
                assert!(platform_input(
                    platform,
                    &format!("ssmcp.computer.{operation}")
                ));
                assert_eq!(
                    engine(platform, Session::Unavailable, operation, false),
                    "ssmcp-native"
                );
            }
            assert_eq!(
                engine(platform, Session::Unavailable, "windows", false),
                "zuse"
            );
            assert_eq!(
                engine(platform, Session::Unavailable, "screenshot", false),
                "zuse"
            );
            assert_eq!(
                engine(platform, Session::Unavailable, "screenshot", true),
                "ssmcp-native"
            );
        }
        assert!(!platform_input("linux", "ssmcp.computer.focus"));
        assert_eq!(engine("linux", Session::X11, "focus", false), "zuse");
        assert_eq!(engine("linux", Session::X11, "screenshot", true), "zuse");
        assert_eq!(
            engine("linux", Session::Wayland, "screenshot", true),
            "ssmcp-native"
        );
        assert_eq!(
            engine("linux", Session::X11, "monitors", false),
            "ssmcp-native"
        );
    }

    #[test]
    fn failed_zuse_probe_does_not_claim_native_fallback_for_zuse_operations() {
        let native = json!({
            "platform":"windows","available":true,"permissions":{"interactive_desktop":true},
            "capabilities":{
                "windows":capability(true,"EnumWindows",None),
                "screenshot":capability(true,"GDI cursor capture",None),
                "focus":capability(true,"PID and foreground verification",None)
            }
        });
        let status = status(
            "windows",
            Session::Unavailable,
            json!({"available":false,"reason":"worker not registered"}),
            Some(native),
        );
        assert_eq!(status["capabilities"]["windows"]["available"], false);
        assert_eq!(status["capabilities"]["screenshot"]["available"], false);
        assert_eq!(
            status["capabilities"]["windows"]["reason"],
            "worker not registered"
        );
        assert_eq!(status["capabilities"]["focus"]["available"], true);
        assert_eq!(status["capabilities"]["focus"]["engine"], "ssmcp-native");
        assert_eq!(
            status["capabilities"]["screenshot_with_cursor"]["available"],
            true
        );
        assert_eq!(status["permissions"]["interactive_desktop"], true);
        assert_eq!(status["engine_routes"]["windows"], "zuse");
        assert_eq!(status["engine_routes"]["status"], "mixed");
    }

    #[test]
    fn default_zuse_capture_is_independent_of_optional_native_cursor_helpers() {
        let native = json!({"available":true,"capabilities":{
            "screenshot":capability(false,"cursor helper",Some("helper unavailable")),
            "key":capability(false,"Quartz Events",Some("input permission denied"))
        }});
        let library = json!({"available":true,"backend":"quartz","capabilities":{
            "windows":capability(true,"quartz",None),
            "screenshot":capability(true,"quartz",None)
        }});
        let status = status("macos", Session::Unavailable, library, Some(native));
        assert_eq!(status["capabilities"]["screenshot"]["available"], true);
        assert_eq!(status["capabilities"]["screenshot"]["engine"], "zuse");
        assert_eq!(
            status["capabilities"]["screenshot_with_cursor"]["available"],
            false
        );
        assert_eq!(
            status["capabilities"]["key"]["reason"],
            "input permission denied"
        );
        assert_eq!(status["zuse"]["backend"], "quartz");
    }
}

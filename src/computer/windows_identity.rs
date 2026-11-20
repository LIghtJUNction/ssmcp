//! Bind a Windows window handle to the process observed during enumeration.
use serde_json::Value;

pub(super) fn encode(handle: usize, pid: u32) -> String {
    format!("hwnd:{handle:x}:{pid}")
}

#[cfg(any(windows, test))]
pub(super) fn parse(id: &str) -> Result<(usize, u32), String> {
    let parts: Vec<_> = id.split(':').collect();
    if parts.len() != 3
        || parts[0] != "hwnd"
        || parts[1].is_empty()
        || parts[1].len() > std::mem::size_of::<usize>() * 2
    {
        return Err("window_id must be copied from computer.windows".into());
    }
    if !parts[1].bytes().all(|b| b.is_ascii_hexdigit())
        || !parts[2].bytes().all(|b| b.is_ascii_digit())
    {
        return Err("invalid Windows window identity".into());
    }
    let handle = usize::from_str_radix(parts[1], 16).map_err(|_| "invalid Windows handle")?;
    let pid = parts[2]
        .parse::<u32>()
        .map_err(|_| "invalid window process identity")?;
    if handle == 0 || pid == 0 {
        return Err("window identity cannot be zero".into());
    }
    Ok((handle, pid))
}

pub(super) fn from_zuse(id: &Value, pid: &Value) -> Result<String, String> {
    let handle = id
        .as_str()
        .and_then(|id| id.strip_prefix("0x"))
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .and_then(|id| usize::from_str_radix(id, 16).ok())
        .filter(|handle| *handle != 0)
        .ok_or("zuse returned an invalid Windows handle")?;
    let pid = pid
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or("zuse returned an invalid Windows process identity")?;
    Ok(encode(handle, pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn zuse_window_identity_round_trips_through_the_native_focus_parser() {
        let token = from_zuse(&json!("0x1a2b"), &json!(42)).unwrap();
        assert_eq!(token, "hwnd:1a2b:42");
        assert_eq!(parse(&token).unwrap(), (0x1a2b, 42));
        assert_ne!(token, from_zuse(&json!("0x1a2b"), &json!(43)).unwrap());
        assert!(
            parse("0x1a2b").is_err(),
            "a bare recycled handle is insufficient"
        );
    }

    #[test]
    fn incomplete_native_identities_never_become_focus_tokens() {
        for (handle, pid) in [
            (json!("0x0"), json!(1)),
            (json!("0x1"), json!(0)),
            (json!("0x1"), Value::Null),
            (json!("0x1"), json!(u64::from(u32::MAX) + 1)),
            (json!("0x1"), json!("42")),
            (json!("hwnd:1:42"), json!(42)),
            (json!("0xnot-hex"), json!(42)),
        ] {
            assert!(from_zuse(&handle, &pid).is_err());
        }
    }
}

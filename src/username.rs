//! Canonical server-local account names shared by the CLI and portal.
const RESERVED: &[&str] = &[
    "admin",
    "administrator",
    "api",
    "auth",
    "bin",
    "daemon",
    "default",
    "github",
    "guest",
    "help",
    "http",
    "https",
    "login",
    "logout",
    "mcp",
    "nobody",
    "null",
    "oauth",
    "operator",
    "portal",
    "root",
    "server",
    "setup",
    "ssmcp",
    "ssmcpd",
    "staff",
    "support",
    "system",
    "undefined",
    "user",
    "users",
    "www",
];

/// Names are case-insensitive ASCII; reject whitespace instead of silently changing input.
pub(crate) fn normalize(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    if !(3..=32).contains(&bytes.len())
        || !bytes[0].is_ascii_alphabetic()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(b))
    {
        return Err("username must contain 3-32 ASCII letters, digits, '_' or '-', start with a letter and end with a letter or digit".into());
    }
    let name = raw.to_ascii_lowercase();
    if RESERVED.contains(&name.as_str()) || name.starts_with("gh-") {
        return Err("this username is reserved by the server; choose another name".into());
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_have_one_spelling() {
        assert_eq!(normalize("LIghtJUNction").unwrap(), "lightjunction");
        assert_eq!(normalize("alice_2-home").unwrap(), "alice_2-home");
        assert_eq!(
            normalize(&format!("a{}", "1".repeat(31))).unwrap().len(),
            32
        );
    }

    #[test]
    fn rejects_reserved_and_ambiguous_names() {
        for name in [
            "",
            "ab",
            "0abc",
            "alice-",
            "alice_",
            " alice",
            "alice ",
            "a.b",
            "a/b",
            "a@b",
            "管理员",
            "Kelvin",
            "Root",
            "ADMIN",
            "gh-123",
        ] {
            assert!(normalize(name).is_err(), "{name}");
        }
        assert!(normalize(&"a".repeat(33)).is_err());
    }
}

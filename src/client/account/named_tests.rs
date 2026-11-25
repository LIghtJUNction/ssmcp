use super::*;

#[test]
fn named_destinations_normalize_accounts_ports_and_ipv6() {
    for (target, username, origin) in [
        ("Alice@EXAMPLE.com:443", "alice", "https://example.com"),
        (
            "alice_dev@EXAMPLE.com:8443",
            "alice_dev",
            "https://example.com:8443",
        ),
        ("alice@[::1]:8443", "alice", "https://[::1]:8443"),
        (
            "alice@http://127.0.0.1:3000",
            "alice",
            "http://127.0.0.1:3000",
        ),
    ] {
        assert_eq!(
            parse_target(target).unwrap(),
            (username.into(), origin.into())
        );
    }
    for target in [
        "alice",
        "alice@",
        "@example.com",
        "root@example.com",
        "ab@example.com",
        "älïce@example.com",
        "alice@@example.com",
        "alice@host/path",
        "alice@host?query",
        "alice@host#fragment",
        "alice@host\\other",
        "alice@http://example.com",
        "alice@host:0",
        "alice@host:65536",
        "alice@ https://example.com",
        "alice@https://user:pass@example.com",
    ] {
        assert!(parse_target(target).is_err(), "{target}");
    }
}

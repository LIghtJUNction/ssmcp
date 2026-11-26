use super::*;

fn gateway() -> Pancake {
    let private =
        PrivateKeyDer::from_pem_slice(include_bytes!("fixtures/test-rsa-private.pem")).unwrap();
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&private).unwrap();
    let webhook_key =
        SubjectPublicKeyInfoDer::from_pem_slice(include_bytes!("fixtures/test-rsa-public.pem"))
            .unwrap();
    let config = CommerceConfig {
        store_id: "STO_test".into(),
        ..CommerceConfig::default()
    };
    Pancake {
        client: crate::http_client::builder().build().unwrap(),
        config,
        signing_key,
        webhook_key,
    }
}
#[test]
fn request_signature_matches_independent_openssl_fixture() {
    let gateway = gateway();
    let signature = gateway
        .signature(
            CREATE_SESSION,
            1_700_000_000,
            br#"{"productId":"PROD_test","currency":"CNY"}"#,
        )
        .unwrap();
    assert_eq!(signature, include_str!("fixtures/request-signature.txt"));
}
#[test]
fn raw_signed_webhook_verifies_and_tampering_does_not() {
    let gateway = gateway();
    let raw = include_bytes!("fixtures/webhook.json");
    let signature = include_str!("fixtures/webhook-signature.txt");
    let event = gateway.verify(raw, signature, 1_000_000).unwrap();
    assert_eq!(event.store_id, "STO_test");
    let mut changed = raw.to_vec();
    changed.push(b' ');
    assert!(matches!(
        gateway.verify(&changed, signature, 1_000_000),
        Err(Error::Signature)
    ));
    let mut changed = raw.to_vec();
    changed[0] = b'[';
    assert!(matches!(
        gateway.verify(&changed, signature, 1_000_000),
        Err(Error::Signature)
    ));
    assert!(gateway.verify(raw, "t=1000000,v1=AAAA", 1_000_000).is_err());
}
#[test]
fn environment_store_and_replay_window_are_strict() {
    let mut gateway = gateway();
    let raw = include_bytes!("fixtures/webhook.json");
    let signature = include_str!("fixtures/webhook-signature.txt");
    assert!(
        gateway
            .verify(raw, signature, 1_000_000 + 45 * 60 * 1000)
            .is_ok()
    );
    assert!(
        gateway
            .verify(raw, signature, 1_000_000 + 45 * 60 * 1000 + 1)
            .is_err()
    );
    assert!(gateway.verify(raw, signature, 939_999).is_err());
    assert!(
        gateway
            .verify(raw, &format!("{signature},t=1000000"), 1_000_000)
            .is_err()
    );
    gateway.config.environment = "prod".into();
    assert!(matches!(
        gateway.verify(raw, signature, 1_000_000),
        Err(Error::PaymentMismatch)
    ));
    gateway.config.environment = "test".into();
    gateway.config.store_id = "STO_other".into();
    assert!(matches!(
        gateway.verify(raw, signature, 1_000_000),
        Err(Error::PaymentMismatch)
    ));
}

#[test]
fn utc_expiry_is_strict_and_never_rounded_early() {
    assert_eq!(utc_timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(utc_timestamp("1970-01-01T00:00:00.001Z"), Some(1));
    assert_eq!(utc_timestamp("1970-01-01T00:00:01+00:00"), Some(1));
    assert!(utc_timestamp("2024-02-29T00:00:00Z").is_some());
    assert_eq!(utc_timestamp("1970-01-01T00:00:00.000000001Z"), Some(1));
    assert_eq!(utc_timestamp("1970-01-01T00:00:00.000000000Z"), Some(0));
    assert_eq!(
        utc_timestamp("9999-12-31T23:59:59.9Z"),
        Some(253_402_300_800)
    );
    for invalid in [
        "1969-12-31T23:59:59.9Z",
        "1970-01-01T00:00:00.0000000000Z",
        "2024-02-29t00:00:00Z",
        "2024-02-29T00:00:00z",
        "2024-02-29T00:00:00-00:00",
        "2000-01-0１T00:00:00Z",
        "2023-02-29T00:00:00Z",
        "2024-02-30T00:00:00Z",
        "2024-02-01T00:00:60Z",
        "1970-01-01T00:00:01+08:00",
        "1970-01-01T00:00:01.Z",
    ] {
        assert_eq!(utc_timestamp(invalid), None);
    }
}

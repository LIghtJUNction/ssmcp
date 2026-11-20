# Source dependency compatibility — 2026-11-21

The default branch at `4b73e6b339cd1880c6c7327295189fc96d74cb42` declared newer reqwest, sha2 and TOML packages while its Cargo.lock still recorded the previous dependency graph. A real `cargo build --locked` of the oh-my-ztty source bundle exposed that mismatch.

## Changes

Cargo.lock now resolves the requirements already present in Cargo.toml. The manifest's package requirements and features are unchanged.

sha2 0.11 returns byte arrays without the old LowerHex implementation. A private formatter writes each byte as two lowercase hexadecimal digits. Existing checksum values, leading zeroes, account-cache filenames and persistent identifiers retain their encoding. It adds no dependency.

All production HTTP constructors now use the existing explicit TLS factory. That factory supplies ring, bundled WebPKI roots and HTTP/1.1 ALPN, with TLS 1.2 and TLS 1.3 enabled. Existing timeout, proxy, redirect and header options remain in their caller chains. Account pin configurations still override the preconfigured backend after the factory is called. Production initialization does not depend on installing a global crypto provider.

Independent integration fixtures initialize their own ring provider where they deliberately construct reqwest clients directly. A source scan confirms that the only remaining production Client constructor is inside the explicit TLS factory.

## Local validation

| Check | Result |
|---|---|
| `cargo test --offline --locked --lib digest::tests` | 2 passed; known SHA-256, incremental hashing, leading zeroes and identifier framing |
| `cargo test --offline --locked --lib http_client::tests` | 3 passed in a separate test process; request encoding, certificate rejection before credentials, TLS versions, HTTP/1.1 and hostname verification |
| Integration targets `allow_account`, `client_server_config`, `context_helpers`, `two_tools`, `resources`, `collaboration`, `workspace_bridge`, `oauth_security` | 34 passed, 12 left ignored by their existing definitions |
| `cargo build --offline --locked --release --bin ssmcp` | Passed after both compatibility fixes |
| Constructor scan and `git diff --check` | Passed |

The 12 ignored tests require explicitly enabled Node.js, bubblewrap or rsync scenarios. They are not counted as passing. This change has not been deployed to the running daemon and does not constitute a new formal release.

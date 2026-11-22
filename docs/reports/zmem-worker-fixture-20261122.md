# Worker snapshot integration validation — 22 November 2026

The worker-manager change in `5feff683bb53832f85b3c39993bb9894482dad47` added fields to `worker::Snapshot`. The terminal worker test fixture did not supply them, so library tests failed to compile with E0063 before any filtered test could run.

This change supplies the test process ID, no bound manager and `stopping=false` in that fixture. Production logic, assertions, Cargo.lock and all source pins are unchanged.

## Actual checks

Rust and Cargo 1.100.0 were used in an isolated sandbox, with the shared target directory under one outer ownership lock and `-j2`.

| Source | Command | Result |
| --- | --- | --- |
| Original 5feff683 | `cargo test --locked -j2 --lib worker:: -- --test-threads=2` | Exit 101, E0063; tests did not start. |
| Original 5feff683 production library | `cargo clippy --locked -j2 --lib -- -D warnings` | Exit 0. |
| 5feff683 plus this fixture correction | `cargo test --locked -j2 --lib worker:: -- --test-threads=2` | 42 passed, 0 failed, 0 ignored. Includes six new manager tests and seven terminal-worker tests. |
| Same corrected source | `cargo test --locked -j2 --lib registry::tests:: -- --test-threads=2` | 6 passed; two overlap the worker filter, for 46 distinct passing tests. |

Worker test compilation and execution took 102.005 seconds with dependency-cache reuse; test execution itself took 0.16 seconds. No real remote-agent, external-authentication or cgroup scenario was run. The production-only Clippy result does not assert that all test targets were linted.

## Source identities

- Fixture before SHA-256: `475e30d3fd2c85dac4d2d4902de3378ed58242bffe4a25260dfeb9ef27eacd1c`.
- Fixture after SHA-256: `1a76344b0aecda8633f9eaf59280d5285e6eec16afec9a85927681b67432d99f`.
- Reviewed text patch SHA-256: `ba018236b8300512e3423e1c78793fdb2decd23846b3fc86ffa148596b3fc530`.
- Unchanged Cargo.lock SHA-256: `24d9e9d1c3ede565c41026d997439b609ab8260c282d60a327e5f90fbe893cc5`.

No generated binary or acceptance archive is included. This text record and the adjacent source correction provide the reproducible validation inputs.

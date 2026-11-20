# Native messaging source checkpoint

This independent next-iteration source checkpoint adds explicitly selected native recipients, a bounded authenticated mailbox, broker routing, negotiated wire transport and the terminal sender API. It does not change the current v0.10.14 release candidate.

Recipients use immutable connection IDs and, when available, verified registered agent IDs. A device account inbox is explicitly labeled as delivery to that account on its next eligible call; an observed model label is never treated as an authenticated agent. Cross-client routing checks the sender's live account access to the source and destination, resolves the exact destination identity, and preserves local-user versus agent origin.

Request IDs deduplicate sends across destinations within the bounded receipt history. Mailbox admission is atomic and bounded. Pending messages are not silently evicted. Inclusion in a response and recipient acknowledgement are separate states; neither proves reading or execution. Revoked or disconnected scopes expire without rerouting. Mailboxes and receipts are scoped to the live broker process and connection; this checkpoint does not promise persistence across a server restart.

Only authenticated devices negotiating native_messages_v1 receive the new local protocol. Existing strict wire-v7 peers keep their earlier Ready/frame behavior. Public messages helpers queue plain text: @paths are not implicitly read or transferred.

Validation executed: cargo fmt --all, git diff --check and node --check src/code_mode.js. Seven mailbox regression tests have been written for identity/client/agent isolation, concurrent deduplication, exact acknowledgement, budgets, expiry, capacity and typed origin, but no Cargo compilation or Rust test has run for this checkpoint.

Pending: public HTTP tool/catalog and automatic finalizer integration, terminal recipient picker and draft persistence, compiled/real authenticated transport and UI acceptance, release documentation and versioning. The UI and HTTP work is assigned to the existing collaborating agents in separate source trees.

## 2026-11-20 回执与授权边界增量

源码已补 OAuth resource 范围、目的账号即时权限复核、实际 Session 连接记录和锁外异步终端刷新。UI 持有终端状态锁时发起 send/refresh，由其原输入事件标记重绘，避免递归锁等待。broker Hub 仅在 Linux 编译，跨平台 wire 和客户端保留。

另新增 5 个真实 loopback WebSocket/设备注册与正常测试 OAuth 授权回归定义：旧能力协商、相同显示名的精确选择、纯文本收件、agent 来源与去重、成员凭据隔离、撤权后失效、OAuth resource 限制。新增 6 个独立 Manager 回归及 3 个 wire 回归，覆盖未确认正文、回执匹配、超时、重连与协议校验。这批只完成格式和差异检查，尚未编译或执行；静默 PTY 刷新、公开 MCP finalizer 与应用整合验收仍待统一构建。已有 7 个 Hub 测试同样不能仅凭源码视为通过。

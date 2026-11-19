# Agent 协作原语验收

验证日期：2026-11-18（Asia/Taipei）。验证源码：[b3a209d](https://github.com/LIghtJUNction/ssmcp/commit/b3a209d192acdd736eecfd23e901ed9434739962)。

会话内成员发现、私信、广播、重试去重、收件确认与生命周期检查均已通过。本次在 Linux 开发环境运行核心单测、真实 loopback HTTP/OAuth 集成测试和现有协议回归，使用确定性的客户端 fixture。

## 接口与实现

新增公开 MCP 工具 `collab`，提供 `join/list/update/send/receive/ack/leave` 七个操作。每条在线客户端连接拥有独立成员表和邮箱，参与者凭证绑定会话、授权主体、agent ID 与随机私密令牌。JavaScript 可使用同名 `collab.*` helper。

直接 `receive` 由 Rust 处理，最长等待 30 秒；等待不占用 `code_mode` 的四个 JavaScript 执行槽。JavaScript helper 的等待上限为 1 秒。消息在服务端内存中分发，接收者确认后释放；广播先完成所有收件箱的容量检查，再统一入队。

[完整接口和可复制示例](agent-collaboration.md) · [机器可读验收结果](reports/agent-collaboration-20261118/validation.json)

## 验证结果

| 范围 | 结果 |
| --- | --- |
| 协作核心、HTTP 入口、JavaScript helper | 19 passed，0 failed |
| 真实 HTTP/OAuth 协作 | 3 passed，0 failed |
| HTTP 单元回归 | 16 passed，0 failed |
| 动态资源单元回归 | 4 passed，0 failed |
| 连接与授权单元回归 | 12 passed，0 failed |
| 资源与公开工具 HTTP 回归 | 7 + 7 passed，0 failed，包含显式启用的 ignored 场景 |
| JavaScript 搜索、执行、CUA 回归 | 23 passed，0 failed |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --locked --lib --tests -- -D warnings` | 通过 |

分组命令、退出码、结果摘要和耗时记录在 JSON 中。耗时包含编译与测试启动，用于复现验收过程。

## 合并版本复核

协作变更通过 [PR #15](https://github.com/LIghtJUNction/ssmcp/pull/15) 合入主分支，合并提交为 [90447db](https://github.com/LIghtJUNction/ssmcp/commit/90447db0d6512dae835e12edfceb25438cf7d889)。合并时主分支也包含客户端默认中转服务器与 HTTPS 公钥配置的改动。

对比确认：协作核心、鉴权与 HTTP 接线、JavaScript helper、资源和相关测试共 13 个文件，与已验证源码 `b3a209d` 完全一致。`Cargo.lock` 中 208 个包的名称、版本、来源与校验值保持一致。独立复核未发现中转配置与协作功能的实际冲突；中转配置影响客户端建连，成功连接后的成员和消息语义保持不变。

合并后曾安排追加协作核心与 HTTP 测试；该运行等待开发机全局 Rust 编译锁约 10 分 36 秒，随后在测试执行前取消。此追加运行没有产生通过项。上表记录已完成的 `b3a209d` 验收；合并版本的补充证据为源码、锁文件对比和独立复核，详见 JSON 的 `post_merge_review`。

## 行为证据

- 多个参与者使用同一显示名仍获得独立身份；错误令牌、其他授权主体及其他会话中的凭证均被拒绝。
- 三个参与者之间的私信只进入目标邮箱，广播发送给当时已加入的其他成员。
- 五个独立 HTTP 收件请求同时等待时，第六个参与者仍能广播，并唤醒全部等待者；测试同时检查协作没有调用原生工具。
- 相同幂等键和内容返回原回执；改变内容被拒绝。消息可重复读取，确认后释放，未来游标与越界确认被拒绝。
- 满邮箱使整次广播失败，全部目标均不会收到部分投递。消息、邮箱、会话总量与完整收件 JSON 的字节上限均有测试。
- 等待取消释放接收许可；成员离开、过期或连接关闭会结束等待，旧成员凭证失效。

## 运行边界

每个会话最多 64 个有效参与者，租约为 15 分钟；成功成员操作会续租。单条 payload 最多 16 KiB，每个邮箱最多 128 条且 256 KiB，整个会话的逻辑排队量最多 4 MiB。单次收件最多 64 条且 JSON 业务结果最多 128 KiB。

成员和未确认消息随在线客户端连接存在；开发机重连或服务端重启后重新加入。调用方保存自己的成员凭证、安排收件，并为消息触发的业务副作用处理重复执行。较大的日志和补丁通过路径、提交 ID 或产物 ID 交换。

新增能力位于服务端；本次没有增加依赖或修改客户端 wire 协议。发布后的服务端需要刷新 MCP 工具目录，使直接 `collab` 入口可见。


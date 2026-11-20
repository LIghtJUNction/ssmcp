# 上下文快照预算与来源边界

本阶段修复两个上下文边界：近期已确认的用户要求与最新状态优先进入有界快照；工具提供者不能用保留元数据字段冒充本机历史。生产改动仅涉及 `src/client/context_memory.rs` 与 `src/runtime.rs`。基线为 [23eaf6b](https://github.com/LIghtJUNction/ssmcp/commit/23eaf6b70d4382a915bbbf812727b7d334b5f6e3)，候选为 [4f42a94](https://github.com/LIghtJUNction/ssmcp/commit/4f42a94be2fa54e6f1ef816ae16e9876d08bb998)。

**当前仅完成源码检查，运行验收仍待最终组合门禁。** rustfmt 检查、Git 补丁检查均退出 0，另一位原生协作 agent 完成两文件只读复核，未发现新增阻断。新增三项 Rust 回归尚未编译或运行。本阶段没有启动 Cargo、安装客户端、重载服务或触发 CI。[来源与检查记录](validation.json) 保存源文件 SHA256、实际命令及待测项。

## 近期要求为何会消失

原快照收缩顺序先删除全部 recent 记录，再缩短旧 summary。较长摘要加上其来源索引可能占满约 12 KiB，即使磁盘中仍保留新要求，自动返回视图也会没有近期记录。本轮只读调用中，当前已运行服务确实返回过 `recent=[]`、`recent_omitted=6`、`summary_truncated=true`。该观察不代表新候选二进制已运行。

新顺序先缩短旧摘要，再按需省略旧摘要的来源列表；完整信息仍能通过 `ssmcp.context.read` 读取。若还需删减近期记录，保留当前 recent 中最近一次已确认的用户要求及最后一条状态记录。每条保留记录的消息 ID、SHA256、投递信息和 UTF-8 文本保持原样。新增 `snapshot.context_sources_omitted` 明确说明视图省略了多少旧摘要来源。

快照收缩不修改持久化文档或 JSONL。单条快照上限仍为 12 KiB 减去 256 字节封装预留；原有每次调用聚合 64 KiB 和恢复索引 32 KiB 边界也没有放宽。保护范围是仍在 recent 中的记录，已经被既有 compaction 移到 checkpoint 的旧用户消息不会在这里重新提取。

## 历史字段必须由本机生成

原 `attach_context_metadata` 遇到提供者自带 `_meta["ssmcp.context"]` 时保留该字段并跳过本机注入，而 `AuditRun::capture_context` 会消费此命名空间。较高的伪造 revision 还能压住同次调用中随后返回的真实低 revision。该结论来自生产调用链审阅；本阶段没有向正在运行的服务注入伪造数据。

现在 Runtime 在附加历史之前清除提供者冒用的保留键，包括本机快照不可用或预算不足的情况。只有 Runtime 生成的本机值能进入该命名空间。提供者的正文、结构化结果、错误标志及其他元数据保留。新回归直接连接清理函数与实际 AuditRun 捕获，覆盖成功/错误返回、有/无本机快照以及伪造高 revision。

非对象形式的 `_meta` 仍沿原有兼容规则保持原样，并且不附加本机快照；它也不能被当成历史对象捕获。本补丁保障保留命名空间的来源，不声称修复了所有格式异常的提供者返回值。

## 尚未实施的跨调用去重

审阅期间五次不同 code_mode 调用自动返回了完全相同的 revision 706 快照，每份数组序列化为 8,427 UTF-8 字节。源码也显示每次新调用都会新建捕获容器；现有去重仅发生在单次调用内部。这说明存在重复内容，不是吞吐或延迟基准。

本补丁没有增加跨调用全局缓存。仅按 client 或账户省略快照，会使新 agent 或上下文恢复后的 agent 收不到必要历史。后续可引入调用方显式确认已读取的 workspace/revision，或基于真实授权参与者绑定的确认机制；新 agent、恢复、工作区变化和未知确认状态都应重新获得快照，显示名不能充当隔离身份。

## 待执行的组合回归

新增三项：

- `client::context_memory::tests::snapshot_budget_preserves_fresh_user_and_state_before_old_summary_and_sources`
- `client::context_memory::tests::snapshot_budget_keeps_latest_user_when_escaped_assistant_updates_fill_view`
- `runtime::tests::provider_context_metadata_cannot_spoof_or_mask_local_memory`

最终整合门禁应运行 `client::context_memory::tests::` 全组，以及新增来源测试和已有 `runtime::tests::memory_metadata_preserves_native_output_errors_and_vendor_metadata`、`runtime::tests::native_memory_budget_returns_an_explicit_read_reference_without_changing_output`。这些检查覆盖 UTF-8/JSON 转义预算、来源与投递字段、持久文档不变及旧元数据兼容边界。运行结果必须绑定最终源码和测试二进制，不能用本报告的格式检查代替。

原有 pending/定向用户要求排除规则、工作区校验、真实 CallScope、OAuth 与 ACL 均未改动。恢复索引仍是只读引用，不会重放历史要求或命令。

## 自审

- 贡献：修复快照选择顺序和保留命名空间来源，没有引入新的记忆框架。
- 表述：区分磁盘记录、自动快照和本机历史，不把历史文字当成新用户指令。
- 证据：已有源码检查及只读现场观察；Rust 运行结果明确待补。
- 覆盖：新用例包含旧摘要满额、旧来源列表、JSON 转义膨胀、伪造高 revision 和本机快照缺失。
- 边界：权限、队列领取、agent 绑定、服务部署及跨调用去重没有改变。

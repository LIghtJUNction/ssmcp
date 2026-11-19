# 本地上下文记忆

本地记忆保存当前工作区的检查点与分支，方便后续工具调用重新取得已保存的背景。原始对话继续使用同一份 [JSONL 历史](conversation-history.md)；记忆不会重写原对话、重放要求或工具，也不会调用模型或改变外部模型的上下文窗口。

## 本地命令

| 命令 | 结果 |
| --- | --- |
| `/context` | 列出分支、当前分支与 revision；不接受参数 |
| `/memory` | 查看当前分支的摘要、最近片段和来源；不接受参数 |
| `/compact [SUMMARY]` | 无参数时保存标记为 `local_extractive` 的本地摘录；有参数时保存 `provided_summary`，并保留最近四条记录 |
| `/fork [NAME]` | 从当前记忆创建并激活新分支；名称可省略 |
| `/switch CONTEXT_ID` | 按 `/context` 返回的完整 `context_` 加 32 位十六进制 ID 切换分支 |

命令通过现有异步输入任务执行，结果显示为本地 Context 事件。执行期间继续编辑不会被返回结果覆盖。切换的是记忆分支，当前用户草稿、收件人、JSONL 会话与待发送队列保持各自状态。调用者提供的摘要不会被自动核验；来源 ID 和哈希标识原记录，不证明摘要中的每个判断正确。

## 进入记忆的内容

自动记录仅包含已经成功保存、经过现有脱敏处理的 assistant 文本，以及服务器新确认送达且目标为 `next` 的用户要求。排队中的要求、精确发给某个 Agent 的要求、原始工具输出和控制者元数据不会自动成为共享记忆。

用户要求在发布确认后进入 JSONL，在新的投递回执到达后才进入共享记忆；两者不是同一个时刻。因此，刚送达的要求可能到后续快照才可见。恢复旧投递快照或重复回执不会重新追加记录。

每个片段保留来源 conversation/message ID、SHA-256、原文 UTF-8 字节数，以及截断或省略标记。哈希对应 JSONL 中保留的脱敏正文。读取恢复状态会校验 schema、工作区、容量、来源身份及同一身份的哈希一致性；失败时保留原文件并明确报错。

## 原生工具与只读 UI 投影

本机注册六个工具：`ssmcp.context.status`、`list`、`read`、`compact`、`fork`、`switch`，后五项同样使用完整 `ssmcp.context.` 前缀。修改操作支持 `expected_revision`；过期 revision 会拒绝整次修改，调用者应重新读取后作出决定。

现有 `ssmcp.ui.get` 同时提供同一状态的正式投影：`/context/status`、`/context/read`、`/context/list`。它们没有第二份存储。只读检查可直接使用当前 relay 已有的通用 `call`：

```javascript
return await call("CLIENT_ID_FROM_LIST", "ssmcp.ui.get", {
  path: "/context/status"
});
```

投影返回 `{schema_version, path, value}`，最大 8 KiB。较长记忆使用 `/context/read/summary` 或 `/context/read/recent` 加 `offset`、`limit` 分页；字符串偏移按 Unicode 字符，数组偏移按条目。

显式保存检查点：

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
const status = await call(clientId, "ssmcp.context.status", {});
return await call(clientId, "ssmcp.context.compact", {
  summary: "调用者核对后的当前目标、已完成工作与下一步。",
  expected_revision: status.revision
});
```

未显式绑定 Agent 的 UI 观察调用不领取 Next caller 要求；显式 `collab.bind` 保持现有精确投递规则。原生 context 观察工具的领取分类由承载 `code_mode` 的 relay 实现，不能仅凭工具的只读描述推断运行中 relay 的行为。

## 容量、持久化与交付边界

| 对象 | 上限 |
| --- | --- |
| 一个工作区 | 8 个分支；状态文件 256 KiB |
| 一个分支 | 24 KiB；摘要 8 KiB；最近 8 条记录 |
| 最近记录片段 | 每条 1536 UTF-8 字节；保留原来源与截断标记 |
| 检查点来源 | 最多 16 个；超出时标记省略数量 |
| 单次工具记忆快照 | 每条小于 12 KiB；relay 添加的结构化与文本内容合计最多 64 KiB |

Linux 默认状态位置为 `$XDG_STATE_HOME/ssmcp/context-memory/<工作区哈希>/state.json`；未设置时沿用历史目录的默认状态根。它复用历史的私有目录、文件锁和原子替换机制，不使用项目内另一份对话日志。原 JSONL 仍遵循自己的保留上限；记忆不是无限归档。

本轮交付只更新本机客户端，不部署生产 relay。通用 `call` 可使用已注册的本机原生工具；当前运行环境的只读检查优先使用上述 `ui.get` 投影。源码中的 `context.*` JavaScript helper，以及外层 `structuredContent.context_memories` / `SESSION CONTEXT` 自动添加，都需要对应的新 relay。后者是历史参考，不是新用户要求，且保持原工具结果不变；新要求只由 `user_messages` 表示。

`/handoff [NOTE]` 另行导出当前对话、待发要求和问题为可编辑的私有 `@file` 草稿；它不会自动发送或切换 Agent。工作区记忆、原始历史与显式交接各自使用上述入口，均不恢复模型或工具执行。

[记忆实现与模块回归](../src/client/context_memory.rs) · [隔离端到端夹具](../scripts/e2e_context_memory.py) · [输入与本地沟通](client-input.md)

夹具使用临时工作区与新建的测试 relay；源码和夹具的存在不等于已安装或已运行通过。具体测试、安装与 reload 状态以本轮验证报告为准。

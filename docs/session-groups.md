# 会话连接、逻辑线程与组发

会话页按连接与逻辑线程两层显示。`client_id` 和协作接口中的 `session_id` 标识同一中继连接，重连后须重新发现。`agent_id` 和 `thread_id` 标识该连接下同一个已登记的逻辑参与者。收件地址使用这些 SSMCP 标识，系统进程 PID 和线程 TID 不参与寻址。用本机 `ssmcp.ui.get` 的 `/identity` 可读取实际 PID，字段名为 `process_id`。

## 选择收件人

输入 `/sessions` 打开会话页。上下键选择行；列表显示缩短的 ID，宽屏详情显示完整 ID。按 `i` 可把选中项可用的完整连接 ID、线程 ID 和精确收件人地址显示到当前对话，窄屏也能查看。

连接行下缩进显示该连接的逻辑线程。当前连接的本地用户队列用于排队要求，`/message` 选择的是原生文本收件人。观察到的调用记录供审计查看。只有收件目录公布的地址可用于原生消息和组发。

## 任意组合

在会话页按空格选择或取消一个精确收件人。可以混选不同连接的收件箱和各连接下的逻辑线程，每组最多 32 个。勾选连接行只选中该连接的收件箱。

按 `g` 回到输入框，现有正文保留在 `/group send ` 后面。草稿已有 `/send` 时沿用该命令。检查正文后按 Enter 发送。也可以直接使用：

```text
/group add EXACT_RECIPIENT_ID EXACT_RECIPIENT_ID
/group remove EXACT_RECIPIENT_ID
/group list
/group clear
/group send 要发送的原文
```

地址必须来自当前已验证的收件人列表。展示名称、路径和 `*` 不能代替地址。组消息保持原生纯文本语义，不展开正文里的文件、技能或 shell 命令。

`/send 正文` 跟随最近一次明确选择。当前选择是组时逐个发送原文，后来从 `/message` 选定单个原生收件人时只发给该收件人。后来明确选择本地的 `Next caller` 或已登记线程时，使用匹配的监听器并准备文件与技能引用。组或原生收件人失效时会保留草稿并提示错误，不改投本地监听器。[选择发送路径](force-send-listener.md#选择-send-的收件人)

## 保存组合和查看回执

```text
/group save review team
/group use review team
/group status
/group status GROUP_REQUEST_ID
/group retry
/group retry GROUP_REQUEST_ID
```

命名组合最多保存 16 个。使用客户端 reload 时，当前选择和命名组合会随本地检查点保留；保存内容只有精确收件人 ID 和账号的 provider、subject，不包括访问凭据、正文或发送计划。普通退出后的长期保存不在这个检查点的保证范围内。

恢复选择需要重新验证整个组合。新连接目录中的每个目标都必须仍然在线、可发送且属于原账号，验证完成后才重新勾选。任何目标缺失或账号变化时，整个选择保持待核验；`/group list` 会列出原 ID 与原因。可以等待目录更新，也可以明确重新选择、使用 `/group remove EXACT_ID` 移除失效目标，或用 `/group clear` 清除旧选择。用户的新选择不会被稍后到达的旧恢复结果覆盖。

恢复组合不会自动发送。命名组合在 `/group use NAME` 时仍逐一核对新目录。恢复的 `/group send` 或发往组的 `/send` 草稿必须再次按 Enter，且未通过核验的选择会阻止发送。客户端重连取得新连接 ID 后，旧地址须重新选择。

接受本地发送计划只表示客户端已保留正文和目标。客户端按既有的 16 个待处理请求上限逐步发送，最多 32 个目标不会因为同时达到通道容量而被丢弃。每个目标仍经过现有消息鉴权、精确路由和独立回执。

`/group status` 展示每个目标的状态、消息 ID 和错误。只有全部目标取得接受回执，且目标、草稿正文、草稿修订和光标均与提交时一致，才清空原草稿。部分成功时保留草稿。

同一组请求 ID 绑定完整目标集合、账号身份和正文。每个目标使用稳定的子请求 ID。重试沿用这些 ID，只处理尚未确认且可重试的失败目标。已接受目标即使后续状态查询出错也会被跳过。断线会停止尚未发送的目标。重连后先查看旧组回执，再决定是否另发新消息。

客户端内存中最多保留 128 组回执计划。`queued`、`included` 和 `acknowledged` 分别描述原生消息的交付阶段，任务执行情况由收件方另行报告。

## 验证入口

- 消息调度、混合目标、32 目标流控、部分失败重试、身份变化及断线不重放：`src/client/messaging/groups/tests.rs`。
- UI 多选、命名组合、原文、草稿与逐目标回执：`src/client/terminal/conversation/groups/tests.rs`。
- reload 组合配置、完整集合重验、账号变化与无发送计划恢复：`src/client/terminal/conversation/groups/reload.rs` 的测试。
- 两层结构、完整 ID、既有原生收件人选择和 120/48 列帧：`src/client/terminal/conversation/sessions/tests.rs`。

设置 `SSMCP_INTERACTION_PREVIEW_DIR` 后，帧测试输出 `session-groups-120` 与 `session-groups-48` 的 ANSI 帧和尺寸元数据。帧由正式渲染代码生成，身份来自合成验收夹具。
## 隔离端到端验收

`scripts/e2e_session_groups.py` 使用预先构建的产品程序、生产 loopback 账号路由夹具和独立 VT 解码器。它创建一个源 PTY 与两个收件进程，注册连接 A 上的逻辑线程，再通过会话页逐行选中“连接 A 的收件箱、A 的线程、连接 B 的收件箱”。脚本覆盖 `/group send` 和选中组时的 `/send`，身份提供方使用合成测试账号，收件由夹具处理。

场景核对每个目标的原文、稳定子请求 ID、独立消息 ID、首次确认和重复确认，并检查源客户端重启取得新连接地址后，旧组仍绑定原连接，重试不会重放。输出包含 120/48 列真实 VT 帧、逐目标回执和前后二进制哈希。脚本使用已有可执行文件，运行结果见输出目录内的 `report.json`。

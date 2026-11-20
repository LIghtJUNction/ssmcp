# 会话进程、线程与组发

会话页按两层显示：一个 SSMCP 连接是一个会话进程；这个进程中已登记的协作参与者是会话线程。进程使用真实 `client_id`，线程使用真实 `agent_id`。操作系统 PID、展示名称和模型名称不会被当作消息地址。

## 选择收件人

输入 `/sessions` 打开会话页。上下键选择行；列表显示 ID 前缀，宽屏详情显示完整 ID。按 `i` 可把选中项的完整进程 ID、线程 ID 和精确收件人地址显示到当前对话，窄屏也能查看。

进程行下缩进显示该进程的线程。当前进程的用户队列与原生收件箱保留各自的发送语义；通过 `/message` 进入时仍使用原生文本消息。观察到的调用记录只有审计信息，不能凭名称变成线程收件人。未公布收件箱的进程标题也不能被当作通配符。

## 任意组合

在会话页按空格选择或取消一个精确收件人。可以混选不同进程的收件箱、同一进程的多个线程，或进程与线程的任意组合，每组最多 32 个。选中进程只选择它的收件箱，不会隐式勾选其全部线程。

按 `g` 回到输入框，现有正文保留在 `/group send ` 后面。检查正文后按 Enter 发送。也可以直接使用：

```text
/group add EXACT_RECIPIENT_ID EXACT_RECIPIENT_ID
/group remove EXACT_RECIPIENT_ID
/group list
/group clear
/group send 要发送的原文
```

地址必须来自当前已验证的收件人列表。展示名称、路径和 `*` 不能代替地址。组消息保持原生纯文本语义，不展开正文里的文件、技能或 shell 命令。

## 保存组合和查看回执

```text
/group save review team
/group use review team
/group status
/group status GROUP_REQUEST_ID
/group retry
/group retry GROUP_REQUEST_ID
```

命名组合最多保存 16 个，保存在当前客户端进程内。进程重启后需要重新选择；恢复的 `/group send` 草稿在没有选择时会报错，不会改投默认收件人。组合内固定原账号身份，账号变化后需要显式重新选择。

接受本地发送计划只表示客户端已保留正文和目标。客户端按既有的 16 个待处理请求上限逐步发送，最多 32 个目标不会因为同时达到通道容量而被丢弃。每个目标仍经过现有消息鉴权、精确路由和独立回执。

`/group status` 展示每个目标的状态、消息 ID 和错误。全部目标取得已入队或已接收的服务回执后，才清空仍未被用户修改的原草稿。部分失败保留草稿；修改过的新草稿或新的选择也会保留。

同一组请求 ID 绑定完整目标集合、账号身份和正文。每个目标使用稳定的子请求 ID。显式重试只处理未确认的失败目标，已接受目标不会重发。断线后未发送目标停止，重连不会自动继续旧组；应先查看回执，再决定是否创建新消息。

进程内最多保留 128 组回执计划。以上保证不等于外部副作用恰好执行一次：`queued`、`included` 和 `acknowledged` 仍使用原生消息协议各自的含义。

## 验证入口

- 消息调度、混合目标、32 目标流控、部分失败重试、身份变化及断线不重放：`src/client/messaging/groups/tests.rs`。
- UI 多选、命名组合、原文、草稿与逐目标回执：`src/client/terminal/conversation/groups/tests.rs`。
- 两层结构、完整 ID、既有原生收件人选择和 120/48 列帧：`src/client/terminal/conversation/sessions/tests.rs`。

设置 `SSMCP_INTERACTION_PREVIEW_DIR` 后，帧测试输出 `session-groups-120` 与 `session-groups-48` 的 ANSI 帧和尺寸元数据。画面来自生产渲染代码；身份使用合成验收夹具，不能作为生产连接已经升级的证明。

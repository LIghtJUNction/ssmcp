# 实时上下文与记忆功能迁移

ssmcp 当前只保留进程内的有界会话和事件，不再维护工作区记忆检查点或持久分支。同一进程断网重连保留已有内存；退出、重启或 `/reload` 会清空。旧会话与记忆文件不会作为运行时状态读取，也不会被本次迁移删除。

## 已退役的入口

以下入口不再用于读取、压缩或恢复会话背景：

| 旧入口 | 当前状态 |
| --- | --- |
| `/context`、`/memory` | 工作区记忆视图已退役 |
| `/compact`、`/fork`、`/switch` | 记忆检查点与分支操作已退役 |
| `ssmcp.context.status`、`ssmcp.context.list`、`ssmcp.context.read` | 持久记忆读取工具已退役 |
| `ssmcp.context.compact`、`ssmcp.context.fork`、`ssmcp.context.switch` | 持久记忆修改工具已退役 |
| `ssmcp.ui.get` 的 `/context/status`、`/context/read`、`/context/list` | 旧记忆投影已退役 |

文件与技能来源仍可通过 `ssmcp.context.roots`、`ssmcp.skills.*` 和 `ssmcp.agents.*` 读取。命名连接目标的 `ssmcp context create/use/show` 也继续管理连接配置。这些功能分别处理已有文件和连接目标，不承担会话记忆恢复。[文件与技能来源](context.md) · [命名连接目标](connection-contexts.md)

## 读取当前工作内容

进度与结果继续由 `ssmcp.ui.message` 或 `ssmcp.ui.present` 记录，回执中的 `recorded: true`、`storage: "memory"` 表示内容已经进入进程内存。问题由 `ssmcp.ui.ask` 创建，答案通过 `ssmcp.ui.questions` 读取；各接口继续保留自己的身份与交付规则。

读取当前事件使用 `ssmcp.ui.events`，参数包括 `action: "thread"` 与实际 `client_id`。需要某个成员时传 `agent_id`，需要已认证的未绑定主入口时传 `main: true`；二者互斥，都省略时读取所选进程范围的聚合事件。分页沿用返回的 `generation` 与 `next_cursor`，并检查 `retention_gap_possible`，不能把被裁剪的窗口当成完整记录。[完整字段、来源与保留边界](conversation-history.md)

事件的来源由实际调用时的账号、连接和线程确定。客户端不会把私有线程输入自动合并为共享工作区检查点，也不会把旧文本作为新的用户要求或重新执行其中的工具调用。

## 显式保存与交接

需要把当前内容交给另一个 Agent 时，使用 `/handoff [NOTE]`。它将仍保留的会话、待发要求和未回答问题导出为私有 Markdown 文件，并在输入框准备可编辑的 `@file` 引用。用户随后选择接收人并提交；导出本身不发送消息，也不改变队列状态。该文件由用户管理，后续事件不会自动更新它。[交接范围和容量](conversation-history.md)

`/openai` 展示本进程观察到的调用标识，`/chatgpt set/open/copy/clear` 管理本进程的显式对话链接。它们只使用内存，重启或 `/reload` 后不从旧文件恢复。[调用标识与对话链接](commands.md#查看-openai-调用标识和打开-chatgpt)

外部模型使用哪些上下文、怎样压缩或持久化，由对应 harness 决定；ssmcp 的事件窗口和显式交接不会恢复模型内部会话。[客户端输入与本地沟通](client-input.md) · [自定义客户端的上下文组织](client-extension.md)

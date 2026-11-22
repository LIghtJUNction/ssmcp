# 在 SSMCP 会话中使用 zcalld 原生监听

这个入口把用户显式发送的一条文本交给已配置的本机 zcalld，再把执行结果写回原会话。需要同时使用支持本地 `reply` 的 SSMCP 客户端和包含 `zcalld listen` 的 zcalld。代码进入分支、二进制通过验收、正式发布和本机安装是不同状态；具体版本以对应发布记录为准。

## 启动与发送

先按 zcalld 的现有配置启动 `zcalld serve`。模型路由应已经在本机服务中配置好。监听器不会扫描其他 SSMCP 会话，也不会替用户建立新的模型配置。

对要使用的 SSMCP 客户端调用 `ssmcp.ui.listener`，参数为：

~~~json
{"op":"status"}
~~~

从结果读取 `local_endpoint`，使用其中的私有 Unix socket 路径启动监听器。下面的路径和模型路由是占位值，需要换成实际配置：

~~~sh
zcalld listen \
  --ssmcp-socket "/absolute/path/from/local_endpoint" \
  --model "your-configured-route" \
  --config "/absolute/path/to/your-zcalld-config"
~~~

本地 gateway 若启用了已有的认证机制，可追加 `--token-env ZCALLD_LOCAL_TOKEN`；命令行只传环境变量名，不传凭据值。监听租约的 token 由程序保留在私有内存中。

在同一个 SSMCP 输入框显式选择 **Next caller**，输入：

~~~text
/send 请解释这段代码为什么会返回空值
~~~

然后按 Enter。普通文本的 Enter 仍进入原有 Requests 队列。要把显式输入交给 zcalld，先确认当前目标是 **Next caller**；其他目标遵守当前所选收件人的投递规则，不会静默改投 zcalld。监听器拒收时会保留草稿。

实际命令参数与本地传输说明见 [zcalld 原生监听指南](https://github.com/LIghtJUNction/zcalld/blob/feat/ssmcp-listener-p1-20261120/docs/ssmcp-listener.md) 和 [SSMCP 程序监听器](thread-listeners.md)。

## 接收回执和执行结果

接收和生成是两步。接收程序能确认“这条输入已交给我”，却不能在同一时刻确认稍后的模型生成已经成功。因此两步共用一个 `delivery_id`，各自记录结果。

| 界面或协议状态 | 已经发生的事情 | 仍不能据此推断的事情 |
| --- | --- | --- |
| `accepted` | 监听器明确确认收到这条输入 | 模型已经生成、结果已经保存、整个用户任务已经完成 |
| `execution_state: completed` | 本次生成结果已提交给原会话的回复通道 | 用户已批准结束整个任务 |
| `execution_state: failed` | 本次执行失败，失败结果走同一范围校验 | 原来的接收 ACK 被撤销 |
| `recorded` / `storage: "memory"` / `shown` | 分别说明进程内记录、存储位置与原会话展示状态 | 用户已阅读，或新进程能够恢复旧内容 |

SSMCP 按连接和投递编号保证原输入先显示且只显示一次，再显示该次执行结果。快速回复与稍后到达的 ACK 共用这一去重规则，新编辑的草稿继续按原有规则保护。

原始输入、执行结果与权限校验的实现分别见 [实时消息记录](../src/client/interface/history/listener.rs)、[回复状态机](../src/client/interface/listener/replies.rs) 和 [终端发送逻辑](../src/client/terminal/conversation/listener.rs)。

## 范围与限制

- 当前入口使用 Linux 的私有 Unix socket。目录权限为 0700、socket 为 0600，并核对真实本机 peer；工具 JSON 中的 UID、PID 或展示名不能授予身份。
- 监听程序须显式登记 `enable_reply: true`。认证 MCP 的普通监听登记不因此获得本地回复能力。
- 一次只生成一条。生成期间继续续租；新的显式发送会得到忙碌拒收，保留用户草稿。
- 用户原始正文和执行结果正文各最多 16 KiB，按 UTF-8 字节计算；引用继续经过既有展开与大小校验。
- 回复必须对应同一有效租约中明确 accepted 的输入，且原实时会话归属仍有效。租约过期、关闭、更换或断线后的迟到结果不能插入新会话。
- 相同输入的相同结果返回原回执；改变正文或执行状态会报冲突。客户端不自动重新登记、重跑模型或重发未知结果。
- 执行结果与 `ui.message` 的认证和原会话归属保持各自语义，结束审批已退役。

当前阶段只连接一条显式输入和一条执行结果。输入和结果只记录在当前进程内存中，仍保留的事件可通过 `ssmcp.ui.events` 按真实线程归属分页读取。同一进程断网重连保留内存事件，但不会恢复已经失效的监听租约；重启或 `/reload` 会清空内容，旧文件不参与恢复也不会被删除。[实时事件与保留边界](conversation-history.md)

监听器不自动构造多轮模型上下文，也不执行模型返回的工具调用。模型发现、别名与属性的公共协议、推理强度控制、持久上下文及独立协议库的设计见 [下一阶段协议设计](design/chat-listener-protocol.md)；该设计文档不是当前运行能力的声明。

## 复现验收

`scripts/e2e_zcalld_listener.py` 是隔离验收入口，使用实际 SSMCP PTY、实际 zcalld `listen` / `serve` 和只返回预设数据的本机 HTTP provider。验收应覆盖普通队列与显式发送分离、原文、ACK 与执行结果分离、忙碌、HTTP 失败、调用归属、断线、结果去重以及 120/48 列实际终端帧。测试不调用真实模型。

运行时应提供已冻结的客户端、server、library fixture、VT 解码器和 zcalld 二进制，并放在私有网络与临时目录中。报告中的源码 SHA 参数只是候选标识；必须把二进制摘要与外层冻结构建清单对应，才能把结果归于该源码。实际通过结果以本轮验收报告为准，脚本存在本身不表示通过。

# 程序监听器：网络与本地 socket

`ssmcp.ui.listener` 接收发往本地目标的 `/send <正文>`。接收程序可以通过现有 MCP 网络连接等待，也可以使用 Linux 上的私有 Unix socket。两个入口操作同一个监听器状态，不创建第二份消息队列。

这与普通请求队列、`user.wait`、协作消息收件箱是不同的通道。监听器本身不调用模型，也不把程序返回的数据标成新的用户指令。通过 `code_mode` 显式绑定参与者时，原有外层 `user_messages` 回包规则仍然适用；调用方仍需检查这些字段。

## 登记与身份

网络入口沿用服务器的 OAuth、当前账号 ACL 和实际控制账号计费。工具参数中没有可声明账号、系统 UID 或线程身份的字段。客户端从经过验证的 relay call 中取得实际账号与线程范围：

- 未绑定参与者的调用属于该账号的主线程。
- 已绑定参与者的调用需要有效注册凭据，并在 relay 与客户端协商 `thread_scope: 1` 后携带匹配的会话和线程。
- 展示名、模型名、`run_id` 和工具 JSON 均不能授予身份。账号使用稳定的 provider/subject；登录名变化不会把租约转给另一个账号。
- 长轮询返回后，服务器再次检查当前账号授权和原始参与者绑定。参与者退出或授权被撤回时，结果不会交给已经失效的调用者。

一个连接同时只有一个有效租约。租约保留登记时的账号和线程。即使知道 token，另一个账号或同账号的另一个线程也不能通过网络读取或确认它。

```javascript
// 通过已认证的 MCP code_mode 调用。需要 agent 私有范围时，使用既有
// code_mode agent 参数绑定真实注册参与者；不要把 thread_id 塞进工具参数。
return await call(clientId, "ssmcp.ui.listener", {
  op: "open",
  label: "My receiver",
  idempotency_key: "receiver-start-unique-request-id"
});
```

`open` 返回 `listener_id`、秘密 `token`、`connection_id`、`thread_id` 和时限。`thread_id: null` 表示主线程。token 不进入状态快照和终端提示；接收程序应只在内存或自己的受保护凭据存储中保留它。

可选的 `idempotency_key` 按实际账号／线程隔离。本地登记则按内核提供的 UID、PID 和进程启动时间隔离。同一范围、同一 key、同一 label 的重试返回原租约；label 改变会报冲突。已关闭或过期的登记重试不会静默恢复。每条连接保留最近 128 个 key，连接结束清空；超出这一窗口不能依赖登记去重。重新登记需要显式新 key。

## 读取、确认和线程选择

`read` 最多等待 5 秒，并续期 30 秒租约。一次只允许一个未完成的读取；另一个入口同时读取会收到 `read_busy`。没有 ACK 时，后续读取返回相同消息 ID、游标和正文，便于程序识别重试。

```javascript
const nativeResult = await call(clientId, "ssmcp.ui.listener", {
  op: "read", listener_id, token, wait_ms: 5000
});
if (nativeResult.isError) throw new Error("listener read failed");
const result = nativeResult.structuredContent;
if (result.delivery) {
  // 先由接收程序实际接收或保存，再发送 accepted。
  return await call(clientId, "ssmcp.ui.listener", {
    op: "ack", listener_id, token,
    delivery_id: result.delivery.delivery_id,
    status: "accepted"
  });
}
return result;
```

实际 native 调用的返回值仍采用客户端的 MCP result envelope；示例中的 `result` 指其 `structuredContent`。

消息游标在当前连接内递增。与 `connection_id`、`delivery_id` 一起使用；新连接重新开始计数。更换接收程序不会在旧连接内重置游标。重复且相同的 ACK 返回原回执，冲突 ACK 被拒绝。

终端的接收者选择必须匹配租约范围。主线程不能被静默投给 agent；选定 agent 也不能被投给主线程或另一个 agent。匹配的 agent 监听器可以接收该线程的 `/send`。在程序确认前，终端保留输入草稿。

如果最近明确选择的是组或原生收件人，`/send` 走原生消息路径，失败时保留选中的目标与草稿。要投递给监听程序，请明确选择 `Next caller` 或与租约匹配的已登记线程。[收件人选择](force-send-listener.md#选择-send-的收件人)

ACK 时限是 5 秒。没有读取过的消息超时为失败；已尝试投递但没有确认的消息为“未知”。断线、取消、超时或换租约都不会自动重发未知消息。`accepted` 只表示程序确认收到，不表示模型运行，也不表示外部副作用恰好执行一次。

## Linux 私有 Unix 入口

`status` 和 UI 状态中的 `local_endpoint` 给出当前连接的 socket 路径：

```text
/tmp/ssmcp-<effective-uid>/listener-<pid>-<random>/receiver.sock
```

仅 Linux 提供这个入口。目录为 `0700`，socket 为 `0600`；路径逐层拒绝符号链接，服务端核对真实 Unix peer UID。临时 socket 与持久 OAuth 凭据、账号账本分别管理；监听器消息与回执只保留在当前进程中，不迁移现有用户连接。

同一系统用户的程序可以独立登记主线程监听器。若显式获得一个网络租约的 token，也可以代该租约读取和确认；这属于给本机同 UID 程序的明确委托。远程账号不能用本地租约 token 伪造网络所有者。本地请求中的任何 UID/PID/账号字段都不会成为凭据。

每个 Unix 连接只处理一个请求：4 字节大端无符号长度，随后是 UTF-8 JSON。请求 JSON 与 native 工具参数相同。响应也是相同长度前缀，内容为：

```json
{"ok":true,"result":{"schema_version":1}}
```

失败响应为 `{"ok":false,"error":"..."}`。请求上限 64 KiB，响应上限 256 KiB，同时最多处理 16 条连接，整体请求预算 6 秒。不要在等待响应前半关闭发送端；EOF 或额外字节会取消这一请求，并释放读取占用。

服务端停止时中止自有处理任务，只移除自有 socket inode、锁文件和唯一目录。Unix 入口启动失败时，网络监听器仍可使用，不会开放匿名网络端口。

## 本地生成程序的执行结果

Linux 接收程序可以在 open 时显式设置 enable_reply: true。这个选项仅接受私有 Unix socket 的真实本机 peer；认证 MCP 的 open 仍可登记普通监听器，但不能启用这个本地回复路径。旧调用省略该字段时，接收和确认行为保持不变。

启用后，用户发送时固定当前实时会话归属。程序先 read，再 ack(status: "accepted")，最后针对同一个 delivery_id 提交一条执行结果：

~~~json
{
  "op": "reply",
  "listener_id": "<open 返回的 ID>",
  "token": "<仅保留在程序内存>",
  "delivery_id": "<已 accepted 的消息 ID>",
  "execution_state": "completed",
  "text": "本次生成的正文"
}
~~~

execution_state 可以是 completed 或 failed。返回值的 reply.message_id 标识已记录的执行结果；`recorded: true`、`storage: "memory"` 表示结果保留在进程内存中，shown 表示本次结果已交给原会话的显示区。这些回执只描述监听程序的接收、执行和显示状态，不代表用户已经阅读或同意结束整个任务。ui.message 继续执行认证与原调用归属检查。

回复要求原租约仍有效、原接收回执仍在当前连接的 32 条窗口内，且原实时会话归属仍有效。未确认、拒收、过期、关闭、更换租约或断线后的迟到结果会被拒绝。相同消息的相同正文与执行状态返回原结果；修改正文或状态会报冲突。每条输入最多记录和显示一条结果，旧连接不会自动重发生成或回复。

结果正文最多 16 KiB UTF-8，只接受普通文字、换行和制表符；Unix 请求帧最多 64 KiB。启用回复的原输入同样限 16 KiB，超限在发送前拒绝并保留草稿。程序必须在生成期间继续续租；如果还有一条新输入到达，程序可以明确拒收并说明忙碌，不能把接收 ACK 当成生成完成。该阶段不构造多轮模型上下文，也不恢复旧 token。

监听器回归应覆盖重复回复、原文顺序、失败生成、原调用归属、租约关闭/更换/过期、断线、远端越权及大小边界。

## 当前实现范围与验证

相关源码为 `runtime/call_scope.rs`、`broker.rs`、`client/interface/listener.rs` 和 `client/interface/listener/transport.rs`。socket 所有权校验与可替换公共前端共用 `local_socket.rs`。

线程私有请求、回执和事件只保留在当前进程的各自有界内存中，经过验证的账号与线程隔离继续生效。当前事件通过 `ssmcp.ui.events` 的 `action: "thread"` 分页读取；具体过滤与游标规则见[实时事件](conversation-history.md)。

同一进程断网重连保留内存事件，但断开的监听租约和投递权限仍按上述规则失效。进程重启或 `/reload` 清空内存；客户端不从旧会话、outbox 或记忆文件恢复内容，也不删除这些旧文件。

隔离验收脚本为 `scripts/e2e_listener_transports.py`，使用已有真实生产 router fixture、合成上游 GitHub、真实 CLI/PTY，以及两个独立 Python 接收程序。它不读取真实用户输入、不使用生产登录、不启动模型，也不安装或重载运行中的客户端。运行前提供已经编译好的客户端、VT 解码器和 library test ELF：

```sh
SSMCP_E2E_FIXTURE_BINARY=/absolute/path/to/library-test-elf \
python3 scripts/e2e_listener_transports.py \
  --bin-dir /absolute/path/to/binaries \
  --decoder /absolute/path/to/vt_snapshot \
  --out /absolute/path/to/private-test-output
```

具体运行结果、源码摘要和可执行文件摘要见对应的验收报告。

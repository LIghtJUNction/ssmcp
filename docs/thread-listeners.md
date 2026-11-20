# 程序监听器：网络与本地 socket

`ssmcp.ui.listener` 用于把用户显式输入的 `/send <正文>` 交给一个已登记的程序。接收程序可以通过现有 MCP 网络连接等待，也可以使用 Linux 上的私有 Unix socket。两个入口操作同一个监听器状态，不创建第二份消息队列。

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

选择原生收件箱或原生参与者后，`/send` 会明确拒绝，并保留草稿与当前收件人。要使用监听发送，须显式选择 `Next caller` 或与租约匹配的已登记线程；命令不会作为普通消息发送到原生收件箱，也不会改投默认监听器。`scripts/e2e_force_send.py` 从真实 `/message` 选择入口检查这条规则，并核对草稿、收件人、空原生收件箱和监听回执；既有 Rust 回归继续检查指定 agent 的租约匹配。

ACK 时限是 5 秒。没有读取过的消息超时为失败；已尝试投递但没有确认的消息为“未知”。断线、取消、超时或换租约都不会自动重发未知消息。`accepted` 只表示程序确认收到，不表示模型运行，也不表示外部副作用恰好执行一次。

## Linux 私有 Unix 入口

`status` 和 UI 状态中的 `local_endpoint` 给出当前连接的 socket 路径：

```text
/tmp/ssmcp-<effective-uid>/listener-<pid>-<random>/receiver.sock
```

仅 Linux 提供这个入口。目录为 `0700`，socket 为 `0600`；路径逐层拒绝符号链接，服务端核对真实 Unix peer UID。临时 socket 与持久 OAuth、账号账本、历史记录分开，不迁移现有用户连接。

同一系统用户的程序可以独立登记主线程监听器。若显式获得一个网络租约的 token，也可以代该租约读取和确认；这属于给本机同 UID 程序的明确委托。远程账号不能用本地租约 token 伪造网络所有者。本地请求中的任何 UID/PID/账号字段都不会成为凭据。

每个 Unix 连接只处理一个请求：4 字节大端无符号长度，随后是 UTF-8 JSON。请求 JSON 与 native 工具参数相同。响应也是相同长度前缀，内容为：

```json
{"ok":true,"result":{"schema_version":1}}
```

失败响应为 `{"ok":false,"error":"..."}`。请求上限 4 KiB，响应上限 256 KiB，同时最多处理 16 条连接，整体请求预算 6 秒。不要在等待响应前半关闭发送端；EOF 或额外字节会取消这一请求，并释放读取占用。

服务端停止时中止自有处理任务，只移除自有 socket inode、锁文件和唯一目录。Unix 入口启动失败时，网络监听器仍可使用，不会开放匿名网络端口。

## 当前实现范围与验证

相关源码为 `runtime/call_scope.rs`、`broker.rs`、`client/interface/listener.rs` 和 `client/interface/listener/transport.rs`。socket 所有权校验与可替换公共前端共用 `local_socket.rs`。

线程私有用户请求继续保存在原始请求历史和 outbox；它们不会进入共享 workspace checkpoint。本次没有新增线程持久记忆读取器，也没有把外部分支尚未接通的用户消息／webhook 原型当作公开入口。

隔离验收脚本为 `scripts/e2e_listener_transports.py`，使用已有真实生产 router fixture、合成上游 GitHub、真实 CLI/PTY，以及两个独立 Python 接收程序。它不读取真实用户输入、不使用生产登录、不启动模型，也不安装或重载运行中的客户端。运行前提供已经编译好的客户端、VT 解码器和 library test ELF：

```sh
SSMCP_E2E_FIXTURE_BINARY=/absolute/path/to/library-test-elf \
python3 scripts/e2e_listener_transports.py \
  --bin-dir /absolute/path/to/binaries \
  --decoder /absolute/path/to/vt_snapshot \
  --out /absolute/path/to/private-test-output
```

特定候选版本的实际通过项目、源码摘要和可执行文件摘要记录在对应验收报告中。脚本存在不等于该候选已经通过验证。

已验证候选 1001a0dba75795ca723a24ab4311546500ae4c43 的结果见[2026-11-19 验收记录](reports/thread-listeners-20261119/README.zh-CN.md)：89 项去重 Rust 测试、118 项强制发送检查和 73 项双程序检查通过；该记录不表示已部署运行中的服务。

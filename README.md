# ssmcp

## Live demo

[**Watch the edited demo video (English on-screen guide)**](https://drive.google.com/file/d/1xAn67HJMXeEKS-rpBpaRXlE368ziIRVn/view?usp=drivesdk)

The recording demonstrates a real remote terminal session: discover an online client with `list`, orchestrate its native tools with `code_mode`, execute a shell command, and read the terminal output.

**What you are seeing:** real commands and output from the connected machine — not a simulated terminal.


用 SSH 公钥连接本机 shell 或编码智能体，再通过两个 MCP 工具统一控制：`list` 发现客户端，`code_mode` 用 JavaScript 调用客户端能力。

```sh
# 在不同电脑或不同项目目录中运行，可以使用同一个服务端账号。
ssmcp --name laptop -t shell alice@ssmcp.example
ssmcp --name workstation -t codex alice@ssmcp.example
```

`-t` 选择这条连接提供的本机后端。无论选择 `shell`、`codex`、`cc`、`pi` 还是 `opencode`，远程 MCP 的工具表都只有 `list` 和 `code_mode`。本机实时显示远程操作、输出和智能体事件；按 `Ctrl-C` 断开并清理后端进程。

## 连接

客户端成功启动后，在 stdout 输出一次连接信息：

```json
{
  "url": "https://ssmcp.example/alice",
  "login_code": "本次连接的一次性登录码",
  "login_expires_in": 600,
  "client_id": "服务端分配的64位十六进制连接ID",
  "name": "laptop",
  "type": "shell",
  "cwd": "/home/alice/project",
  "hostname": "alice-laptop"
}
```

在支持 Streamable HTTP、OAuth Authorization Code、S256 PKCE 和动态客户端注册的 MCP 客户端中填写 `url`，启动登录。在浏览器中确认目标 URL、客户端名称和回调来源，再输入 `login_code`。登录码签发后有效 600 秒、成功使用一次即失效；`login_expires_in` 已扣除本机后端初始化耗时。MCP 客户端取得独立的 OAuth access token 并负责保存和续期，登录码不能作为 Bearer token 使用。详见 [OAuth 兼容性](docs/deployment.md#oauth-兼容性)。

同一 SSH 账号可以连接多台电脑或多个本机进程，最多同时在线 128 个客户端。每条连接有独立 ID；显示名字可以相同，调用时始终使用 ID。

授权可以操作同一 SSH 账号下的在线客户端，包括后来连接的客户端。授权绑定签发登录码的 SSH 连接：这条连接断开后，该授权的 access token 和 refresh token 立即失效，其他客户端及其独立授权保持可用。不同 SSH 账号互相隔离。

## 两个 MCP 工具

| 工具 | 参数 | 返回内容 |
| --- | --- | --- |
| `list` | `{}` | 当前账号的在线客户端，以及代码接口说明 |
| `list` | `{"client_id":"…","cursor":"可选分页游标"}` | 指定客户端的原生工具名称、说明和完整参数 schema |
| `code_mode` | `{"code":"JavaScript 源码","timeout_ms":30000}` | 代码的返回值 `result` 和日志数组 `logs` |

MCP 协议方法 `tools/list` 固定返回上述两个工具。先调用工具 `list` 获得 ID、名字、`type`、`hostname` 和 `cwd`，再按需读取某个客户端的能力。大型原生工具表按页返回，使用 `nextCursor` 请求下一页。`cwd_kind: "startup"` 表示 `cwd` 是启动 ssmcp 时的目录；shell 后续执行 `cd`、智能体创建其他工作目录时，这个注册信息保持原值。

### 在代码中操作 shell

以下源码作为 `code_mode` 的 `code` 参数传入。代码可以使用变量、条件、循环、`await` 和 `Promise.all`：

```javascript
const client = clients.find(c => c.name === "laptop" && c.type === "shell");
if (!client) throw new Error("laptop shell is offline");

const status = await call(client.id, "terminal.status", {});
if (status.isError) throw new Error(JSON.stringify(status.content));
await call(client.id, "terminal.write", {data: "pwd\n"});
const output = await call(client.id, "terminal.read", {
  cursor: status.structuredContent.next_cursor,
  wait_ms: 1000
});
console.log(output.structuredContent.text);
return output;
```

shell 的 `terminal.read`、`terminal.write`、`terminal.resize`、`terminal.status` 都是代码内部能力。远程执行命令时发送带换行的 `data`；发送 `"\u0003"` 中断前台程序。持续读取时使用返回的 `next_cursor`。读取可能只得到部分输出，调用方按任务的结束条件继续读取。

### 在代码中使用任意后端

`list({client_id})` 和 `await tools(clientId)` 提供同一套原生能力描述。先查看实际 schema，再通过 `call` 传入它要求的参数。不要根据后端名字猜测工具名。

```javascript
const client = clients.find(c => c.type === "codex");
if (!client) throw new Error("No Codex client connected");
const page = await tools(client.id);
return {
  client,
  tools: page.tools.map(t => ({name: t.name, description: t.description})),
  nextCursor: page.nextCursor ?? null
};
```

代码环境提供以下全局对象和函数：

| 接口 | 语义 |
| --- | --- |
| `clients` | 本次代码开始时的客户端元数据快照 |
| `await call(id, name, arguments = {})` | 调用指定客户端的原生工具，返回完整 MCP 结果 |
| `await tools(id, cursor?)` | 读取指定客户端的一页原生工具 schema |
| `print(value)` / `console.log(...)` | 将日志加入本次返回的 `logs` |
| `return value` | 返回可 JSON 序列化的结果；省略时返回 `null` |

`call` 保留原生 `content`、`structuredContent`、`isError` 和 `_meta`；代码可以据此处理文字、图像、审批请求及错误。原生 `isError: true` 也按原样返回，调用代码应显式检查。找不到客户端、参数不合法或桥接失败会抛出可捕获的异常。原生智能体的权限策略和审批流程继续生效。

每次 `code_mode` 调用启动系统安装的 Node.js 子进程，并使用系统 `bubblewrap` 隔离运行环境。服务端通过有界 JSON 消息与子进程通信；子进程只获得本次客户端列表和桥接接口，不继承服务器凭据。源码最多 64 KiB；默认期限 30 秒，`timeout_ms` 可设为 1–900000 毫秒。运行时限制内存、调用次数和输出，并可中断无限循环。

代码超时会停止后续编排。已经交给本机后端的命令可能继续执行，重试前应通过终端输出、原生事件或状态查询确认结果。

## 客户端模式

```sh
ssmcp --name terminal alice@ssmcp.example                 # 默认 shell
ssmcp --name coder -t codex alice@ssmcp.example
ssmcp --name claude -t cc alice@ssmcp.example
ssmcp --name pi -t pi alice@ssmcp.example
ssmcp --name opencode -t opencode alice@ssmcp.example

# 指定 SSH 配置、密钥、端口或 shell。
ssmcp -F ~/.ssh/ssmcp_config -i ~/.ssh/id_ed25519 -p 2222 \
  --shell /bin/bash --name laptop alice@my-server

# 本机检查某个后端的原生工具，无需 SSH。
ssmcp -t codex --list-tools

# 覆盖后端可执行文件，-- 后的参数直接传给该后端。
ssmcp -t pi --backend /opt/pi/bin/pi alice@ssmcp.example -- --model configured-model
```

每个客户端进程提供一个后端，远程代码可以组合调用不同客户端。各模式的协议、工具发现和使用条件见 [后端说明](docs/backends.md)。

## 两个程序

| 程序 | 运行位置 | 职责 |
| --- | --- | --- |
| `ssmcp` | 本机 Linux | SSH 公钥登录，运行选定的后端并实时显示活动 |
| `ssmcpd serve` | Linux 服务器 | 发布 `/用户名`，维护客户端列表，运行受限 JavaScript 并转发调用 |
| `ssmcpd attach` | SSH 登录后的服务端进程 | 在 SSH 标准输入输出与守护进程 Unix socket 之间转发数据 |

OpenSSH 处理认证及主机密钥验证。守护进程从 Unix socket 的内核凭据获取实际 UID 和用户名，请求数据无法指定其他身份。本机无需为 ssmcp 开放公网端口；MCP 使用 HTTPS，服务器和本机之间使用已经建立的 SSH 连接。

## 安装与升级

需要 Linux 5.3 或更新内核和 OpenSSH。构建使用仓库固定的 Rust 1.100.0。服务器使用系统包管理器安装 Node.js 18+、bubblewrap 0.8+ 和 `sshd`；公网 HTTPS 可由 Caddy 提供。客户端运行所选编码智能体时，需要对应 CLI 已安装并完成本机配置。

```sh
# Debian / Ubuntu：服务端运行依赖
sudo apt install openssh-server nodejs bubblewrap

# Arch Linux：ssmcp-git 包也声明了这些依赖
sudo pacman -S openssh nodejs bubblewrap

# Fedora
sudo dnf install openssh-server nodejs bubblewrap
```

只运行 `ssmcp` 客户端时，不需要服务端的 Node.js 和 bubblewrap。运行脚本随 `make install` 安装到 `$PREFIX/libexec/ssmcp/code-mode.js`，运行时本身由系统包管理器安装和更新。

```sh
cargo build --locked --release
sudo make install
```

`make install` 安装两个程序、man page 和配置示例，支持 `PREFIX` 与 `DESTDIR`。完整服务端配置见 [部署说明](docs/deployment.md)。

**0.3.0 使用私有桥接协议 v4，并将远程工具表收敛为两个工具。** 升级时同时更新 `ssmcp` 和 `ssmcpd`，重新连接客户端，并刷新 MCP 客户端保存的工具表。原有直接调用 `terminal.*` 或智能体工具的脚本应改为通过 `code_mode` 的 `call` 调用。

## 权限与生命周期

启动 ssmcp 就是把本机选定后端交给同一 SSH 账号的 MCP 凭据持有者。shell 具有启动它的本机用户权限；编码智能体遵循它自身的策略。应使用可信服务器和合适的独立 SSH 账号管理这些客户端。

服务器只监听 loopback HTTP，由反向代理提供 HTTPS。携带 `Origin` 时必须匹配配置的公网 origin。access token 只通过 `Authorization` 请求头传递，动态登录码通过 POST 表单提交；这些凭据不写入 URL 或服务日志。客户端成功输出包含登录码，应按凭据保存。

断开客户端后，服务端移除其连接 ID，撤销关联的登录码与 OAuth 授权，并清理本机后端。重新连接会生成新 ID 和新登录码。服务器重启会清空所有连接和凭据。shell 关闭时清理普通前台、后台任务；主动脱离终端会话的守护进程不属于该生命周期。

## 开发与验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# 以下两项要求系统允许 bubblewrap 创建隔离命名空间。
cargo test --locked --lib code_mode::tests -- --include-ignored
cargo test --locked --test two_tools -- --include-ignored
cargo build --locked
python3 scripts/e2e.py --bin-dir target/debug
python3 scripts/e2e_agents.py --bin-dir target/debug
```

真实 SSH 测试使用临时 Ed25519 密钥、独立本机 `sshd`、临时 known_hosts 和随机 loopback 端口，验证 OAuth、客户端发现、系统 JavaScript 调用、终端日志、多客户端并存与断线撤销。`e2e_agents.py` 在五种模式中走 SSH、OAuth 和两个 MCP 工具，智能体使用确定性协议 fixture，不调用模型。CI 在 Linux x86_64 和 arm64 上运行同一套检查。普通 `cargo test` 明确跳过需要真实隔离环境或原生 CLI 的测试；上述专用命令在 CI 中执行真实 Node.js 与 bubblewrap 验证。当前执行结果见 [验收报告](docs/validation.md)。

参考：[MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)、[OpenSSH sshd_config](https://man.openbsd.org/sshd_config)、[Node.js](https://nodejs.org/)、[bubblewrap](https://github.com/containers/bubblewrap)。

MIT License.

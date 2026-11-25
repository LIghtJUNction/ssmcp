# 在本机运行 MCP

`ssmcp local` 在当前 Linux 机器启动 MCP 入口和终端会话。它不需要远程 ssmcp 服务器或 GitHub 登录。文件、命令、终端、会话队列和等待用户仍使用同一套原生执行与授权流程。

本页描述仓库中的 Linux 首版实现。安装包是否包含此命令，以 `ssmcp local --help` 为准；源码合入、正式发布和已经安装的程序是不同状态。macOS、Windows 本地宿主、后台本地会话与 MCP stdio 桥接尚不在此入口的支持范围内。

## 在 Codex 安装本机控制入口

先运行一次注册命令，再在要控制的工作目录启动会话。

```sh
ssmcp mcp install --harness codex
ssmcp local --listen 127.0.0.1:8765
```

注册命令将名称 `ssmcp-local` 和当前系统用户的回环 MCP 地址写入 Codex。重复安装会复用相同的入口，已有同名条目指向其他地址时会提示检查。首次授权时，在另一个终端运行 `codex mcp login ssmcp-local`，输入本机会话 `/logs` 显示的一次性登录码。授权后，可通过 `list` 选择该会话，并通过 `code_mode` 使用其原生工具。

要使用其他端口，在两个命令中都指定同一个 `--listen IP:PORT`。本地会话退出后，工作区工具随之下线。重新启动后，Codex 保留服务器地址，授权使用新会话的一次性登录码。

## 运行前提

本地模式同时承担客户端与 MCP 宿主的工作。除了 `ssmcp` 可执行文件，执行 `code_mode` 还需要已安装的原生 Node.js、Bubblewrap 的 `bwrap` 和 util-linux 的 `prlimit`。这些系统运行依赖不会被打包进客户端。

`node`、`bwrap` 和 `prlimit` 通常从启动进程的 `PATH` 查找；Node 也可用 `SSMCP_NODE` 指向已经安装的原生可执行文件。程序会检查 Node 是否支持隔离运行所需的参数。Bubblewrap 必须能创建本次沙箱使用的用户与其他 Linux namespace；系统策略禁止该操作时，工具编排会报错，不会退回无沙箱执行。

本地执行脚本 `code-mode.js` 与 Linux 客户端一起编译。每次启动本地宿主时，程序把这份同版本内容写入本次会话的 `0700` 私有目录，文件权限为 `0600`，再明确交给沙箱使用；退出时只清理自己创建且身份未被替换的文件。无需另外安装该脚本，也不依赖调试构建的源码回退路径。更新客户端后，已有本地会话继续使用各自的脚本，新会话使用新程序内的脚本。

远程 `ssmcpd` 的安装脚本路径与 `SSMCP_CODE_MODE_RUNNER` 配置保持原有语义；它们不会替代 `ssmcp local` 自己持有的脚本。单独的文件、命令与已安装工具仍有各自的运行前提。本地模式不会下载模型程序、安装上述依赖或修改系统 namespace 策略。实现见[本地宿主](../src/client/local.rs)、[沙箱执行器](../src/code_mode.rs)和 [Node 运行时检查](../src/program.rs)。

## 直接在本机连接

在要共享的项目目录运行：

```sh
ssmcp local --name dev --title "本地开发"
```

默认监听 `127.0.0.1` 的一个可用端口。终端显示“本地会话”，并提示用 `/logs` 查看本次 MCP 地址和一次性登录码。地址形如 `http://127.0.0.1:43123/alice`，实际端口和路径以当前会话显示为准。

连接步骤：

1. 保持这个终端运行，在对话输入区执行 `/logs`。
2. 将其中的 MCP 地址填入**同一台机器上、支持 Streamable HTTP 和 OAuth 的 MCP 客户端**。
3. MCP 客户端打开授权页面时，粘贴这个会话显示的一次性登录码。登录码只使用一次；它不是远程账户密码。
4. 授权后，从 `list` 选择这个实际客户端，再调用文件、终端、命令或其他已加载工具。Esc 从 Logs 返回对话。

使用 Codex 时，运行 `codex mcp add ssmcp-local --url URL` 注册上述 MCP 地址。若需单独发起授权，运行 `codex mcp login ssmcp-local`。同一次在线会话可复用控制应用已获得的授权。关闭本地会话会停止监听、关闭其工具执行环境，并结束这次连接所支持的授权。重启、重连或 `/reload` 后，需要使用新会话的授权信息；固定端口不会让旧凭据永久有效。

需要固定入口时，明确指定一个未被占用的本机端口：

```sh
ssmcp local --listen 127.0.0.1:8765
```

也支持字面 IPv6 回环地址，例如 `--listen '[::1]:8765'`。`0.0.0.0`、局域网地址和主机名不能作为此监听参数。默认的随机端口有助于多个本地会话同时运行。

`127.0.0.1` 指向访问者自己的机器。云端托管的 MCP 客户端不能因此访问你的电脑；把回环地址粘贴到云端页面不会创建隧道。需要跨机器访问时，使用现有的远程模式，或者配置下面的局域网 HTTPS 入口。

## 通过 Unix socket 连接

`ssmcp local --socket` 额外监听一个供本机程序连接的 HTTP/MCP Unix socket。省略路径时，程序使用下表中的默认位置，并在启动消息中显示实际路径；不带 `--socket` 的本地会话继续只提供原有回环 HTTP 入口。参数实现见[本地模式选项](../src/client/local.rs)和 [socket 监听器](../src/client/local/socket.rs)。

| 启动环境 | 默认 socket 路径 |
|---|---|
| 已设置 `XDG_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/ssmcp/mcp.sock` |
| 未设置 `XDG_RUNTIME_DIR` | `/tmp/ssmcp-<有效 UID>/mcp.sock` |

```sh
ssmcp local --socket
```

`--socket PATH` 或 `--socket=PATH` 可以指定其他绝对路径。父目录须已存在、属于当前有效 UID，且权限恰好为 `0700`；程序不会修改已有目录的权限。下面的命令先创建一个私有目录，再以其中的 socket 作为入口：

```sh
socket_dir="$(mktemp -d /tmp/ssmcp-mcp.XXXXXX)"
ssmcp local --socket "$socket_dir/mcp.sock" --listen 127.0.0.1:8765
```

Unix socket 承载的仍是 HTTP，请求地址和认证方式与终端显示的 MCP 地址相同。对上面的固定端口示例，在另一个终端将 `PATH` 换成实际 socket 路径，即可检查 HTTP 连通性；访问 MCP 用户路径还需完成原有的一次性连接码 OAuth 流程，并携带访问令牌。`curl --unix-socket` 只选择连接通道，不改变 URL 中的主机名或端口；服务继续校验 `Host`、`Origin` 和令牌，回环监听也继续可用。路由依据见[本地 HTTP 入口](../src/http.rs)。

```sh
curl --unix-socket PATH http://127.0.0.1:8765/healthz
```

Unix socket 模式使用回环 HTTP 作为授权来源，因此 `--socket` 与 `--public-url` 不能同时使用。后者为已有 TLS 反向代理选择 HTTPS 来源，而当前 Unix socket 监听器承载明文 HTTP；参数校验会明确拒绝这种组合。

Socket 文件只允许相同有效 UID 的进程连接。默认目录权限为 `0700`，socket 权限为 `0600`；完整路径最多 107 字节，文件名最多 64 字节，文件名字符限于 ASCII 字母、数字、`.`、`-` 和 `_`。路径祖先不能是符号链接，也不能允许不受信任的用户写入。已设置但不安全或过长的 `XDG_RUNTIME_DIR` 会明确报错，此时可用 `--socket PATH` 选择一个合适的私有目录。权限与路径检查见[监听器](../src/client/local/socket.rs)和[共享 socket 绑定代码](../src/local_socket.rs)。

已有监听器占用路径时，新会话会报错并保留原监听器。只有属于当前用户、权限符合要求且连接被明确拒绝的旧 socket 才会被回收；普通文件、符号链接和替换后的 socket 都不会被覆盖。正常关闭会移除本次监听器持有的 socket，私有目录和协调后续启动的锁文件可以保留。多个本地会话需要使用不同的自定义 socket 路径。清理行为见[绑定与释放实现](../src/local_socket.rs)。

`scripts/e2e_local_socket.py` 提供针对该入口的独立验收。它使用已有可执行文件、物理 PTY 和临时配置，通过默认路径与自定义路径完成真实 OAuth、MCP 初始化及工具列表请求，并检查活动路径冲突、失效 socket 恢复和退出清理；运行入口见[测试脚本](../scripts/e2e_local_socket.py)。

```sh
python3 scripts/e2e_local_socket.py \
  --bin-dir /path/to/bin \
  --decoder /path/to/vt_snapshot \
  --report /path/to/new-local-socket-report.json
```

## 使用 `NAME.local`

`.local` 名称和 HTTPS 入口是两个独立条件。名称解析告诉客户端连接哪台机器，TLS 则验证连接的服务器并加密流量。仅修改显示地址不能完成其中任何一步。

本地模式可以连接**已经配置好**的系统名称解析与 TLS 反向代理。例如，已确认 `workstation.local` 解析到正确机器，客户端信任该名称的 TLS 证书，且反向代理将该 HTTPS 站点转发到 `127.0.0.1:8765`，才运行：

```sh
ssmcp local \
  --listen 127.0.0.1:8765 \
  --public-url https://workstation.local
```

`--public-url` 必须与非零的固定 `--listen` 端口同时提供，参数次序不限。反向代理的上游端口必须与此监听地址一致。MCP 客户端使用会话显示的 `https://workstation.local/系统用户名`，而不是直接访问代理的 HTTP 上游。

| 条件 | 要求 |
| --- | --- |
| 名称解析 | 每个访问设备都能将该 `.local` 名称解析到正确机器。系统已有的 Avahi、Bonjour 或其他 mDNS 服务可能提供此能力，须实际检查。 |
| HTTPS | 证书覆盖使用的名称，并受访问设备信任。浏览器可打开站点不代表其他 MCP 客户端使用了相同信任配置。 |
| 反向代理 | 转发到明确的回环端口，保留外部请求的准确 `Host` 与 `Origin`；不要将 Host 改成上游的 `127.0.0.1`。 |
| 访问范围 | ssmcp 自身仍只监听回环地址。若反向代理监听局域网地址，局域网可达性由该代理、操作系统和网络配置决定。OAuth 授权仍然必需。 |

`ssmcp local` 不会自动发布 mDNS 记录、修改 hosts、安装信任根、申请证书、开放防火墙或修改反向代理。DNS-SD 的服务实例名称也不等于一个可解析的主机地址。`.local` 的协议含义见 [RFC 6762](https://www.rfc-editor.org/rfc/rfc6762.html)，服务发现与主机地址的区别见 [RFC 6763](https://www.rfc-editor.org/rfc/rfc6763.html)。

## 授权与隔离

本地连接通过新建的私有 Unix socket 进入 Broker。目录权限为 `0700`，两端都会验证对方属于相同的有效系统 UID。不同系统用户不共享默认 socket 目录，也不会修改彼此已有目录的权限。

HTTP 入口仍要求 OAuth。每条本地路由都检查配置的 Host；带 Origin 的请求还必须与配置的来源准确一致。一次性登录码、PKCE、访问令牌、消息队列交付和连接撤销沿用现有实现。本地网关不安装门户、设备注册或账户管理路由。

直接 HTTP 回环模式使用独立的 HttpOnly/SameSite Cookie 名称，以便浏览器能完成本机授权；HTTPS `.local` 与远程服务器的 Cookie 仍保留 Secure。这个例外只适用于显式本地入口中的字面回环地址，不允许普通网络上的 HTTP 登录。Streamable HTTP 的回环绑定、来源校验和鉴权要求见 [MCP 传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)。

登录码和访问令牌能授予当前会话的工具访问权。不要把真实登录码、授权链接或原始 Logs 截图放入公开问题、报告或示例。

## 等待用户与系统通知

支持等待用户原语的版本可以在本地会话使用同样的 `ui.wait`：

```javascript
return await ui.wait(clientId, {
  timeout_ms: 20000,
  intent: "need_user",
  reason: "请确认下一步要处理的文件",
  notify: true
});
```

等待有明确时限。最终响应中的 `user_wait` 回执与 `user_messages` 才能确认实际交付；本地状态或一次唤醒不能替代交付确认。超时要求调用者继续等待，不表示用户同意结束。系统通知在运行客户端的机器上请求发送，结果区分已提交、不可用和失败；提交成功不证明用户已看到通知。

普通 Queue、等待用户与强制发送保持各自的交付规则。本地入口没有为这些机制另建一套实现。

## 验证这个入口

源码中的 `scripts/e2e_local.py` 使用已编译的真实 `ssmcp`、独立物理 PTY 和 `vt_snapshot` 解码器。它在临时 HOME/状态目录内运行，不读取开发者的账户，不启动模拟 GitHub 提供方，也不启动模型。

夹具会将同一份客户端复制到没有脚本文件的临时安装前缀，并移除客户端环境中的 `SSMCP_CODE_MODE_RUNNER`。在真实阻塞等待期间，它检查存活 Bubblewrap 进程实际绑定的本地脚本路径及内容哈希，因此调试构建的源码回退不能替代此项验证。输入目录中的同版本 `code-mode.js` 只提供期望哈希；也可用 `--runner-sha256` 明确指定该哈希，运行中的客户端不读取这份外部参照文件。

```sh
python3 scripts/e2e_local.py \
  --bin-dir PATH_TO_FROZEN_BIN \
  --decoder PATH_TO_VT_SNAPSHOT \
  --browser \
  --report local-validation.json
```

`--browser` 必须找到已经安装的 Node.js、Chromium 和 Playwright；不会下载或自动安装它们。若 Playwright 不在当前 Node 模块搜索路径，可用 `SSMCP_E2E_PLAYWRIGHT_MODULE` 指向安装包的实际入口，必要时用 `SSMCP_E2E_CHROMIUM` 指定 Chromium 可执行文件。

不带 `--browser` 时，脚本使用直接 HTTP 请求验证 OAuth 协议，并明确记录 `browser_cookie_verified=false`。该结果不能代替真实浏览器的 Cookie 验证。测试报告也不会把 Host/Origin 单元验证称为已经部署 mDNS 或 TLS 代理；这些仍需在实际网络配置中另行验证。

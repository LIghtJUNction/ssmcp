# ssmcp

通过一条 SSH 公钥连接，把本机终端提供给远程 MCP 客户端。

```sh
ssmcp alice@ssmcp.xxxx.yyy
```

连接成功后输出一行 JSON，包含服务端分配的 URL 和本次连接的 secret：

```json
{"url":"https://ssmcp.xxxx.yyy/alice","secret":"本次连接生成的64位十六进制字符串"}
```

在支持 Streamable HTTP 和自定义请求头的 MCP 客户端中填写该 URL，并设置 `Authorization: Bearer <secret>`。保持 `ssmcp` 在前台运行；按 `Ctrl-C` 结束连接，服务端随即撤销这个入口和 secret。

## 两个程序，一条连接

| 程序 | 运行位置 | 职责 |
| --- | --- | --- |
| `ssmcp` | 你的 Linux 电脑 | 公钥登录服务器，创建并控制一个独立 PTY 终端 |
| `ssmcpd serve` | Linux 服务器 | 发布 `https://域名/用户名`，校验 secret，转发终端操作 |
| `ssmcpd attach` | SSH 登录后的服务端进程 | 在 SSH 标准输入输出与守护进程的 Unix socket 之间转发数据 |

SSH 由系统 OpenSSH 实现。`ssmcpd` 从 Unix socket 的内核凭据取得实际用户 UID，再查询用户名；请求体无法指定或冒充其他用户。MCP 请求通过 HTTPS 到达服务器，终端读写再沿已有 SSH 连接回到本机。本机无需开放网络监听端口。

每个服务端用户名同时允许一条连接。重复连接会被拒绝，已有连接保持可用。每次重新连接都会生成新的 256 位随机 secret，凭据只在内存中保存。

## 安装

需要 Linux 5.3 或更新内核、Rust 工具链和 OpenSSH。客户端通过 pidfd 跟踪终端进程身份。构建使用仓库固定的 Rust 1.100.0；服务器另需 `sshd`，公网 HTTPS 可由 Caddy 提供。

```sh
cargo build --locked --release
sudo make install
```

`make install` 将两个程序装入 `/usr/local/bin`，同时安装 man page 和配置示例。支持 `PREFIX` 与 `DESTDIR`。服务端的账户、systemd、SSH 公钥和 HTTPS 配置见 [部署说明](docs/deployment.md)。

## 客户端

```sh
# 使用 ~/.ssh/config 和 ssh-agent。
ssmcp alice@ssmcp.xxxx.yyy

# 指定密钥、端口和本机 shell。
ssmcp -i ~/.ssh/id_ed25519 -p 2222 --shell /bin/bash alice@ssmcp.xxxx.yyy

# 使用独立 SSH 配置，也可以使用其中定义的 Host 别名。
ssmcp -F ~/.ssh/ssmcp_config alice@my-server
```

客户端强制使用 SSH 公钥认证，并保留 OpenSSH 的主机密钥检查。已加密私钥的口令由 OpenSSH 或 `ssh-agent` 处理。登录账户的密码和交互式密码认证被禁用。

`ssmcp` 创建的是当前用户权限下的新终端。命令的工作目录与环境来自启动 `ssmcp` 的进程；在终端中执行 `cd` 后，后续命令会继续使用新的目录。启动它的原有终端保持独立。

## MCP 工具

| 工具 | 参数 | 作用 |
| --- | --- | --- |
| `terminal.read` | `cursor`、`max_bytes`、`wait_ms` | 从字节游标读取输出；可以短暂等待新输出 |
| `terminal.write` | `data` | 写入 UTF-8 文本、换行或控制字符 |
| `terminal.resize` | `rows`、`cols` | 设置终端窗口大小 |
| `terminal.status` | 无 | 查询本机终端状态 |

执行命令时调用 `terminal.write`，例如 `{"data":"pwd\n"}`，然后调用 `terminal.read`。发送 `{"data":"\u0003"}` 相当于在该终端按 `Ctrl-C`，用于中断前台程序。

读取结果包含可直接显示的 `text`、保留原始字节的 `data_base64`、下一次读取应携带的 `next_cursor`、`truncated` 和 `running`。初始 `cursor` 为 `0`，每次读取后使用返回的新游标。缓存保留最近 1 MiB 输出；游标早于缓存时，`truncated` 为 `true`，读取从当前可用的最早位置开始。

单次读取默认 16 KiB，最大 32 KiB；单次写入最大 16 KiB。`wait_ms` 默认 `0`，最大 `5000`。窗口行列都在 `1..=1000` 范围内。输入限制、有限的在途请求和输出缓存防止一个请求无限占用内存。

支持 MCP `2026-07-28` 的 `server/discover` 和逐请求元数据，也支持 `2025-03-26`、`2025-06-18`、`2025-11-25` 的 `initialize` 握手。HTTP 响应采用 JSON；通知返回 `202`。不创建 HTTP session，不提供后台 SSE，`GET` 和 `DELETE` 返回 `405`。

## 连接与权限

连接持续期间，secret 持有者拥有该终端用户的命令执行权限。服务器处理终端数据与 secret，应运行在你信任的服务器上。HTTPS 保护 MCP 客户端到服务器之间的数据，SSH 保护服务器到本机之间的数据。

服务器仅监听 loopback HTTP，公网交给 HTTPS 反向代理。存在 `Origin` 请求头时，必须匹配配置的公网 origin。secret 使用 `Authorization` 请求头传递，程序不会把它写入 URL 或服务日志；客户端的成功输出包含 secret，应按凭据保存。

本机退出、SSH 断开或守护进程重启后，原 secret 失效。客户端关闭 PTY 并清理 shell 和普通前台、后台任务。主动脱离终端会话的守护进程不属于这种终端生命周期管理范围。

## 开发与验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/e2e.py --bin-dir target/debug
```

端到端测试使用独立的本机 `sshd`、临时 Ed25519 密钥、临时 known_hosts 和随机 loopback 端口，覆盖真实 SSH 认证、两代 MCP 协议、终端操作与断线撤销。测试不需要生产服务器或已有私钥。CI 在 Linux x86_64 与 arm64 上执行同一套检查。

本次交付的测试结果和证据见 [验收报告](docs/validation.md)。

`ssmcp.xxxx.yyy` 是部署占位域名。部署前将它替换为你实际控制、已指向服务器的域名。

## 参考

- [MCP 2026 Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
- [MCP 2025 Streamable HTTP](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [OpenSSH sshd_config](https://man.openbsd.org/sshd_config)
- [OpenSSH ssh_config](https://man.openbsd.org/ssh_config)

MIT License.

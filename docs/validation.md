# ssmcp 0.1.0 验收报告

日期：2026-11-16（UTC+8）。

`ssmcp` 已实现从 SSH 公钥登录到远程 MCP 控制本机终端的完整链路。Linux x86_64 和 arm64 的 CI 均通过：每种架构各 26 项 Rust 测试、40 项真实端到端检查，以及 Release 构建和安装检查。

## 交付与证据

| 项目 | 结果与链接 |
| --- | --- |
| 仓库 | [LIghtJUNction/ssmcp](https://github.com/LIghtJUNction/ssmcp) |
| 验证的代码提交 | [`3d5e2d39d38a4b0118086f88f77fc059e48f8b85`](https://github.com/LIghtJUNction/ssmcp/commit/3d5e2d39d38a4b0118086f88f77fc059e48f8b85) |
| 完整 CI | [GitHub Actions run 43739064752](https://github.com/LIghtJUNction/ssmcp/actions/runs/43739064752) |
| x86_64 作业 | [ubuntu-24.04，全部通过](https://github.com/LIghtJUNction/ssmcp/actions/runs/43739064752/job/132887999218) |
| arm64 作业 | [ubuntu-24.04-arm，全部通过](https://github.com/LIghtJUNction/ssmcp/actions/runs/43739064752/job/132887999364) |
| 部署文档 | [Linux 部署](deployment.md) |

本报告及 README 的后续文档提交不改变上述已验证代码。

## 实现方式

客户端执行 `ssmcp user@ssmcp.xxxx.yyy`，通过系统 OpenSSH 公钥认证进入服务端的固定桥接命令 `ssmcpd attach`。守护进程通过 Unix socket 的内核凭据取得该连接的 Linux UID，再查出用户名，因此 `/user` 的身份来自服务端操作系统。

服务端为该用户注册一个 MCP 入口，并生成 32 字节系统随机 secret，经现有 SSH 加密连接返回。客户端随后创建一个独立 PTY，输出 URL 和 secret。MCP 调用携带 `Authorization: Bearer <secret>`，由服务端转发到本机终端。每个用户名同时允许一条连接。

系统提供两个 Rust 二进制。`ssmcpd serve` 负责 loopback HTTP 与连接管理；同一个二进制的 `attach` 子命令负责 SSH 标准输入输出桥接。`ssmcp` 负责本机 SSH 子进程和 PTY。HTTPS 使用现有反向代理，SSH 密钥及主机校验使用 OpenSSH 的标准配置。

## 功能与验证对应关系

| 功能 | 验证内容 |
| --- | --- |
| 仅公钥认证 | 检查真实 sshd 有效配置；未授权 Ed25519 公钥、密码和 keyboard-interactive 登录均被拒绝 |
| 身份与 secret | URL 使用实际 SSH 账户名；缺失或错误 secret 被拒绝；同一 secret 无法访问另一用户 |
| 连接隔离 | 同用户名重复连接被拒绝，原连接继续可用 |
| 终端操作 | 真 PTY、窗口调整、命令执行、中文输出、持久工作目录和 Ctrl-C 均通过 |
| 并发与缓冲 | 等待输出时仍可写入；半帧回复跨并发请求正常拼接；1 MiB 输出缓存与参数上限有单测 |
| 生命周期 | 断开后旧 secret 被拒绝；shell 和普通终端作业退出；重连生成不同 secret 和可用的新终端 |
| MCP 2025 | `2025-03-26`、`2025-06-18`、`2025-11-25` 初始化协商和工具列表通过 |
| MCP 2026 | `2026-07-28` discovery、逐请求 metadata、Header 一致性及编码工具名通过 |
| HTTP 边界 | 非法 Origin、无效 JSON、Batch、未知协议版本、超大 body 被拒绝；响应禁缓存；通知不会执行终端操作 |
| Linux 交付 | fmt、Clippy 零警告、Release 构建、PREFIX/DESTDIR 安装及 systemd 文件安装通过 |

Rust 测试包括 25 项库测试和 1 项服务端参数测试。端到端脚本使用真实 `sshd`、临时密钥、临时 known_hosts、独立配置和随机 loopback 端口。成功凭据不写入测试日志。

## 使用入口

构建与安装：

```sh
cargo build --locked --release
sudo make install
```

服务端在配置好专用账户、SSH 公钥策略和 HTTPS 后启动：

```sh
ssmcpd serve \
  --public-url https://ssmcp.xxxx.yyy \
  --listen 127.0.0.1:8080 \
  --socket /run/ssmcpd/control.sock
```

客户端连接：

```sh
ssmcp user@ssmcp.xxxx.yyy
```

MCP 提供四个工具：`terminal.read`、`terminal.write`、`terminal.resize`、`terminal.status`。执行命令时先写入包含换行的 `data`，再按游标读取输出。按本机 `Ctrl-C` 结束连接并撤销 secret。

## 运行边界

客户端要求 Linux 5.3+、可用的 pidfd 和挂载的 `/proc`；构建固定使用 Rust 1.100.0。服务端的标准部署采用 systemd、OpenSSH 与 HTTPS 反向代理。Linux x86_64 和 arm64 已完成 CI 验证。

secret 持有者获得启动 `ssmcp` 的本机用户的终端权限。服务端处理 secret 和终端数据，应由用户信任。终端清理覆盖普通前后台作业；主动通过 `setsid` 脱离会话的程序不属于终端生命周期清理范围。

`ssmcp.xxxx.yyy` 为占位域名。本次完成代码、自动测试与部署材料；实际公网 DNS、证书和服务器安装需要使用真实域名与服务器环境。CI 的 HTTPS origin 是协议配置，完整链路在 loopback HTTP 上测试；公网 TLS 由部署时的反向代理提供。

本地执行环境禁止创建 Unix socket，所以完整跨进程链路以公开的 GitHub Linux CI 结果为验收证据。本地另外完成了可运行的 PTY、协议与权限参数测试，以及安装文件检查。

## 复现

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/e2e.py --bin-dir target/debug
cargo build --locked --release
make install-server DESTDIR=/tmp/ssmcp-install
```

部署主机的 SSH 配置与重载步骤见 [部署文档](deployment.md)。

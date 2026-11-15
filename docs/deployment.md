# Linux 部署

部署结构是 OpenSSH 处理身份认证，`ssmcpd` 处理 MCP 与在线连接，Caddy 处理公网 HTTPS。服务器安装两个 Rust 二进制中的 `ssmcpd`；本机运行 `ssmcp`。客户端需要 Linux 5.3 或更新内核来使用 pidfd。以下命令以常见的 systemd Linux 发行版、`/usr/local` 安装前缀和用户 `alice` 为例。

## 1. 安装程序和服务

```sh
cargo build --locked --release
sudo make install-server
sudo groupadd --system ssmcp
sudo useradd --system --gid ssmcp --no-create-home \
  --home-dir /nonexistent --shell /usr/sbin/nologin ssmcpd
```

已有同名组或服务账户时复用即可。守护进程使用独立的 `ssmcpd` 账户运行。systemd 创建 `/run/ssmcpd`，权限为 `0750`；控制 socket 权限为 `0660`，组为 `ssmcp`。该组只应包含允许使用此服务的账户。

编辑 `/etc/ssmcpd/ssmcpd.env`，将公网 URL 替换为自己的实际域名：

```ini
SSMCP_LISTEN=127.0.0.1:8080
SSMCP_PUBLIC_URL=https://ssmcp.xxxx.yyy
```

公网 URL 只包含 `https://`、主机名和可选端口，不带用户路径、查询参数或 fragment。用户名由服务端在连接建立后追加。

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now ssmcpd
sudo systemctl status ssmcpd
```

服务日志写入 stderr，由 systemd journal 收集：`journalctl -u ssmcpd`。日志不包含 secret 或终端数据。

## 2. 配置 SSH 公钥账户

为每个客户端使用的服务端用户名创建一个普通账户，加入 `ssmcp` 组，并安装其 SSH 公钥：

```sh
sudo useradd --create-home --user-group --shell /bin/sh --groups ssmcp alice
sudo install -d -m 0700 -o alice -g alice /home/alice/.ssh
sudo install -m 0600 -o alice -g alice alice.pub /home/alice/.ssh/authorized_keys
```

已有账户用 `sudo usermod -aG ssmcp alice`。`alice.pub` 是你事先取得的公钥文件；私钥始终留在客户端。账户需要满足发行版的账户和 PAM 登录策略，shell 应为可执行的 `/bin/sh` 或类似 shell，供 OpenSSH 执行固定的桥接命令。

把配置示例安装到 SSH 配置目录：

```sh
sudo install -m 0644 packaging/sshd-ssmcp.conf /etc/ssh/sshd_config.d/60-ssmcp.conf
sudo sshd -t
sudo sshd -T -C user=alice,host=localhost,addr=127.0.0.1
```

确认主配置包含 `Include /etc/ssh/sshd_config.d/*.conf`，并检查上一条命令中的有效配置至少包括：

```text
authenticationmethods publickey
pubkeyauthentication yes
passwordauthentication no
kbdinteractiveauthentication no
forcecommand /usr/local/bin/ssmcpd attach
disableforwarding yes
permittty no
permituserrc no
```

配置中的 `Match Group ssmcp` 会把该组的 SSH 登录固定到 `ssmcpd attach`，包括普通 `ssh`、远程命令和 SFTP 请求。`DisableForwarding` 关闭 TCP、Unix socket、agent 和 X11 转发。组外账户保留原有 SSH 策略。

确认配置通过检查后，按发行版重新加载 SSH 服务，Debian/Ubuntu 常用 `sudo systemctl reload ssh`，其他发行版可能使用 `sshd` 服务名。保留现有管理连接，直到新连接验证成功。

`ssmcpd attach` 默认连接 `/run/ssmcpd/control.sock`。自定义 socket 时，守护进程与 `ForceCommand` 两边都传入同一个 `--socket` 路径。自定义安装前缀时，也要同步更新 `ForceCommand` 的二进制路径。

## 3. 提供 HTTPS

将实际域名解析到服务器，并在 Caddy 配置中加入：

```caddy
ssmcp.xxxx.yyy {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy 与 `SSMCP_PUBLIC_URL` 必须使用同一个域名。按 Caddy 的部署要求开放 HTTPS 端口并完成证书签发。8080 是 loopback 内部端口，无需向公网开放。代理必须保留请求路径与 `Authorization`、`Origin`、`MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name` 请求头。

浏览器请求的 `Origin` 必须等于配置的公网 origin；没有 `Origin` 的服务器端 MCP 请求可正常使用。当前接口面向可配置 Bearer 请求头的 MCP 客户端。

## 4. 建立终端连接

在本机安装 `ssmcp` 并执行：

```sh
ssmcp alice@ssmcp.xxxx.yyy
```

首次连接按 OpenSSH 的提示核验服务器主机密钥，或提前配置可信的 known_hosts。收到 JSON 后，把 `url` 和 `secret` 填入 MCP 客户端。连接未成功时不会创建本机终端或输出凭据。

每次连接创建一个新 PTY。secret 与本次连接绑定；退出再连接后，应更新 MCP 客户端保存的 secret。一个用户名同时连接多个本机会被拒绝，需要不同的服务端账户。

## 5. 检查运行状态

`http://127.0.0.1:8080/healthz` 返回守护进程健康状态，不列出用户名或 secret。对 `/alice` 的请求每次都需要 Bearer。通过一次 `terminal.write` 和 `terminal.read` 验证端到端命令执行，随后退出本机 `ssmcp` 并确认原 secret 得到 `401`。

本仓库附带的 `scripts/e2e.py` 在临时本机环境执行相同的认证与终端流程。生产部署还需验证实际 DNS、HTTPS 证书、反向代理和 SSH 配置；示例域名没有预先部署任何服务。

## 参考

- [OpenSSH 有效配置与 Match](https://man.openbsd.org/sshd_config)
- [systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)
- [Caddy reverse_proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)

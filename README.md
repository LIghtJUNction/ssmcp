# ssmcp

**把你电脑上的 Shell 和编码智能体转为远程mcp服务**

[![CI](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.100.0-64748b?style=flat)](rust-toolchain.toml)
[![Linux](https://img.shields.io/badge/Linux-x86__64%20%C2%B7%20arm64-64748b?style=flat)](.github/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-64748b?style=flat)](LICENSE)

一条 SSH 连接，两个 MCP 工具。`list` 找到在线客户端，`code_mode` 调用它们的原生能力；命令、输出和智能体事件实时显示在本机终端。项目、模型配置和后端工具执行都留在本机。[工作原理](docs/backends.md)

本工具有意模仿ssh，尽可能保持功能简洁。

![本机工具通过 SSH 连接服务器，MCP 客户端经 HTTPS 和 OAuth 发现并调用工具；结果沿原连接返回。](docs/assets/connection-flow.svg)

[**配置与登录教程**](docs/configuration.md) · [服务器部署](docs/deployment.md) · [后端与工具接口](docs/backends.md) · [验收记录](docs/validation.md) · [真实操作视频](https://drive.google.com/file/d/1xAn67HJMXeEKS-rpBpaRXlE368ziIRVn/view?usp=drivesdk)


# 应用场景
1. ChatGPT chat模式 Pro模型控制你本地的codex（还支持别的）完成软件工程任务。
2. 使用我的AI订阅，远程控制朋友的电脑，帮朋友配置电脑环境，安装软件，安装ai工具。
3. ...你来想想

# 安全警告⚠️

本工具安全边界如下：
1. 中转服务器可信任，😌作者我是可信任的，但我还是建议你自己部署。
2. 授权流程如下，服务器同意你接入（你的公钥已经在服务器里面了），客户端和服务器建立连接，服务器给你发一个一次性令牌。不要泄露这个令牌。
打开ChatGPT，打开插件，点击添加。
参数：https://ssmcp.lmm.best
没了，接下来保存即可
点击连接，这一步是跳转Oauth登录页面，把刚刚令牌输入进去，就代表你是你了。
3. 第三方登录（开发中），也就是github登录以及邮箱登录功能，这样客户端使用第三方账号登录，连接时使用相同的，第三方账号登录，不用担心令牌泄露，一次性令牌登录我依旧会保留。
4. 建议，最好还是私有化部署，不要用公共的，需要自备域名和服务器，源代码都开源。


## 快速开始

服务器已部署 `ssmcpd`、接受你的 SSH 公钥后，在本机项目目录启动客户端。已有 `~/.ssh/config` 可以直接复用：

```sshconfig
Host my-ssmcp
    HostName ssmcp.example.com
    User alice
    Port 22
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
```

```sh
cd ~/projects/ssmcp
ssmcp --name ssmcp-project -t codex my-ssmcp
```

`my-ssmcp` 是 SSH 配置别名；用户名、地址、端口和密钥交给系统 OpenSSH 解析。也可使用 `ssmcp alice@ssmcp.example.com`，或 `-F ~/.ssh/work_config` 指定配置文件。0.3.1 起支持省略用户名；已安装的 0.3.0 若提示 `destination must be user@host`，可先用 `alice@my-ssmcp`，或[重新构建安装](#安装与升级)。[SSH 配置细节](docs/configuration.md)

**已经能用同一把公钥连接该服务器账号，就可以继续用它。** 首次登记和换钥都由服务器 OpenSSH 管理；受限账号启用 `ForceCommand` 后，通过独立管理员账号或服务器控制台追加新公钥。完整教程包括首次配置、保留旧钥、验证新钥和撤销旧连接的步骤。[公钥配置与更新](docs/configuration.md#更新公钥)

启动成功后，终端会输出一次连接信息：

```json
{
  "url": "https://ssmcp.example.com/alice",
  "login_code": "本次连接的一次性登录码",
  "login_expires_in": 1800,
  "client_id": "服务端分配的连接 ID",
  "name": "ssmcp-project",
  "type": "codex",
  "cwd": "/home/alice/projects/ssmcp",
  "hostname": "workstation"
}
```

将 `url` 添加到支持 OAuth 的 MCP 客户端，在它打开的登录页输入 `login_code`。授权完成后，先调用 `list` 选择客户端，再用 `list({client_id})` 查看原生工具及参数。**保持本机 ssmcp 进程运行。** [逐步登录教程](docs/configuration.md#登录流程)

## SSH 连接，OAuth 授权

SSH 用公钥确认这台电脑可以连接服务器账号。OAuth 将一次性登录码换成 MCP 客户端保存的访问凭据。这个授权可以访问同一 SSH 账号下的所有在线客户端，并绑定签发登录码的那条 SSH 连接。[授权实现](src/oauth.rs)

![公钥登记、本机启动、一次性登录码授权与 MCP 调用的四步流程。](docs/assets/login-flow.svg)

| 你做的操作 | 登录状态 |
| --- | --- |
| 重新打开 ChatGPT 或其他 MCP 应用，本机 ssmcp 仍在线 | 应用保存的令牌仍有效时可以继续使用；支持 refresh token 的客户端可自动续期 |
| access token 到期，客户端持有有效 refresh token | 支持刷新的客户端静默换取新令牌并重试，无须输入登录码 |
| 退出后重新启动签发授权的 ssmcp | 旧授权已撤销，用新连接的登录码重新授权 |
| 再开一个项目客户端，原授权连接仍在线 | 同一 SSH 账号的现有授权可以发现它 |
| 重启 `ssmcpd` | 内存中的连接与 OAuth 状态清空，需要重新连接并授权 |
| 提示 `invalid_grant` 或登录码过期、已使用 | 按[登录恢复步骤](docs/configuration.md#登录恢复)取得新码并重新授权 |

0.3.2 默认将登录码有效期设为 **30 分钟、使用一次**，access token 有效期设为 **8 小时**。refresh token 每次成功刷新都会轮换，并把闲置期限向后延长 **30 天**，同一授权从首次登录起最长 **90 天**。客户端登记默认有效 **365 天**。实际剩余时间受授权和登记到期时间约束；终端的 `login_expires_in` 已扣除后端启动时间。[有效期与服务端配置](docs/deployment.md#oauth-lifetimes)

**访问令牌到期后，MCP 客户端可以用 refresh token 自动续期。** 新注册省略 `grant_types` 时，服务端默认提供刷新授权；客户端需保存每次返回的新 refresh token，并实现刷新请求。服务器返回到期提示与新令牌，自动发起刷新由 MCP 客户端负责。[静默续期流程](docs/configuration.md#silent-refresh)

## 五种后端，同一个入口

| 本机后端 | 启动方式 | 运行内容 |
| --- | --- | --- |
| Shell | `ssmcp --name terminal my-ssmcp` | 有状态的本机 Shell 与 PTY；默认模式 |
| Codex | `ssmcp -t codex --name coder my-ssmcp` | 已安装 Codex 的 App Server |
| Claude Code | `ssmcp -t cc --name claude my-ssmcp` | 已安装 Claude Code 的 MCP 工具 |
| Pi | `ssmcp -t pi --name pi my-ssmcp` | 已安装 Pi 的 JSONL RPC |
| OpenCode | `ssmcp -t opencode --name opencode my-ssmcp` | 已安装 OpenCode v2 的服务接口 |

每个进程提供一个后端，同一账号最多同时在线 128 个客户端。`--name` 是显示名称，调用始终按服务端分配的 ID 路由；`cwd` 记录启动目录。Agent 的安装、模型配置和原生权限在本机完成，支持范围以实际发现的 schema 为准。[后端要求与示例](docs/backends.md)

```sh
# 本机发现工具，无需 SSH，也不建立远程连接。
ssmcp -t codex --list-tools

# 使用另一份 SSH 配置或已有私钥。
ssmcp -F ~/.ssh/work_config -t codex my-ssmcp
ssmcp -i ~/.ssh/id_ed25519 -p 2222 alice@ssmcp.example.com

# 自定义后端路径；-- 后的参数交给本机后端。
ssmcp -t pi --backend /opt/pi/bin/pi my-ssmcp -- --no-session
```

## 两个工具就够了

| MCP 工具 | 用法 |
| --- | --- |
| `list({})` | 列出当前账号的在线客户端：ID、名字、类型、主机和启动目录 |
| `list({client_id})` | 读取指定客户端的原生工具名称与参数 schema；用 `nextCursor` 分页 |
| `code_mode({code, timeout_ms})` | 用 JavaScript 调用原生工具，返回结果和日志 |

例如，对名为 `terminal` 的 Shell 客户端执行 `pwd`。将下面源码放入 `code_mode` 的 `code` 参数：

```javascript
const client = clients.find(c => c.name === "terminal" && c.type === "shell");
if (!client) throw new Error("terminal is offline");

const before = await call(client.id, "terminal.status", {});
if (before.isError) return before;
const sent = await call(client.id, "terminal.write", { data: "pwd\n" });
if (sent.isError) return sent;
return await call(client.id, "terminal.read", {
  cursor: before.structuredContent.next_cursor,
  wait_ms: 1000
});
```

单次读取可能只有部分输出，继续用返回的 `next_cursor` 读取。调用 Agent 时，先用 `list({client_id})` 或 `await tools(client.id)` 检查实际 schema。`call` 保留原生 `content`、`structuredContent`、`isError` 和审批数据；代码应检查原生错误与任务状态。[完整 JavaScript 接口](docs/backends.md#3-用-code_mode-编排调用)

代码由服务器的系统 Node.js 在 bubblewrap 隔离环境中执行，通过 SSH 将原生调用交给本机后端。每次编排独立运行；源码、内存、调用次数、输出和时长都有上限。代码超时后，已经发给后端的操作可能继续，重试前先检查执行状态。[资源与生命周期](docs/backends.md#传输与资源边界)

## 安装与升级

当前支持 Linux 5.3+，仓库固定构建工具链为 Rust 1.100.0。客户端需要 OpenSSH 和所选本机后端；服务端需要 OpenSSH Server、系统 Node.js 18+、bubblewrap 0.8+ 及 HTTPS 反向代理。[完整部署教程](docs/deployment.md)

```sh
# 在仓库目录构建，并安装程序、运行脚本、man page 和文档。
cargo build --locked --release
sudo make install
```

服务端依赖通过系统包管理器安装：

```sh
# Debian / Ubuntu
sudo apt install openssh-server nodejs bubblewrap
# Arch Linux
sudo pacman -S openssh nodejs bubblewrap
# Fedora
sudo dnf install openssh-server nodejs bubblewrap
```

`ssmcpd serve` 发布 HTTPS 后面的 MCP 服务；SSH 登录后的 `ssmcpd attach` 转发连接数据。`make install-server` 另安装 systemd unit 和初始配置，具体账户、域名与反向代理设置见[服务器部署](docs/deployment.md)。只运行客户端时无需安装服务端的 Node.js 和 bubblewrap。

0.3 系列使用私有桥接协议 v4。跨协议版本升级时同时更新两端；重启服务后重新连接、完成 OAuth 登录并刷新 MCP 工具表。原有直接调用 `terminal.*` 或 Agent 工具的脚本，通过 `code_mode` 的 `call` 调用。

0.3.2 的长期授权与默认刷新支持需要更新 `ssmcpd`。升级重启后，在 MCP 客户端重新注册并登录一次；以后保持签发授权的 SSH 连接在线，由支持刷新的客户端自动续期。服务端重启和 SSH 断线仍会撤销相应授权。[升级步骤](docs/deployment.md#upgrade-032)

## 验证与贡献

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
# 需要系统允许 bubblewrap 创建隔离命名空间。
cargo test --locked --lib code_mode::tests -- --include-ignored
cargo test --locked --test two_tools -- --include-ignored
python3 scripts/e2e.py --bin-dir target/debug
python3 scripts/e2e_agents.py --bin-dir target/debug
```

CI 在 Linux x86_64 与 arm64 上运行检查。SSH 测试使用临时密钥、独立 sshd、临时 known_hosts 和 loopback 端口；五模式链路测试使用确定性 Agent 协议 fixture，不发起模型推理。默认测试中需要系统隔离或单独安装原生 CLI 的探针会明确标为 ignored。[测试结果与范围](docs/validation.md) · [CI 工作流](.github/workflows/ci.yml)

## 文档导航

| 你想做什么 | 从这里开始 |
| --- | --- |
| 复用 SSH 配置、首次登记公钥、更新密钥 | [配置教程](docs/configuration.md) |
| 登录 ChatGPT / MCP，恢复过期授权 | [登录与恢复](docs/configuration.md#登录流程) |
| 安装服务端、配置 systemd / SSH / HTTPS | [Linux 部署](docs/deployment.md) |
| 编排 Shell 或 Agent 的原生能力 | [后端与代码模式](docs/backends.md) |
| 查看测试、架构与实际验收范围 | [验收记录](docs/validation.md) |
| 看真实终端操作 | [演示视频，英文说明](https://drive.google.com/file/d/1xAn67HJMXeEKS-rpBpaRXlE368ziIRVn/view?usp=drivesdk) |

[MIT License](LICENSE) · [OpenSSH](https://www.openssh.com/) · [MCP](https://modelcontextprotocol.io/)

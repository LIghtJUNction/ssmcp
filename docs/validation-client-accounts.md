# 客户端账号、临时设备与默认看门狗验收记录（0.7.0）

日期：2026-11-17（Asia/Taipei）  
仓库：<https://github.com/LIghtJUNction/ssmcp>

## 交付状态

本次完成了 0.7.0 的源码实现、开发机构建、定向回归和 GitHub 提交。运行中的公共服务、当前用于开发的管理客户端和发行资产没有被替换。本记录不代表公网实例已部署，也不代表完成了真实 GitHub OAuth 或 Windows / macOS 现场验收。

实现提交：

- [默认看门狗与显式禁用](https://github.com/LIghtJUNction/ssmcp/commit/42eba5d2dde58f14cffb87f74932652d133a72bc)
- [账号、HTTPS 设备接入、临时命令与交互终端](https://github.com/LIghtJUNction/ssmcp/commit/b5a4c949143627cb5592faa5f5fd82b7bb90b67e)
- [命令、配置、手册与使用文档](https://github.com/LIghtJUNction/ssmcp/commit/c70209050390090a913830380c0c2d7f5914e12e)

## 实现范围

| 用户操作 | 当前行为 |
| --- | --- |
| `ssmcp login` | 复用 GitHub 浏览器授权和明确的设备确认，保存按服务器隔离的客户端凭据 |
| `ssmcp whoami` | 查询已验证身份、服务器与有效期；不输出令牌 |
| 登录后运行 `ssmcp` | 经 HTTPS / WSS 接入，并自动绑定自己的 GitHub 稳定账号 ID，无须重复写自己的 `--allow` |
| `ssmcp list` / `ls` | 列出当前账号有权控制的在线会话，支持 JSON |
| `ssmcp -i` / `list -i` | 选择远端会话，进入终端，传送输入、输出、Ctrl-C 和窗口尺寸；Ctrl-] 离开后保留远端终端 |
| `ssmcp invite` | 生成一次性临时接入命令；默认从同一服务器下载临时客户端，B 无须预装 ssmcp 或 SSH |
| `ssmcp join` | 兑换邀请，在 B 前台创建临时被控会话；临时设备凭据只保存在内存 |
| 默认看门狗 | 网络断开后按默认 5 秒间隔恢复；`--no-watchdog` 禁用 |
| `ssmcp logout` | 撤销这次客户端登录及其派生的邀请和临时设备凭据 |

原有 SSH、显式 `--allow`、本地后台会话和重载接口继续保留。`ssmcp sessions list` 仍列出本地保存的会话，远端账号列表用 `ssmcp list` 或 `sessions list --remote`。旧的 `-i FILE HOST` 私钥参数继续支持，也可写成 `--identity FILE`。历史保存会话沿用原 watchdog 策略，新建会话采用默认启用。

## A 控制 B 的操作

A 登录已经升级的服务器：

~~~sh
ssmcp login --server https://ssmcp.example.com
ssmcp whoami
ssmcp invite
~~~

复制输出的完整单行命令，到 B 的终端执行并保持进程运行。A 再执行：

~~~sh
ssmcp list
ssmcp -i
~~~

B 使用 PowerShell 时，A 生成 `ssmcp invite --shell powershell`。B 已安装兼容版本时，可用 `ssmcp invite --no-bootstrap`。

邀请默认 10 分钟内可兑换一次，兑换后的设备会话默认 1 小时，且不超过签发它的客户端登录剩余期限。`--ttl` 接受 30–3600 秒，`--session-ttl` 接受 60–86400 秒。临时会话不能持久化为后台服务或执行进程重载；结束后重新生成邀请。完整说明见[客户端账号与临时接入](client-accounts.md)。

## 身份与生命周期

账号凭据绑定服务器 origin 与 GitHub 数字 ID。不同服务器的凭据独立保存；普通 HTTPS / WSS 接入不依赖 GitHub 用户名再次解析。旧 SSH 路径自动填写本人 `--allow` 时，服务器会核对当前 GitHub 用户名仍对应已保存的稳定 ID，无法验证时停止自动授权。

长期客户端令牌不会进入邀请命令。邀请只能兑换一次，派生设备凭据不能列出或调用账号的其他会话，也不能继续签发邀请。服务器持久化令牌摘要，Unix 本地凭据目录与文件分别限制为 0700、0600。下载路径采用固定平台白名单和安全文件打开；引导下载来源固定为服务器配置中的 origin，不使用请求 Host，不跟随重定向。

设备授权过期、注销撤销或服务器明确返回 401/403 时，客户端结束连接并停止重试。测试发现过“刚收到 Ready 即被撤销”的竞态：连接关闭可能先于最终拒绝帧被读取，导致错误恢复。修复后，客户端在 relay 退出时先读取已到达的结束原因，两种撤销时序都已通过真实进程检查。[授权实现](../src/device.rs) · [传输实现](../src/client/relay.rs) · [连接生命周期](../src/client.rs)

## 验证结果与边界

| 检查 | 结果 | 覆盖范围 |
| --- | --- | --- |
| `cargo test --lib -- --test-threads=8` | 558 通过，7 忽略，0 失败 | 集成检查点的全库运行；随后新增和调整的账号、下载、错误响应代码分别进行了下列定向验证 |
| `cargo test --lib client::account::tests` | 11 通过 | 凭据隔离、权限、链接拒绝、URL、命令引用、重定向及 `--server URL -i` 与旧私钥参数区分 |
| `cargo test --lib device::tests` | 3 通过 | HTTP / WebSocket 授权角色、账号隔离、一次性兑换、注销与慢请求竞态 |
| `cargo test --lib portal::setup_tests` | 10 通过 | 登录确认流程及稳定 GitHub 身份核验 |
| `cargo test --lib client_downloads::tests::` | 5 通过 | 文件与平台约束、真实本地 HTTP 下载、伪造 Host 无效及正确字节返回 |
| `cargo test --locked --test client_startup --test client_reload --test watchdog` | 17 通过 | 旧 SSH 启动、重载、默认重连和显式禁用 |
| `python3 scripts/e2e_account_relay.py --binary target/debug/ssmcp` | 37 项检查通过 | 真实客户端进程、本地模拟 HTTP / WebSocket 网关、实际工具执行、A/B 隔离、邀请重用拒绝、重连、撤销与清理 |
| `python3 scripts/e2e_account_interactive.py --bin-dir target/debug` | 6 项检查通过 | 真实 A CLI 与 Linux PTY、真实 B Shell；验证输入、Ctrl-C、窗口尺寸、Ctrl-] 以及后置 `-i` |
| `python3 scripts/test_client_bootstrap.py` | 4 通过 | POSIX 平台选择、curl / wget、参数传递、下载失败与临时目录清理 |
| `sh -n src/client_downloads/install.sh` | 通过 | POSIX 脚本语法 |
| `cargo check --locked --target x86_64-pc-windows-gnu --bin ssmcp` | 通过 | Windows 客户端 Rust 交叉编译检查 |
| `cargo clippy --all-targets --locked -- -D warnings` | 通过 | 所有本地目标静态检查 |
| `cargo fmt --all -- --check` 与 `git diff --check` | 通过 | Rust 格式与补丁空白检查 |

这些检查没有访问真实 GitHub 用户授权流程。relay 端到端脚本使用模拟网关；交互脚本的 API 同样由本地 fixture 提供，它驱动真实 Shell / PTY，但不使用真实 ssmcpd 或 B 的 WSS 连接。服务端真实路由和 Broker 行为由 Rust 测试单独覆盖。不能据此宣称公网 TLS、现有反向代理路由或生产 OAuth 配置已经验收。

Windows GNU 目标通过交叉编译检查，但没有执行 Windows 客户端、PowerShell 引导脚本或 MSVC / ARM64 发行构建；开发机未提供 pwsh。macOS 客户端没有现场运行。POSIX 引导需要系统 Shell 与 curl 或 wget；Windows 使用 PowerShell / .NET。Linux 下载目标沿用 GNU libc 发行约束。

## 部署需要具备的条件

运行新功能需升级实际服务进程，配置已有 GitHub OAuth 与账号门户，并确保登录、`/setup`、`/api/client/*`、`/mcp` 和 OAuth 回调共享同一个 Broker 的状态。`/api/client/connect` 需要转发 WebSocket Upgrade；`/client/*` 需要指向提供下载的新服务。

默认 bootstrap 需要设置：

~~~toml
[server]
client_download_dir = "/var/lib/ssmcpd/clients"
~~~

该目录放入与 0.7.0 接口兼容的目标平台原始二进制，按文档中的 `ssmcp-TARGET` / `ssmcp-TARGET.exe` 命名。没有配置目录或缺少目标文件时返回 503。此次没有发布 tag / Release、上传发行二进制或修改生产路由；文档中的快速开始以完成上述服务升级为前提。

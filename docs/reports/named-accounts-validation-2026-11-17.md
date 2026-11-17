# ssmcp 用户名入口与管理员控制：实现与验收记录

日期：2026-11-17（UTC）  
实现提交：[bdda70d](https://github.com/LIghtJUNction/ssmcp/commit/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34)  
开发分支：`feat/username-entry-admin`

## 交付结果

客户端支持直接运行：

```sh
ssmcp yourname@ssmcp.lmm.best
```

`yourname` 是用户在目标服务器选择的名称，可以与 GitHub 登录名不同。客户端检查名字后，按需发起 GitHub 授权；在浏览器确认身份与申请的名称后，服务器完成注册绑定，客户端继续连接。后续连接复用该服务器的本地凭据。独立的 `ssmcp login` 保留为兼容命令，日常连接无须先执行它。

该入口使用 HTTPS/WSS。已有 SSH 连接可通过 `--ssh` 显式选择；相关活动文档、测试夹具和端到端脚本已经同步。

完整操作说明：[用户名入口与服务器管理](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/docs/named-accounts.md)。

## 名称与身份

- 名称长度为 3–32 个 ASCII 字符，统一为小写。
- 首字符为字母，末字符为字母或数字，中间允许字母、数字、下划线和连字符。
- 拒绝系统保留名及内部 `gh-` 前缀；同一服务器内名称唯一。
- 每个 GitHub 账号在每台服务器绑定一个名称。最终占用检查和绑定在同一次存储事务中完成，防止并发抢注覆盖。
- GitHub 数字 ID 继续标识账号归属。旧账号可绑定自定义名称，原有设备、权限和授权仍关联同一个账号。
- 前台连接遇到过期或无效凭据时重新授权；临时服务故障、限流和拒绝访问会保留凭据并显示错误。后台会话报告需要前台授权，不会自动打开浏览器。
- 保存凭据前核对服务器返回的 GitHub 身份和申请的用户名；各服务器的凭据相互隔离。

实现依据：[名称校验](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/username.rs)、[账号存储](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/portal/store.rs)、[客户端自动授权](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/client/account.rs)、[授权确认流程](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/portal/setup.rs)。

## 管理员配置与门户

先使用自己的名称完成一次注册绑定，再通过服务器的 SSH 管理入口编辑现有 `/etc/ssmcpd/ssmcpd.toml`。在已有的 `[portal]` 段中设置：

```toml
[portal]
admin_usernames = ["yourname"]
default_rate_limit_bps = 8388608
```

保留现有门户存储、管理套接字、审计以及 GitHub OAuth 配置。部署本次服务器版本后，重启 `ssmcpd` 使管理员配置生效。服务器启动时将名称解析为已绑定的 GitHub 数字 ID；未注册或被封禁的名称会明确报错。

管理员使用对应 GitHub 账号登录门户，在 `/account/admin` 可以：

| 功能 | 实际行为 |
| --- | --- |
| 查看用户 | 显示自定义用户名、GitHub 身份、封禁状态、在线连接数和限速设置 |
| 查看连接 | 显示连接名称、设备、工作目录、所有者和连接时间 |
| 断开连接 | 关闭选中的当前连接；启用重连的客户端可以重新连接 |
| 封禁用户 | 持久化封禁状态，撤销账号凭据、邀请、设备授权、门户会话及 MCP 授权，关闭该用户的连接并拒绝重连 |
| 解除封禁 | 允许重新授权；此前撤销的凭据不会恢复 |
| 修改限速 | 更新该用户全部连接共用的额度，现有连接立即采用新值 |
| 恢复默认限速 | 移除该用户覆盖值，使用服务器默认值 |

管理员不能封禁当前操作账号。管理写入校验账号身份、管理员权限、来源和 CSRF 令牌；策略修改和凭据发放串行协调，并记录操作主体与目标的审计信息。

实现依据：[管理策略与撤销](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/broker/management.rs)、[门户操作](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/portal/dashboard.rs)、[服务配置](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/server_main.rs)。

## 默认限速与范围

默认每用户上行 **8 MiB/s**、下行 **8 MiB/s**，每方向约 **67.1 Mbit/s**。同一账号的所有连接共享额度，重连不重置独立额度；两个方向分别限制。管理员可以通过门户修改单用户额度。

该默认值给终端操作、工具调用和常见文件传输留出余量，同时为每个账号设置明确的中转带宽上限。服务器配置使用字节/秒，门户使用 MiB/s。配置允许 1–1,073,741,824 字节/秒，初始额度最多允许 64 KiB 突发。

限速统计 ssmcp 中转的应用消息字节，范围不包含用户电脑上其他应用的流量或 TLS/TCP 协议开销。WebSocket 按完整消息转发，大消息会先等待共享额度，因此网络发送仍可能出现消息级突发。

实现依据：[共享带宽预算](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/user_policy.rs)、[HTTP/WSS 接入](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/device.rs)。

## 本地验证结果

验证环境：Linux x86_64，仓库指定的 Rust 工具链。合计 **670 项通过，0 项未解决失败，7 项保持原有忽略标记**。

| 验证目标 | 通过 | 忽略 |
| --- | ---: | ---: |
| 库测试 `src/lib.rs` | 628 | 7 |
| 服务端二进制测试 `ssmcpd` | 9 | 0 |
| 客户端重载 `client_reload` | 6 | 0 |
| 默认服务器与凭据隔离 `client_server_config` | 5 | 0 |
| 客户端启动 `client_startup` | 9 | 0 |
| OAuth 安全回归 `oauth_security` | 11 | 0 |
| 看门狗重连 `watchdog` | 2 | 0 |
| 合计 | **670** | **7** |

覆盖的关键行为包括首次自动授权、旧账号绑定名称、名称占用与保留字拒绝、账号身份不匹配拒绝、过期凭据自动重新授权、后台登录限制、HTTP 身份绑定、真实本地 WebSocket 断开、封禁后拒绝重连、解封不恢复旧令牌、非管理员和自封禁拒绝、管理员表单权限与 CSRF、同一用户跨连接共享限速以及动态限速和封禁唤醒。

核心库和二进制在组合回归中通过。随后发现并修正旧 SSH 测试读取开发机账号、以及显式配置路径缺少空配置文件的问题；修正后，以下五组集成测试统一重新执行并全部通过：

```sh
cargo test --no-fail-fast \
  --test client_reload \
  --test client_server_config \
  --test client_startup \
  --test oauth_security \
  --test watchdog \
  --locked --offline -j 1 -- --test-threads=2
```

`cargo fmt --all -- --check`、`git diff --check` 和 7 个修改后的 Python 端到端脚本的语法检查通过。验证采用独立测试目录、测试账号、可控授权服务和本地 HTTP/WSS。未执行完整 Python 端到端套件或 macOS/Windows 平台回归。

关键测试：[客户端账号流程](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/client/account/named_tests.rs)、[实时 HTTP/WSS 管理测试](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/device.rs)、[门户表单测试](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/portal/dashboard_tests.rs)、[限速测试](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/src/user_policy.rs)。

## 界面检查

使用实际 Rust 门户视图和合成账号、连接数据渲染，通过独立 Chromium 检查桌面与手机布局。表单可见且有效，手机布局无水平溢出；截图中的账号与连接均为测试数据。

- [桌面截图：1440 × 1755](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/docs/assets/named-accounts-admin-desktop.png)
- [手机截图：390 × 3218](https://github.com/LIghtJUNction/ssmcp/blob/bdda70d5efeee7375cfaa871a30a1b2a00fd8e34/docs/assets/named-accounts-admin-mobile.png)

## 提交与部署状态

实现、操作说明、截图及兼容性修正已分阶段提交并推送，保留并合并了同期主分支的默认服务器、公钥信任、会话协作和项目介绍变更。

本次完成源码与本地验收交付。线上二进制更新、服务重启及生产 OAuth 回调验收尚未执行；新入口需要部署本次客户端和服务器版本后使用。

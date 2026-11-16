# GitHub 会话授权：生产环境补充验收

**验收结果：通过。** 2026-11-16 18:20 UTC（UTC+8 为 2026-11-17 02:20）完成公网登录、MCP 工具调用、会话隔离和刷新令牌检查。16 项检查全部通过，两个临时测试会话均已关闭。[机器可读结果](../reports/oauth-production-verification-20261116.json)

本页补齐 0.4.0 上线时中断的公网验收。0.4.0 已于 2026-11-15 22:00 UTC 完成服务切换；之后的项目更新已将生产服务升级到 0.4.1。复核时 GitHub 主分支为 0.6.1，仓库版本、系统安装版本和正在运行的程序分别核对，不将它们混为同一版本。[初次部署记录](../reports/deploy-0.4.0-20261116.json) · [复核时主分支](https://github.com/LIghtJUNction/ssmcp/commit/eeb7588cbbb0ad49d2cf0d05d90a0f3de8667c70)

## 运行状态

| 项目 | 实际核对结果 |
| --- | --- |
| 生产服务 | `ssmcpd 0.4.1`，systemd 状态为 active |
| 服务器与开发机的系统安装客户端 | `ssmcp 0.4.1` |
| 公网地址 | `https://ssmcp.lmm.best`，健康检查返回 `ok` |
| GitHub 模式 MCP 入口 | `https://ssmcp.lmm.best/mcp` |
| GitHub OAuth 回调 | `https://ssmcp.lmm.best/oauth/callback/github` |
| 服务配置 | `/etc/ssmcpd/ssmcpd.toml`，所有者 `ssmcpd`，权限 `0600` |
| OAuth App 配置 | Client ID 和 Client Secret 均已配置；报告仅保存是否存在的结果 |
| 客户端登记文件 | `/var/lib/ssmcpd/oauth-clients.toml`，文件存在，权限 `0600` |
| 本轮验收范围 | 创建两个临时会话并运行协议检查；服务 PID 在检查前后相同 |

纯文本 TOML 保留可读、可编辑的配置方式。实际密钥由服务账户读取，仓库提供占位示例。完整字段、配置优先级及首次部署步骤见[配置教程](configuration.md)和[部署教程](deployment.md)。

## 授权关系与启动方式

本机使用 SSH 公钥证明“这台客户端可以连接服务器上的哪个账号”。启动参数 `--allow` 声明“谁可以控制这个会话”。浏览器中的 GitHub OAuth 证明控制者身份，服务器将其与会话允许列表比较，再决定是否显示、读取或调用这个会话的工具。[GitHub 授权实现](https://github.com/LIghtJUNction/ssmcp/commit/95fa57967c4622705a2aba761ea4bed73663b451)

已有 SSH 配置可以继续复用。以下命令使用已配置的 `ssmcp.lmm.best` 目标：

```sh
cd /path/to/project
ssmcp --name my-project --allow lightjunction@github ssmcp.lmm.best
```

允许多个用户时，可重复传入 `--allow` 或使用逗号分隔。用户名在连接时解析为 GitHub 数字 ID，当前连接按 ID 校验；重启客户端会重新解析用户名，账号改名后应更新参数。

系统安装的 0.4.1 若需要启动 Codex 后端，可以增加 `-t codex`。0.6 系列已统一客户端能力并移除 `-t`，使用对应版本的[命令说明](commands.md)。这一区别只影响客户端启动方式，GitHub 会话允许列表仍由服务器逐次检查。

随后在 MCP 客户端配置 `https://ssmcp.lmm.best/mcp`，进入授权页面，选择 GitHub 登录，再确认允许当前 MCP 客户端。终端客户端无需另行完成 GitHub 登录。

## 公网检查结果

| 检查 | 结果 |
| --- | --- |
| SSH 接入带 `lightjunction@github` 的临时会话 | 返回 GitHub 模式、`/mcp` 地址和允许列表，未返回登录码 |
| OAuth 发现及动态客户端登记 | 返回正确端点，登记默认支持 `refresh_token` |
| 登录码授权与 PKCE | 浏览器事务、Cookie、Origin、授权码及令牌交换通过 |
| MCP 初始化与工具发现 | 初始化通过，顶层工具为 `list` 和 `code_mode` |
| 会话权限隔离 | 登录码授权的列表不包含 GitHub 受限会话；直接指定受限 ID 读取 schema 被拒绝 |
| 实际原生调用 | 通过 `code_mode` 调用测试会话的 `terminal.status` 成功 |
| RT 轮换与新令牌使用 | 刷新后 AT、RT 均变化；新 AT 可以继续调用 MCP |
| 连接结束与撤销 | 测试登录码连接关闭后，两枚 AT 均返回 401，新 RT 返回 `invalid_grant` |
| 清理 | 两个测试会话均已关闭 |

登录时及刷新后的 AT 均实际返回 `expires_in=28800`。服务器 TOML 配置的期限如下：

| 设置 | 配置值 | 含义 |
| --- | --- | --- |
| `access_token_ttl` | 28,800 秒 / 8 小时 | 单枚 AT 的最长期限 |
| `refresh_token_idle_ttl` | 2,592,000 秒 / 30 天 | 成功刷新会重新计算闲置期限 |
| `refresh_token_max_ttl` | 7,776,000 秒 / 90 天 | 授权绝对上限，刷新不延长 |
| `client_registration_ttl` | 31,536,000 秒 / 365 天 | 动态客户端登记期限 |

RT 的作用是让客户端在 AT 到期后取得新令牌。MCP 客户端需要主动发起刷新、保存轮换后的 RT，再重试请求；令牌不会自行发起网络请求。

## 初次验收中断的原因

启用 GitHub 后，传统 `/SSH用户名` 授权页面同时显示 GitHub 登录表单和登录码表单。两个表单属于同一授权事务，因此包含相同的隐藏 `transaction_id`。原验收脚本要求页面只能出现一个该字段，误将合法页面判为失败。

本次将检查改为“提取的事务 ID 非空，且去重后仅有一个值”，再提交该值。服务器业务代码无需为此修改。修正后，原先尚未执行的登录、隔离、工具调用和刷新检查全部通过。原部署 JSON 保留当时的 `deployed_with_smoke_failure` 记录；本页及新 JSON 记录补验结果，避免覆盖历史证据。

## 验证边界

这次补验使用真实生产 HTTPS、SSH、服务器配置和临时会话，但没有重新驱动用户在 GitHub 网页上的登录与同意过程。GitHub 数字身份解析和受限会话握手已在生产环境检查；完整浏览器协议链路另有受控 GitHub 响应的自动化回归。[0.4.0 自动化验收](validation.md)

服务器端 RT 轮换及新 AT 调用已在生产通过。此次没有观察 ChatGPT 自身何时自动发起刷新，因此不将结果表述为“已实测 ChatGPT 无感续期”。

0.4.1 起可以持久化动态客户端登记；当前服务已配置 StateDirectory，登记文件存在。AT、RT、授权码及未完成的登录事务仍在内存中，守护进程重启后需要重新授权。本轮未重启服务，登记持久化的跨重启行为以对应实现与专项测试为依据。[登记持久化实现](https://github.com/LIghtJUNction/ssmcp/blob/eeb7588cbbb0ad49d2cf0d05d90a0f3de8667c70/src/oauth/registry.rs)

GitHub 身份授权不绑定单条 SSH 会话；本机会话离线后可以继续刷新，匹配允许列表的新会话上线后可继续发现。登录码授权仍绑定签发它的 SSH 连接，连接退出就撤销授权。两种授权的生命周期不同，这是有意保留的行为。[离线与重连回归](https://github.com/LIghtJUNction/ssmcp/blob/95fa57967c4622705a2aba761ea4bed73663b451/src/oauth_github_tests.rs)

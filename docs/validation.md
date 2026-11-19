# ssmcp 0.4.0 GitHub 登录与会话权限验收

本页保留 0.4.0 初版的源码与自动化验收记录。生产服务后续已更新至 0.4.1，2026-11-16 的公网登录、权限隔离与 RT 轮换补验全部通过；当前版本、登记持久化和验证边界见[生产环境补充验收](oauth-production-20261116.md)。

0.4.0 增加每个会话的 `--allow 用户名@github` 策略。本机仍通过 SSH 公钥连接，GitHub 登录发生在浏览器中的 MCP 授权流程；服务器验证 GitHub 身份后，还要求用户明确同意当前 MCP 客户端。发现、读取工具 schema 和实际调用均按目标会话的允许列表检查。[实现提交](https://github.com/LIghtJUNction/ssmcp/commit/95fa57967c4622705a2aba761ea4bed73663b451) · [配置教程](configuration.md#github-allow)

## 版本与测试依据

验证日期为 **2026-11-15 UTC / 2026-11-16 UTC+8**。源码提交 `95fa57967c4622705a2aba761ea4bed73663b451` 的完整 CI 已在 Linux x86_64 与 arm64 通过。[CI 43763608570](https://github.com/LIghtJUNction/ssmcp/actions/runs/43763608570)

| 检查 | [x86_64](https://github.com/LIghtJUNction/ssmcp/actions/runs/43763608570/job/132963654712) | [arm64](https://github.com/LIghtJUNction/ssmcp/actions/runs/43763608570/job/132963655051) |
| --- | --- | --- |
| Rustfmt、Clippy 零警告 | 通过 | 通过 |
| 默认 Rust 测试 | 128 通过，8 显式忽略 | 128 通过，8 显式忽略 |
| 系统 Node.js 与 bubblewrap 集合 | 8 通过 | 8 通过 |
| 两工具 MCP HTTP 集成集合 | 3 通过 | 3 通过 |
| systemd 服务限制下的运行时探针 | 通过 | 通过 |
| 真实 SSH、OAuth、HTTP、PTY | 134 项通过 | 134 项通过 |
| 五种后端的完整 SSH/OAuth 链路 | 189 项通过 | 189 项通过 |
| Release 构建与暂存安装 | 通过 | 通过 |

部分集合重复执行默认测试，表格各行不应相加。8 个默认忽略项中，4 个依赖系统隔离的测试由 CI 专用步骤执行；其余 4 个需要独立原生 Agent CLI 的探针不属于这次默认执行。五后端测试使用确定性协议 fixture，不发起模型推理。[CI 定义](../.github/workflows/ci.yml)

编辑环境通过全目标编译、Clippy、默认测试以及新增身份提供方的 5 项测试和完整 GitHub HTTP 流程的 3 项测试。HTTP 测试使用真实回环 TCP、Unix socket 和受控 GitHub 响应，覆盖完整浏览器到 MCP 的协议链路；它们没有使用真实用户账号登录 GitHub。[提供方测试](../src/identity.rs) · [完整 HTTP 测试](../src/oauth_github_tests.rs)

## 权限与登录行为

| 已验证行为 | 对应证据 |
| --- | --- |
| 重复及逗号分隔的 `--allow` 参数规范化、去重，最多 32 个用户 | [客户端与 wire 校验](../src/client.rs) · [wire 测试](../src/wire.rs) |
| 旧客户端兼容 wire v4；新受限客户端遇到旧服务器会停止，不会退回登录码模式 | [握手回归](../tests/client_startup.rs) |
| GitHub 用户名解析为稳定数字 ID；同名不同 ID、其他提供方均不能冒用 | [Broker 权限测试](../src/broker.rs) |
| 用户只看到明确允许自己的会话；猜测被拒会话 ID 不能读取工具表或到达后端 | [完整 HTTP 权限隔离](../src/oauth_github_tests.rs) |
| 登录码授权不能访问设置了 `--allow` 的会话 | [Broker](../src/broker.rs) · [HTTP 回归](../src/oauth_github_tests.rs) |
| provider state 绑定浏览器 cookie、提供方与原始事务，成功消费后不能重放 | [OAuth 状态测试](../src/oauth.rs) |
| 上游 GitHub PKCE 与下游 MCP PKCE 独立，GitHub token 不能作为 MCP Bearer | [完整 HTTP 回归](../src/oauth_github_tests.rs) |
| GitHub 身份核验成功后仍需确认；取消不会签发 MCP 授权码 | [确认与取消回归](../src/oauth_github_tests.rs) |
| 已验证账号不能被旧页面后的另一次登录替换；最后一次重试成功后仍能确认 | [OAuth 事务回归](../src/oauth.rs) |
| 代码执行中的每次原生工具调用重新验证令牌、资源、目标会话与允许列表 | [HTTP 转发边界](../src/http.rs) · [既有运行中撤销测试](../tests/two_tools.rs) |
| TOML 拒绝未知字段与错误类型；含密钥配置要求私有权限；错误不回显密钥行 | [守护进程配置测试](../src/bin/ssmcpd.rs) |

GitHub 请求固定使用受信任的 HTTPS 端点，禁用重定向，限制响应体与超时。公开用户名查询使用服务器的 OAuth App 认证；用户登录不请求仓库、邮箱或 `offline_access` scope。GitHub token 仅用于当前身份核验，MCP 使用 ssmcp 自己的 AT/RT。[提供方实现](../src/identity.rs) · [GitHub OAuth 文档](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps) · [GitHub API 配额说明](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)

## 刷新与重连

默认期限仍为 AT 最长 8 小时、RT 闲置 30 天、授权绝对上限 90 天、客户端登记最长 365 天。成功刷新会轮换 RT 并延长闲置期限，不能延长绝对上限。原有 AT 到期后通过 RT 换新令牌再成功请求 MCP 的 HTTP 回归继续通过。[续期实现](../src/oauth.rs) · [到期 HTTP 回归](../tests/oauth_security.rs)

新增测试还验证了 GitHub 授权在本机会话离线时继续刷新：列表暂时为空，重新接入明确允许该身份的会话后，原授权的新 AT 可以发现它，无需第二次 GitHub 交换。登录码授权仍绑定其签发 SSH 连接。[离线刷新与重连测试](../src/oauth_github_tests.rs)

刷新请求需要由 MCP 客户端发起并保存新 RT；这些结果证明服务器协议链路，不代表已经实测所有第三方客户端的自动续期。早期验证版本的 OAuth 状态只在内存中。当前配置 `oauth.client_registry` 后，账户授权哈希、原到期时间与重放撤销状态可跨重启恢复；后续验证见[授权持久化](oauth-durability.md)。用户名到 ID 的绑定只对当前连接持续有效，重新启动会重新解析用户名；账号改名后应更新启动参数。

## 配置、教程与部署边界

服务器支持纯文本 TOML：`[server]`、`[auth.github]` 和 `[oauth]`。配置顺序为命令行、环境变量、TOML、默认值；回调从公网 origin 自动生成 `/oauth/callback/github`。实际密钥只保存在服务用户拥有的私有配置文件，仓库中只有占位示例。[示例](../packaging/ssmcpd.toml.example) · [部署教程](deployment.md#github-oauth)

README、配置、公钥、登录、后端和 man page 已同步。两张 SVG 更新为 SSH 连接、GitHub 身份和会话允许列表的关系，保留 CSS 动画与减少动画回退；两张图均经 700 像素宽渲染检查，无裁切或重叠。既有 README 截图仅作为历史资产，未标注为新登录流程截图。

本记录先记录源码与自动化验收；生产切换和实际运行版本另以部署记录核对。GitHub 仓库更新本身不会替换正在运行的服务，升级时需要保持 SSH 强制桥接、systemd 二进制和 runner 安装路径一致。

历史记录：[0.3.2 续期与期限](validation-0.3.2.md) · [0.3.1 SSH 配置与 README](validation-0.3.1.md)。

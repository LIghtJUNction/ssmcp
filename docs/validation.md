# ssmcp 0.3.2 续期与有效期验收报告

0.3.2 延长默认有效期，并补齐正常访问令牌过期后的续期链路：省略 `grant_types` 的新注册默认提供 refresh token；刷新成功会滚动延长闲置期限；失效 Bearer 请求返回标准 `invalid_token` 提示。真实 HTTP 回归已经验证「AT 到期 → RT 刷新 → 保存新令牌 → MCP 重试成功」，无需第二次输入登录码。[实现](../src/oauth.rs) · [HTTP 回归](../tests/oauth_security.rs) · [配置教程](configuration.md#silent-refresh)

这项修复适用于签发授权的 SSH 连接仍在线的情况。连接结束仍会撤销相关授权，服务重启仍会清空内存状态；本轮没有改变这两项生命周期规则，也没有把服务器端测试描述为 ChatGPT 客户端的实测结果。

## 版本与验证依据

验证时间：**2026-11-15 UTC**。源码提交 [`8ffe89e`](https://github.com/LIghtJUNction/ssmcp/commit/8ffe89e828b29af11f98529239cbc6cb62ddb52b) 的完整 CI 已通过，包含 Linux x86_64 和 arm64。报告及教程在后续文档提交补充。[CI 运行 43757927791](https://github.com/LIghtJUNction/ssmcp/actions/runs/43757927791)

| 检查 | [x86_64](https://github.com/LIghtJUNction/ssmcp/actions/runs/43757927791/job/132946009924) | [arm64](https://github.com/LIghtJUNction/ssmcp/actions/runs/43757927791/job/132946010130) |
| --- | --- | --- |
| 格式检查、Clippy 零警告 | 通过 | 通过 |
| 默认 Rust 测试 | 105 通过，8 显式忽略 | 105 通过，8 显式忽略 |
| Node.js 与 bubblewrap 运行时集合 | 8 通过 | 8 通过 |
| 两工具 MCP HTTP 集成集合 | 3 通过 | 3 通过 |
| systemd 服务限制下的运行时探针 | 通过 | 通过 |
| 真实 SSH、OAuth、HTTP、PTY 与多客户端 | 134 项通过 | 134 项通过 |
| 五种后端完整 SSH/OAuth 链路 | 189 项通过 | 189 项通过 |
| Release 构建与暂存安装 | 通过 | 通过 |

默认测试中有 4 个需要独立原生 Agent CLI 的探针未执行；另外 4 个依赖系统隔离的测试由 CI 专用步骤执行并通过。部分集合重复执行默认测试，表格各行不应相加。五后端端到端测试使用确定性协议 fixture，不发起模型推理。[CI 定义](../.github/workflows/ci.yml)

编辑环境也完成格式检查、Clippy、默认 Rust 测试，以及 `ssmcp` 和 `ssmcpd` 0.3.2 二进制构建与版本检查。另用实际命令行验证：无效有效配置会报错，CLI 可覆盖同名的无效环境变量，`serve --help` 不受无效有效期环境变量阻塞。

## 默认值与配置

| 对象 | 0.3.1 | 0.3.2 默认值 |
| --- | --- | --- |
| 一次性登录码 | 10 分钟 | 30 分钟 |
| 授权表单及 CSRF cookie | 10 分钟 | 30 分钟 |
| OAuth authorization code | 60 秒 | 60 秒 |
| access token | 1 小时 | 8 小时 |
| refresh token | 首次授权后固定 24 小时 | 闲置 30 天，成功刷新后重新计时 |
| 同一授权的绝对上限 | 24 小时 | 90 天 |
| 客户端登记及 client secret | 7 天 | 365 天 |

四项令牌与登记期限可以通过 `ssmcpd serve` 参数或环境变量设置，优先级为 CLI、环境变量、默认值。配置只在启动时读取；有效值必须是正整数秒，并满足 `access ≤ refresh idle ≤ refresh max ≤ registration ≤ 315360000`。令牌寿命还会被当前授权和客户端登记的实际剩余时间截断，客户端应读取响应中的 `expires_in`。[参数及环境变量](deployment.md#oauth-lifetimes)

```ini
SSMCP_ACCESS_TOKEN_TTL=28800
SSMCP_REFRESH_TOKEN_IDLE_TTL=2592000
SSMCP_REFRESH_TOKEN_MAX_TTL=7776000
SSMCP_CLIENT_REGISTRATION_TTL=31536000
```

## 已验证的续期行为

| 行为 | 证据 |
| --- | --- |
| 注册省略 `grant_types` 时返回两种实际登记类型，并签发 RT | `omitted_registration_grants_refresh_expired_access_without_a_new_login` |
| 显式只登记 `authorization_code` 时不签发 RT | `explicit_authorization_code_registration_opts_out_of_refresh_tokens` |
| AT 真实到期后返回 `401` 和 `error="invalid_token"`，随后 RT 可换取有效的新 AT、RT | 1 秒有效期的完整 HTTP 流程；没有第二次注册、授权表单、登录码提交或 SSH 重连 |
| 第 24 小时以后可继续刷新；成功刷新延长闲置期限，但不能超过绝对上限 | `refresh_slides_idle_expiry_and_caps_access_at_absolute_deadline` 使用受控时间验证 30 天与 90 天边界 |
| 超过闲置期限即失效，不会仅凭绝对期限尚未到达而继续刷新 | `refresh_inactivity_expires_before_absolute_limit` |
| 滑动续期后仍保留旧 RT 与已兑换授权码的重放记录；重放会撤销活动授权族 | `replay_markers_survive_original_idle_deadline_after_refresh` |
| 错误客户端或资源不会借重放撤销其他授权；并发兑换不能制造有效分支 | 既有 HTTP 客户端认证、绑定及并发回归持续通过 |
| SSH 断开撤销仍有效，同用户名重连不能继承旧授权 | 既有 Broker、OAuth 与真实 SSH 测试持续通过 |
| 非法时长被拒绝，CLI 覆盖环境变量，未配置项使用默认值 | OAuth 配置测试、守护进程参数测试及实际 CLI 检查 |

对应源码：[OAuth 单元测试](../src/oauth.rs)、[HTTP 单元测试](../src/http.rs)、[HTTP 集成测试](../tests/oauth_security.rs)、[守护进程参数](../src/bin/ssmcpd.rs)、[真实 SSH 脚本](../scripts/e2e.py)。

## 协议依据与客户端职责

刷新不是服务器主动推送令牌。持有 RT 的 MCP 客户端需要向 token endpoint 发起 `grant_type=refresh_token` 请求，使用原 `resource` 和登记时约定的客户端认证，保存返回的新令牌，再重试请求。RT 每次轮换，客户端应替换旧值，并串行处理同一授权的刷新。[RFC 6749 第 6 节](https://www.rfc-editor.org/rfc/rfc6749#section-6) · [客户端操作教程](configuration.md#silent-refresh)

RFC 7591 的基础缺省授权类型为 `authorization_code`，同时允许服务器配置或替换注册元数据，并要求返回实际登记值。0.3.2 在省略字段时默认登记刷新能力，是 ssmcp 的服务策略；显式仅请求授权码仍保留原选择。[RFC 7591 第 2、3.2.1 节](https://www.rfc-editor.org/rfc/rfc7591)

RFC 9700 建议按 RT 闲置时间设置到期；RFC 6750 为过期、撤销或其他无效 Bearer 定义了 `invalid_token`。本轮实现使用滑动闲置期限与更长的绝对上限，同时维持轮换及重放检测。[RFC 9700 第 4.14.2 节](https://www.rfc-editor.org/rfc/rfc9700#section-4.14.2) · [RFC 6750 第 3.1 节](https://www.rfc-editor.org/rfc/rfc6750#section-3.1)

## 教程、视觉与升级范围

README、配置教程、部署说明、后端文档和两个 man 页已同步新默认值。登录 SVG 更新到 30 分钟说明，XML 与 700 像素宽预览检查通过，保留减少动画偏好的静态回退。此前 0.3.1 的 README 浏览器截图保留作历史记录，未将它标为 0.3.2 的实测截图。

0.3.2 仍使用内部 wire v4。要让新策略作用于实际连接，需要安装新 `ssmcpd` 并重启网关，再让 MCP 客户端完成一次新的注册与登录。Git 同步不替换已安装程序，重启又会撤销现存连接；升级方法和恢复流程已写入[部署教程](deployment.md#upgrade-032)。本轮没有替换或重启正在运行的生产网关。

这次验证覆盖服务器的续期响应和自动化测试客户端的完整使用流程，未证明所有第三方 MCP 应用都正确持久化、轮换和刷新 RT。SSH 断开与服务重启后的授权持久化也未在本轮实现。旧错误 `invalid_grant` 可以来自到期、断线、撤销或重放，不能仅凭该错误断定实际原因。

此前的 SSH 配置复用、公钥教程和 README 美化验收见 [0.3.1 报告](validation-0.3.1.md)。

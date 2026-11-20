# 门户控制台验收记录（2026-11-17）

本次修改包含极简暗色门户、个人会话和历史、管理员统计与拓扑、服务器配置的管理员名单，以及自动清理的安全审计。以下记录代码和界面验收；生产部署结果独立写入 `docs/reports/portal-console-20261117/deployment.json`，以该报告的实际状态为准。

## 自动检查

| 命令 / 范围 | 结果 | 主要检查 |
| --- | --- | --- |
| `cargo test --lib portal::` | 21 通过 | 真实 HTTP 身份隔离、管理员校验、CSRF / Origin、审计失败不变更状态、配对登记审计、限速、既有账户和公钥行为 |
| `cargo test --lib management` | 11 通过 | 私有 socket、管理员与所有权、连接断开、工具请求审计、调用者归属分离 |
| `cargo test --lib history::` | 11 通过 | 短期保留、数量与大小限制、分页和账号过滤、持久化、损坏与链接检查、Pending 证据 |
| `cargo test --lib broker::` | 19 通过 | 连接和授权、工具派发、关闭与审计顺序 |
| `cargo test --lib http::` | 15 通过 | HTTP / OAuth / MCP 路由回归 |
| `cargo test --lib bridge::` | 4 通过 | 调用转发和错误行为回归 |
| `cargo test --bin ssmcpd` | 9 通过 | 管理员 ID、配置权限、归属冲突、保留期、`--portal-only` |
| `cargo clippy --lib --bin ssmcpd -- -D warnings` | 通过 | 静态检查 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 | 格式与补丁检查 |

不同过滤条件可能覆盖同一测试，表中数量不能直接相加。验证没有调用外部模型，也没有使用生产账号数据库作为测试夹具。

## 关键权限和审计行为

- 浏览器提供的 `subject` 或管理员标记不能改变身份；身份由登录会话和账户存储重建。
- 普通用户只能管理归属于自己 SSH 账户的连接。`--allow` 的工具访问权不能升级为会话管理权。
- 管理员查询和断开在网关重新校验。注册用户摘要不含公钥正文或备注。
- 登录、退出、公钥变更、CLI 配对登记和工具派发先写持久请求记录，审计失败则不先执行。完成记录失败会明确提示可能已执行，避免误导用户直接重试。
- 共享会话的实际 OAuth 调用者和 SSH 会话所有者分别记录。
- 第 31 次匿名门户失败请求、单账号第 121 次管理读取返回 429，拒绝不再递归追加审计。
- 自然断线和关闭清理不因审计故障而挂起；关闭流程给断开事件留出有界写入时间。

## 界面检查

使用 Chromium 渲染实际 Rust 视图生成的页面，检查 1440px 桌面和 390px 手机视口。已检查匿名首页、已登录且无公钥、已登记公钥、门户未启用、错误页、个人会话、历史记录和管理员控制台。检查范围包含标题层级、原生展开控件、完整会话 ID、长公钥与路径换行、POST 表单及 CSRF 隐藏字段。

实际点击删除公钥的展开确认：提示正确说明 SSH 同步生效前的延迟，以及已建立连接不会被删除公钥立即关闭。手机页面没有横向溢出。生产页面不依赖前端脚本、外部字体或示例统计数据。

以下管理员、会话和历史截图使用明确标注的演示数据，仅用于展示布局，不能用来推断服务器的实际用户或连接。

### 首页（桌面）

[门户首页](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/portal-console-20261117/home-desktop.jpg`）

### 管理员（桌面，演示）

[管理员控制台演示](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/portal-console-20261117/admin-desktop-demo.jpg`）

### 个人会话与历史（手机，演示）

[个人会话演示](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/portal-console-20261117/sessions-mobile-demo.jpg`）

[历史记录演示](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/assets/portal-console-20261117/history-mobile-demo.jpg`）

## 部署边界

配置与操作说明见 [账号门户、会话管理与安全审计](portal-console.md)。双进程部署需要同一个服务 UID、私有管理 socket 和门户的 `--portal-only` 参数。后台每分钟清理过期审计；读取、写入和启动也执行清理。默认 7 天、20,000 条、正式文件 16 MiB，达到数量或大小上限会提前清理最旧记录。

历史从启用审计开始。网关升级会重启连接，并使内存中的网页登录和 OAuth 授权失效；实际 GitHub 登录需要用户本人完成。测试通过说明已验证的边界符合预期，不能替代持续运维与后续漏洞修复。

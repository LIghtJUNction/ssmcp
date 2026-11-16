# 首页与账户门户验收（2026-11-16）

[ssmcp.lmm.best](https://ssmcp.lmm.best/) 已上线首页、GitHub 登录入口、个人 SSH 公钥管理与 ChatGPT 插件配置教程。门户使用独立进程，原 MCP 网关的在线连接和授权在部署过程中保持有效。

## 使用流程

1. 打开首页，点击“使用 GitHub 登录”。登录后显示 GitHub 用户名，并分配固定的 SSH 用户名 `gh-<GitHub 账号编号>`。
2. 添加本机的单行 Ed25519 公钥，可填写备注、查看指纹或删除。账户归属使用 GitHub 数字 ID；GitHub 改名不改变 SSH 用户名或公钥归属。
3. 在本机项目目录运行首页生成的连接命令。`--allow 用户名@github` 指定获准操作该客户端的身份；使用 Codex 时加 `-t codex`。
4. 打开 [ChatGPT Plugins](https://chatgpt.com/plugins)，点击 Add；Name 填 `ssmcp`，Connection 填 `https://ssmcp.lmm.best/mcp`，勾选同意后点击 Create，再完成插件授权。

首页登录与 MCP 插件授权分别处理。公钥同步计时器每 15 秒检查变化；删除公钥阻止新的 SSH 认证，已有连接需在本机退出。只上传 `.pub` 公钥。

## 线上验证

| 检查 | 结果 |
| --- | --- |
| HTTPS 首页及样式 | 首页返回 200，实际浏览器正常显示四步教程，桌面全页视觉检查通过 |
| 浏览器 GitHub 登录入口 | 点击按钮后进入 GitHub 官方登录页，页面显示“to continue to ssmcp”，回调地址指向本服务 |
| 浏览器同源表单 | `Referrer-Policy: same-origin` 保留同源 Origin，严格 Origin 与 CSRF 校验继续启用 |
| 受限 SSH 账户 | 通过生产同步器创建一次性测试账户，使用临时 Ed25519 公钥连接公网 SSH 成功 |
| MCP 接入 | 现有 GitHub 身份授权能通过 `list` 发现测试客户端，并通过 `code_mode` 调用 `terminal.status`，返回运行状态 |
| 公钥撤销 | 撤销已发布公钥后，新建 SSH 连接被 public-key authentication 拒绝 |
| 测试清理 | 临时客户端退出，临时账户、公钥授权文件、所有权登记项和本机测试密钥已删除 |
| 服务与代理 | 门户服务及同步计时器 active，Nginx 与 OpenSSH 配置检查通过，MCP 网关主进程未重启 |

## 本地验证

- 本轮部署源码的回归合计 **139 passed、7 ignored、8 filtered**。过滤项为既有 `code_mode::tests`；门户登录、公钥管理及 OAuth 安全测试均实际执行。
- 门户 **8 项测试通过**：完整 HTTP mock GitHub 登录与回调、公钥添加与删除、账号隔离、CSRF/Origin、会话轮换、回调重放、匿名会话容量、持久存储与 HTML 转义。
- SSH 同步器 **5 项 Python 测试通过**：账号与密钥校验、已有账号拒绝接管、受限配置核验、密钥写入与撤销、不同安装前缀与配置目录。
- `cargo fmt --all -- --check`、对应 Rust clippy 检查和 release 构建通过；Arch 安装包已解包核验二进制、脚本、systemd units 和安装路径。
- 合并并行网关更新后，提交 `273708129c77f0673b94bebaa08462dc171cf282` 的门户 **8/8** 与服务配置 **5/5** 再次通过，独立门户配置和安全头保留。

## 部署范围

本次线上门户制品为 `ssmcp-git 0.4.0.r39.g5e2fd33-1`，源码固定在 `5e2fd333e380c4a6af647892cd17131016a6e45e`。门户复用原 GitHub OAuth App 与私有 TOML 配置；没有创建新 OAuth App、复制 provider secret 或重启在线 MCP 网关。

独立门户监听 loopback `127.0.0.1:8086`。代理将首页、样式、`/account/` 和 `portal_` 命名空间的 GitHub 回调转发给门户，MCP 与其他 OAuth 路径继续进入原网关。SSH 配置仅对 `ssmcp-portal` 专用组允许 `gh-*` 账户，实际强制命令和禁用转发策略已验证。

包内服务端 SHA-256：`e2ac1a03ecdc6f745278e61b0758720399d3b3d25dd8eb836f1392378c0c665e`。服务器上的安装结果与该制品一致。恢复材料保存在开发机的服务器备份目录，生产服务器不保留本次临时安装包或构建目录。

## 验证边界

真实浏览器验证到 GitHub 官方登录页。本轮没有在浏览器代登录个人 GitHub 账号，因此真实个人账号最终授权回跳不列为已完成的线上检查；回调协议、用户名绑定和公钥读写由完整 HTTP provider fixture 验证。

开发机已有 code_mode 隔离测试受到当前用户线程数量和 namespace 资源限制，本轮未将它们列入通过结果。公钥到 SSH、在线客户端发现和原生工具调用已在真实公共服务路径验证。

README 与教程采用 [Research-Paper-Writing-Skills](https://github.com/Master-cai/Research-Paper-Writing-Skills) 的段落组织和声明—证据对应原则，保留具体操作步骤、适用范围和可复核结果。

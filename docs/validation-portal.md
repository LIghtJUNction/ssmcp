# 账户门户本地验收（2026-11-16）

本次只修改、验证并同步源码，未部署生产服务器，未创建或修改 GitHub OAuth App。门户直接复用已配置的 GitHub provider、PKCE exchange 和 `/oauth/callback/github`；使用独立 state 命名空间，不自动签发 MCP 授权。

## 已验证

- 7 项门户 Rust 测试通过：numeric ID 与改名稳定性、持久存储、进程锁、权限与符号链接拒绝、公钥解析、CSRF、cookie 轮换、回调重放、完整 HTTP mock GitHub 登录、公钥添加与跨账户操作拒绝、HTML 转义和教程内容。
- 4 项 Python 同步器测试通过：公钥与 ID 校验、同名账户拒绝接管、专用文件写入与撤销、UID 变更拒绝、缺少 SSH 强制限制时拒绝发布公钥。测试使用假账户数据库，不创建真实 OS 用户。
- 过滤既有 `code_mode::tests` 后的 Rust 回归：135 passed、0 failed、7 ignored、8 filtered；包括 OAuth 安全测试和五类后端的默认测试。
- `cargo fmt --all -- --check`、`cargo clippy --locked --all-targets --all-features -- -D warnings`、`cargo build --locked`、`cargo doc --locked --no-deps`、`git diff --check` 通过。
- Chromium headless 从本机运行的门户实际渲染桌面与手机页面，检查留白、表单、命令换行和逐步教程。
- 两个新增 systemd unit 的语法检查通过。完整服务文件检查因本机未安装 `/usr/local/bin/ssmcpd` 报缺少可执行文件；未为此安装或修改本机系统服务。

## 环境限制与未验证范围

完整 `cargo test --locked` 已尝试。既有代码运行器将 `RLIMIT_NPROC` 设为 128，而本机当前用户已有超过 2000 个线程；Node 子进程创建工作线程失败，部分既有 code_mode 测试失败，其取消测试等待未开始的 host 调用而无法结束。终止了本次挂住的测试进程，再运行上述过滤回归；没有放宽产品隔离或修改宿主系统限制。

真实 OpenSSH 本机回归先发现新版 `sshd -T` 输出键名改为大小写混合，修正测试比较为不区分大小写后，认证、登录码、OAuth、MCP 发现等前置检查通过。进入 code_mode 时 bubblewrap 报 `Creating new namespace failed: Resource temporarily unavailable`，该回归未整体通过；五后端完整隔离链路验收也未宣称完成。

GitHub 登录采用 HTTP loopback provider fixture 验证协议，不使用真实用户令牌。真实 GitHub 浏览器回跳、生产 TLS/代理、ChatGPT 插件最终授权，以及 root 同步器在目标发行版创建账户后的真实 SSH/PAM 接入，均需后续明确授权部署后验证。

## 复现

使用独立构建目录避免共享 target 中其他构建的产物干扰：

```sh
CARGO_TARGET_DIR=.local/portal-target cargo test --locked --lib portal::tests
python3 scripts/test_portal_ssh.py
CARGO_TARGET_DIR=.local/portal-target cargo test --locked -- --skip code_mode::tests
CARGO_TARGET_DIR=.local/portal-target cargo clippy --locked --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR=.local/portal-target cargo build --locked
```

README 修改采用 [Research-Paper-Writing-Skills](https://github.com/Master-cai/Research-Paper-Writing-Skills) 的核心说明（已成功读取 README 与 `research-paper-writing/SKILL.md`）：首句明确段落主题、一段一个重点、术语一致、声明与证据对应。README 指向公共首页步骤，并明确自助功能需要服务器启用，避免将本地实现描述成已经上线。

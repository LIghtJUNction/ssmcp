# ssmcp 0.8.0 客户端体验验收：Linux debug 构建

本批验证通过了 Linux 调试构建的格式、严格 Clippy、构建、Rust 测试以及六组客户端场景检查。默认库测试结果为 **715 通过、0 失败、8 个 ignored**；真实 PTY 下的 SSH composer、dashboard 和账号 relay composer 分别完成 **88、84、45 项断言**。各套件的结果分别列出，不把汇总记录、详细报告和重复执行的测试相加。[Rust 门禁](rust-gates.json) · [UI 门禁](ui-gates.json) · [Relay 门禁](relay-gates.json)

## 来源与适用范围

实现源码固定为 [`8d8172765644bfecdf881a5a62262bde93415e53`](https://github.com/LIghtJUNction/ssmcp/commit/8d8172765644bfecdf881a5a62262bde93415e53)，版本为 `0.8.0`。本目录以 `20261118` 为归档批次；原始报告没有墙钟时间字段，因此不补填执行日期或时刻。后续文档和安装打包修改单独记录，不更换这些结果对应的源码提交。

本报告只证明下面列出的 **Linux debug/dev 构建和隔离测试场景**。命令未使用 `--release`；正式 release 二进制、跨平台安装包、预发布部署和生产升级需各自的独立证据。本目录中的 [deployment-preflight.json](deployment-preflight.json) 是另行整理的部署前检查，不属于下述九份 debug 验收报告。

## 1. 构建与 Rust 门禁

[rust-gates.json](rust-gates.json) 记录的八个阶段全部退出 `0`。下表耗时是外层命令计时；`--no-run` 阶段只编译测试可执行文件，不计为已运行测试。

| 阶段 | 命令 | 结果 | 命令耗时（秒） |
| --- | --- | --- | ---: |
| 格式 | `cargo fmt --all -- --check` | 退出 0 | 1.61 |
| 严格静态检查 | `cargo clippy --locked --all-targets -j1 -- -D warnings` | 退出 0 | 65.65 |
| 调试构建 | `cargo build --locked --bins --example vt_snapshot -j1` | 退出 0 | 121.85 |
| Router 测试程序编译 | `cargo test --locked --lib --no-run --message-format=json -j1` | 编译成功 | 139.09 |
| 默认 Rust 测试 | `cargo test --locked -j1 -- --test-threads=4` | 逐目标结果见下表 | 115.79 |
| HTTP 显式补跑 | `cargo test --locked --test two_tools -j1 -- --include-ignored --test-threads=2` | 9 通过，0 失败，0 ignored | 2.77 |
| code_mode 显式补跑 | `cargo test --locked --lib code_mode::tests -j1 -- --include-ignored --test-threads=2` | 22 通过，0 失败，0 ignored，701 filtered | 4.07 |
| Context helpers 显式补跑 | `cargo test --locked --test context_helpers -j1 -- --ignored --test-threads=1` | 2 通过，0 失败，0 ignored | 1.03 |

### 默认测试的完整目标明细

下表逐项转录默认 `cargo test` 的结果。所有目标均为 `0 failed`、`0 measured`、`0 filtered out`；耗时为各测试程序内部计时，不能与外层命令耗时混用。[原始阶段摘要](rust-gates.json)

| 目标 | 通过 | ignored | 内部耗时（秒） |
| --- | ---: | ---: | ---: |
| `src/lib.rs` | 715 | 8 | 34.45 |
| `src/bin/ssmcp.rs` | 0 | 0 | 0.00 |
| `src/bin/ssmcpd.rs` | 9 | 0 | 0.00 |
| `tests/cc_backend.rs` | 5 | 1 | 1.59 |
| `tests/client_reload.rs` | 6 | 0 | 7.39 |
| `tests/client_server_config.rs` | 5 | 0 | 0.06 |
| `tests/client_startup.rs` | 9 | 0 | 4.25 |
| `tests/codex_backend.rs` | 14 | 2 | 21.60 |
| `tests/collaboration.rs` | 3 | 0 | 0.13 |
| `tests/context.rs` | 12 | 0 | 0.49 |
| `tests/context_helpers.rs` | 0 | 2 | 0.00 |
| `tests/context_runtime.rs` | 1 | 0 | 0.48 |
| `tests/oauth_security.rs` | 11 | 0 | 1.13 |
| `tests/pi_backend.rs` | 9 | 1 | 3.60 |
| `tests/release_client.rs` | 4 | 0 | 1.34 |
| `tests/resources.rs` | 10 | 0 | 0.55 |
| `tests/rsync.rs` | 3 | 0 | 0.18 |
| `tests/terminals.rs` | 4 | 0 | 1.74 |
| `tests/two_tools.rs` | 2 | 7 | 0.04 |
| `tests/watchdog.rs` | 2 | 0 | 1.09 |
| `tests/workspace_bridge.rs` | 0 | 3 | 0.00 |
| Doc-tests | 0 | 0 | 0.00 |

显式补跑与默认运行存在重叠。HTTP 的 9 项包含默认已经通过的 2 项和默认 ignored 的 7 项；code_mode 的 22 项也包含默认运行已通过的测试；context helpers 的 2 项对应默认 ignored 的两项。真实 relay composer 场景还会显式启动默认 ignored 的 router fixture，其退出码在 [relay-composer-e2e.json](relay-composer-e2e.json) 中记录为 `0`。这些结果不能简单相加为“新增通过数”。

`ignored` 保留原始含义。本包没有为 `cc_backend`、`codex_backend`、`pi_backend`、`workspace_bridge` 中剩余的 ignored 项提供独立补跑证明；也不据此推断所有被忽略的库测试都已执行。部分 ignored 项是由其他测试启动的子进程 fixture，而非独立的用户场景。

### 后续本地发布脚本单元验证

维护者在后续提交 `0efb04bfbd7f7b0f616f03a6123608cbffc3643e` 的本地 scratch checkout 执行了 `python3 -B -m unittest discover -s scripts -p 'test_release.py' -v`，记录为 **50 tests / 0.941 秒 / OK**。该提交相对上述实现源码只包含安装打包与部署前检查文档变更。用例覆盖部署版本匹配、stage receipt、安装失败回滚和制品摘要等单元夹具；临时仓库与假 API 的 tag/Release 操作不代表远程发布。

这条结果由维护者本轮执行记录补充，**没有归档独立原始日志**，也不在九份 JSON 中；不将其计入真实 staging 或生产升级验收。

## 2. 客户端场景验证

下表的“断言数”是各脚本实际记录的检查项，包含流程中重复验证的条件，不代表彼此独立的功能数量。UI 的四个场景以 [ui-gates.json](ui-gates.json) 关联实现提交和二进制哈希；两个 relay 场景对应 [relay-gates.json](relay-gates.json)。汇总与详细报告是同一次运行的两层记录。

| 场景与详细证据 | 断言数 | 主要验证内容 | 实际运行范围 |
| --- | ---: | --- | --- |
| [Composer](composer-e2e.json) | 88 | 输入与 shell 分离、队列回执、取回编辑、取消、定向成员、草稿保留、实际 `/reload` | 本地真实 OpenSSH 公钥认证、OAuth、客户端 PTY 和独立 vt100 解码器 |
| [Dashboard](dashboard-e2e.json) | 84 | 源码审阅、日志滚动和过滤、主题与布局、终端输入及退出恢复 | 本地真实 OpenSSH、OAuth、客户端 PTY 和独立 vt100 解码器 |
| [Account relay](account-relay-e2e.json) | 37 | CLI 登录、邀请加入、真实客户端原生工具、断线重连、撤销和退出清理 | 真实客户端连接 Python 合成账号网关；回环 WebSocket |
| [Relay composer](relay-composer-e2e.json) | 45 | 实际响应边界交付、CAS 编辑、同账号重连、草稿与安全未交付请求交接 | Rust 测试可执行文件中的生产 Broker/router、回环 HTTP/WS、真实客户端 PTY |
| [TTY observer](observer-e2e.json) | 7 | 真实 PTY、输出和退出码、viewer 复用与并行、取消清理、关闭后恢复 | recording launcher 启动实际 `ssmcp viewer`；未验证特定桌面终端模拟器的窗口外观 |
| [UI smoke](ui-smoke.json) | 38 | 明暗主题、窄屏、输入光标、样例源码与队列导航、调整尺寸、退出恢复 | 生产 compositor、样例事件和隔离 PTY；无真实服务连接 |

### 队列与重载的已验证行为

Composer 和 relay composer 均验证：界面轮询不会抢走待交付请求；请求在后续符合条件的 `code_mode` 响应中以结构化内容和单独标注的用户要求文本交付；已交付请求不会在下一次响应重复出现。取回后进入 editing 状态的请求暂不交付，重新提交保留同一 ID 并推进修订号。[Composer 断言](composer-e2e.json) · [Relay composer 断言](relay-composer-e2e.json)

SSH composer 进一步验证了真实注册成员的公开 roster、身份绑定与精确收件人选择：显示名称相同不能代替认证，其他成员不能领取定向请求；从精确成员改回 `Next caller` 后，下一位普通调用者可以领取。复制历史请求、临时停放草稿和后台回执不会覆盖正在编辑的文本。[Composer 断言](composer-e2e.json)

两条传输路径的 `/reload` 都实际重新执行了前台客户端，并产生新连接 ID。SSH 登录码模式需要重新授权，原连接的 OAuth grant 失效；具名账号 relay 保持同一账号，现有身份 OAuth grant 可控制新连接。两者均验证了未发送草稿恢复、经服务器确认未交付请求的安全重发，以及已交付历史不重发。[SSH 重载证据](composer-e2e.json) · [账号 relay 重载证据](relay-composer-e2e.json)

### 账号 relay 的认证边界

45 项 relay composer 使用 [relay_composer_fixture.rs](../../../src/http/relay_composer_fixture.rs) 在测试可执行文件中运行生产 Broker 和 HTTP router。DCR、两侧 PKCE、明确 consent、token exchange、账号授权、WebSocket 帧、队列 CAS 与 `code_mode` 响应均经过真实实现；上游 GitHub 提供方是隔离的合成服务。[场景脚本](../../../scripts/e2e_relay_composer.py) · [运行结果](relay-composer-e2e.json)

这套场景使用 **loopback HTTP/WS**，原始结果明确为 `tls_verified: false`、`real_github_account: false`、`models_started: false`。因此它不证明公网 TLS、真实 GitHub 账号登录或独立 `ssmcpd` 服务部署。原有 37 项 [account-relay 脚本](../../../scripts/e2e_account_relay.py) 的网关本身也是 Python 夹具，不能用它替代生产 router 的认证与队列验证。

### 耗时口径

| 场景 | 详细报告 `duration_seconds` | 汇总命令耗时（秒） |
| --- | ---: | ---: |
| Composer | 15.878 | 16.24 |
| Dashboard | 35.319 | 35.53 |
| Account relay | 6.077 | 6.077 |
| Relay composer | 7.383 | 7.383 |
| TTY observer | 未记录 | 15.41 |
| UI smoke | 未记录 | 5.74 |

Composer/dashboard 的脚本内部计时与外层进程计时边界不同，两个数值都保留。Relay 的 `duration_seconds` 是归档执行器记录的整条命令耗时；observer 和 UI smoke 的详细报告没有独立耗时字段，不能补造脚本内部时间。这些是一次功能验收的运行耗时，不是性能基准。[UI 计时](ui-gates.json) · [Relay 计时](relay-gates.json)

## 3. 二进制与测试源码对应关系

三个门禁摘要中的实现提交一致；可交叉比对的二进制 SHA-256 没有冲突。composer/dashboard 详细报告中的二进制哈希与 UI 摘要一致；observer/UI smoke 自身不保存完整构建来源，其来源由 UI 摘要关联。下表保留本批构建产物的原始哈希，哈希被列入执行器摘要不表示每个场景都实际启动了该产物。[UI 构建记录](ui-gates.json) · [Relay 构建记录](relay-gates.json)

| 产物 | SHA-256 |
| --- | --- |
| `ssmcp` | `e0510216d7125eb0ea511e66596e11227ec91aaa52acfa6a6742956d2c402b7f` |
| `ssmcpd` | `a6ffed46b6261ec37e87abc3a34b507277422a2fe93c8a684dd12c585bbf86ed` |
| `vt_snapshot` | `4569bc6bb436b103a2c9ea8e9d5f2a72a63dea5badad9ed292c674c0c4159fb7` |
| `router_test_fixture`（库测试可执行文件） | `d4ee3f9cc023567cc4c0b283da031cd9cdee5a1cf544f50b34784c616c948518` |

详细报告还保存了相关 Python 脚本与 Rust router fixture 的源码哈希。归档时逐项核对了报告引用的六个不同文件，均与实现提交内容相符；未将后续文档变更的提交号写回原始结果。

## 4. 九份公开报告与规范化方式

| 文件 | 内容 |
| --- | --- |
| [rust-gates.json](rust-gates.json) | 八个 Rust 门禁的命令、退出码、耗时及逐目标测试摘要 |
| [ui-gates.json](ui-gates.json) | 四个 UI/PTY 场景的命令、结果、来源和构建哈希 |
| [relay-gates.json](relay-gates.json) | 两个 relay 场景的执行器摘要 |
| [composer-e2e.json](composer-e2e.json) | 88 项断言、源码/二进制哈希、PTY 字节数与摘要 |
| [dashboard-e2e.json](dashboard-e2e.json) | 84 项断言、源码/二进制哈希、PTY 与终端查询结果 |
| [account-relay-e2e.json](account-relay-e2e.json) | 37 项检查结果、传输范围及构建来源 |
| [relay-composer-e2e.json](relay-composer-e2e.json) | 45 项断言、认证范围、源码/二进制哈希及 fixture 退出码 |
| [observer-e2e.json](observer-e2e.json) | 7 项断言、viewer slot 与测试进程信息 |
| [ui-smoke.json](ui-smoke.json) | 38 项样例 UI 断言和范围说明 |

公开副本仅对 **41 个字符串值中的机器路径**作了规范化，并统一 JSON 排版：开发机的构建根目录替换为 `${BUILD_ROOT}`，临时报告与日志根目录替换为 `${EVIDENCE_TMP}`。它们是来源占位符，不是可点击访问的远程目录；本页表格链接指向实际归档文件。

其余字段和结构保持原样，**所有数字、布尔值、断言文本、哈希和退出码均未修改**，包括观察器报告中的测试进程 PID。未将汇总报告改写成详细报告，也未给没有 `status`、提交号或耗时的原始记录补添字段。原始 JSON 没有 PTY 明文，只保留字节数、摘要和固定终端查询响应；凭据模式检查未发现长 token、Bearer 凭据或私钥标记。

拆分前原始合并 JSON 的 SHA-256 为：

```text
18661a363a6b99d3fe44281b98e1fb29f15373e0f937dcbce9b9fbd4dc810e50
```

## 5. 复现入口

在上述实现提交的独立 Linux checkout 中运行，使用已安装的 Rust、Python 3、Git、OpenSSH 及项目运行依赖。SSH 场景需要能启动绑定回环高位端口的隔离 `sshd`；账号 relay composer 不依赖真实 GitHub 凭据。执行环境应清除继承的 `SSMCP_*` 和代理配置，避免使用现有客户端会话或账号缓存；脚本还会创建自己独立的认证、配置、会话目录和测试进程。[SSH 夹具](../../../scripts/e2e.py) · [Relay composer 夹具](../../../scripts/e2e_relay_composer.py)

Rust 门禁的完整命令已列在第 1 节，并保存在 [rust-gates.json](rust-gates.json)。编译 router fixture 时保留 Cargo 的 JSON 输出，从 `compiler-artifact` 中选择 `profile.test` 为 `true`、`target.kind` 含 `lib` 的 `executable`；不要假设测试程序一定位于 `target/debug/deps`。原始环境实际使用的可执行文件路径保存在 `fixture_binary`。

下面的三个变量分别指向本次构建的 `debug` 目录、已创建的报告目录和 Cargo 返回的库测试程序。路径需由复现者填写，不能直接使用规范化占位符。

```sh
VERIFY_BIN_DIR=/absolute/path/to/debug
VERIFY_REPORT_DIR=/absolute/path/to/reports
VERIFY_FIXTURE_BIN=/absolute/path/from/cargo/compiler-artifact

python3 -B scripts/e2e_client_ui.py \
  --bin-dir "$VERIFY_BIN_DIR" --out "$VERIFY_REPORT_DIR/ui-smoke"
python3 -B scripts/e2e_composer.py \
  --bin-dir "$VERIFY_BIN_DIR" --report "$VERIFY_REPORT_DIR/composer-e2e.json"
python3 -B scripts/e2e_dashboard.py \
  --bin-dir "$VERIFY_BIN_DIR" --report "$VERIFY_REPORT_DIR/dashboard-e2e.json"
python3 -B scripts/e2e_tty_observer.py \
  --binary "$VERIFY_BIN_DIR/ssmcp" --report "$VERIFY_REPORT_DIR/observer-e2e.json"
python3 -B scripts/e2e_account_relay.py \
  --binary "$VERIFY_BIN_DIR/ssmcp" --report "$VERIFY_REPORT_DIR/account-relay-e2e.json"
python3 -B scripts/e2e_relay_composer.py \
  --bin-dir "$VERIFY_BIN_DIR" --fixture-binary "$VERIFY_FIXTURE_BIN" \
  --report "$VERIFY_REPORT_DIR/relay-composer-e2e.json"
```

Relay composer 脚本会以 `--exact http::relay_composer_fixture::relay_composer_server_fixture --ignored --nocapture` 启动该测试程序，并管理其私有临时目录、停止信号和清理。无需另启生产服务。新的复现运行会产生自己的耗时、PID 和 PTY 摘要；应与本批证据分别保存。[场景启动与清理实现](../../../scripts/e2e_relay_composer.py)

## 6. 本批证据的边界

本批未验证公网 TLS、真实 GitHub 账号授权、macOS/Windows 安装或优化 release 二进制，也未执行真实服务器 staging、服务重启或生产升级。UI smoke 使用样例数据，observer 使用 recording launcher；两者不等于真实用户会话或特定桌面终端的人工视觉验收。客户端场景未启动模型，不能把这些检查描述为模型执行质量测试。

后续 release、staging 或线上升级的结果应按实际源码与制品哈希另存报告，并链接其独立执行记录。本目录的 debug 数值、测试范围和来源保持不变。

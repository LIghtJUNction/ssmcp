# 0.8.0 最终集成验证记录

本目录归档固定源码 **`778f6cc6148fcd8bb8d88cdae4fc98f58378c4dc`** 的集成验证。该源码已合入 main `95b5f26` 的 007 恢复模式、后台电脑控制（CUA）及客户端能力发现改动，并保留客户端界面、源码审计和 v7 要求队列。Rust 组合覆盖、release 构建与独立目录安装已通过；六组 UI／连接流程、007 路由及 Node 接线测试共记录 **324 项通过检查**。安装包、隔离 staging 和版本化用户入口仍为 **pending**，生产网关尚未切换。[总执行记录](validation.json)

## 固定源码与历史范围

| 对象 | 来源或记录 |
| --- | --- |
| 本轮固定源码 | [`778f6cc6148fcd8bb8d88cdae4fc98f58378c4dc`](https://github.com/LIghtJUNction/ssmcp/commit/778f6cc6148fcd8bb8d88cdae4fc98f58378c4dc) |
| Git tree | `c7ccc2bb4d9722f52b314f0c0dc650c624cfb074` |
| 合入的 main | [`95b5f26125ede8825f12feb79b7f870055d004b5`](https://github.com/LIghtJUNction/ssmcp/commit/95b5f26125ede8825f12feb79b7f870055d004b5)，包含 PR #22 的 007 恢复模式及 PR #19 的 CUA 集成 |
| 复用验证的前一提交 | [`73a775d74661fec26767ccac9d4b6102b0c42843`](https://github.com/LIghtJUNction/ssmcp/commit/73a775d74661fec26767ccac9d4b6102b0c42843)；与本轮仅相差 `tests/resources.rs` 和 `docs/client-ui.md` |
| 构建与测试输入清单 | [source-manifest.json](source-manifest.json)，**210 个文件** |
| 清单核对结果 | 所列本地与远端文件逐项一致；远端构建工作树在采集时干净 |

清单的聚合 SHA-256 为：

```text
1bd66e912ad23b4c07db25242c1b85797e81bc3a30071fd7c9bf5383cd571d0f
```

计算对象为清单中的“相对文件路径 → 文件 SHA-256”字典，按 `json.dumps(files, sort_keys=True, separators=(',', ':'))` 序列化为 UTF-8 后求 SHA-256。该值证明所列本地与远端输入一致；构建、运行和部署分别以下述执行记录为准。[清单及算法](source-manifest.json)

此前 `51ec5ad` 构建与 `555307f` 安装包属于[上一轮集成候选](../integrated/README.md)。其测试、二进制哈希和 staging 回执保留为历史记录，不转记为本轮结果。

## Rust 验证与修正记录

### 保留首次失败及修正范围

合并提交 `6777ea8` 的首次检查在一处测试格式差异处停止；`9aaaa99` 修正格式后，严格 Clippy 指出新合入模块的规范问题。`73a775d` 保留逐一等待所有子进程的清理行为，合并内部资源路径参数，使用等价的分页整除判断，并将 guide 常量移到测试模块前；guide 内容未变。这两次失败的命令和退出码分别保存在[格式检查记录](format-initial-attempt.json)和[Clippy 检查记录](lint-initial-attempt.json)。

在 `73a775d` 上，格式与严格 all-target Clippy 已通过。随后默认 `cargo test` 完成库测试和前面的集成目标，但在 `tests/resources.rs` 停止，**原命令退出码为 101**：资源目标通过 8 项、失败 2 项。旧测试仍按每个客户端 6 个目录项计算，新能力发现接口实际提供 9 个；首个客户端目录因此从 11 项增至 14 项，第二个客户端增加 9 项。原始失败和 panic 摘要保存在[资源测试首次记录](resources-initial-attempt.json)。

`778f6cc` 只修改该测试和 UI 文档。资源测试现在核对 **5 个全局 URI 加每个客户端 9 个 URI 的完整集合**、**13 个资源模板的精确集合**、新连接带来的新增集合及旧集合保留；断线后，9 个目录入口与 `resource`、`skills/read`、`agents/read` 三条参数化路由都必须拒绝旧客户端 ID，并验证目录恢复到断线前的原集合。旧版与新版 MCP 协议的 HTTP 状态和错误码继续分别断言。[测试修正](../../../../tests/resources.rs)

### 组合覆盖及来源

[rust-gates.json](rust-gates.json) 明确标记 `aggregate_only: true`，并保留 `original_cargo_test_exit_code: 101`。它组合了 `73a775d` 上已经完成的目标与 `778f6cc` 上的接续执行，**不表示在新提交上重新运行了一次完整的 `cargo test`**。复用前，脚本检查两提交的改动仅限上述测试和文档，运行时代码、依赖与其他测试输入保持不变。

| 验证部分 | 源码与执行结果 |
| --- | --- |
| 全仓格式检查 | `778f6cc`：`cargo fmt --all -- --check`，退出 0 |
| 严格 Clippy | 复用 `73a775d` 的 `--all-targets -- -D warnings`，退出 0；对修改后的 `resources` 测试在 `778f6cc` 重跑严格 Clippy，退出 0 |
| 库测试 | 复用 `73a775d`：**755 通过、9 ignored** |
| 两个 bin 测试目标及此前完成的集成目标 | 复用 `73a775d`；连同库测试共 **16 个目标、844 通过、15 ignored**，逐目标结果见报告 |
| 资源集成测试 | `778f6cc`：**10 通过、0 失败** |
| 接续的 `rsync`、`terminals`、`two_tools`、`watchdog`、`workspace_bridge` 默认目标 | `778f6cc`：依次 **3、4、2、2、0 通过**；`two_tools` 有 7 项 ignored，`workspace_bridge` 有 3 项 ignored |
| 文档测试 | `778f6cc`：命令退出 0，当前 **0 个 doctest** |
| HTTP 回归，包含默认 ignored 用例 | `778f6cc`：`two_tools --include-ignored`，**9 通过** |
| `code_mode` 回归，包含默认 ignored 用例 | `778f6cc`：库测试筛选 `code_mode::tests --include-ignored`，**22 通过** |
| 上下文 helper 回归 | `778f6cc`：`context_helpers --ignored`，**2 通过** |

Cargo metadata 列出的 **22 个默认测试目标**均有成功结果，组合默认覆盖为 **865 通过、25 ignored**。ignored 表示该次默认运行没有执行这些用例；后面显式执行的 HTTP、`code_mode` 和上下文回归按各自记录报告，与默认结果有重叠，数量不相加。接续检查于 **2026-11-17 23:11 UTC** 完成，执行后工作树仍干净。[命令、逐目标结果与来源检查](rust-gates.json)

用于后续路由场景的 debug 库测试 fixture 编译于 `73a775d`，依据上述运行时输入等价检查用于 `778f6cc` 的验证；它不是 release daemon。其 SHA-256 为 `5716476f9fb7f44d82100300d72b25146e61164090b38db0e7d2e4ca048ceffd`，大小为 **176,295,696 字节**。[fixture 来源](fixture-binary.json)

## 两处兼容修复

### 007 声明实际实现的协议版本

007 的最小接收循环支持调用与控制器元数据，因此 Hello 明确使用 **v5 `CONTROLLER_VERSION`**。若沿用完整客户端的 v7 版本号，网关会在 `code_mode` 调用结束后发送该循环不支持的审计完成帧，导致恢复连接断开。修复不要求 007 加载完整界面、源码审计或要求队列。[实现](../../../../src/client/zero.rs)

本轮专用回归 **20 项全部通过**：5 项检查真实客户端 Hello、v5 协议和最小恢复能力，另 15 项通过生产 Broker/router 完成隔离 OAuth 流程与连续两次 HTTP `code_mode` 调用。两次调用结束后，原 `client_id` 和 PID 仍在线，没有以自动重连取得新 ID 代替连接保持；恢复进程没有因此启动 PTY、工具后端或子进程。路由 fixture 退出码为 0，清理错误列表为空。[执行记录](007-routing-e2e.json) · [回归脚本](../../../../scripts/e2e_007.py)

该路由场景使用 `778f6cc` 的 release 客户端和前述 `73a775d` debug 库测试 fixture，运行时输入等价依据见上文。Broker/router 与 OAuth 路径使用生产实现，上游 GitHub 为合成提供方，传输限于 loopback HTTP/WS。本次没有验证公网 TLS、真实 GitHub 账号授权、已安装的网关 daemon 或原生 systemd 生命周期。

### 能力与资源查询保持观察语义

以下四个客户端查询与既有 `ssmcp.ui.*` 一样，单独轮询时不默认领取 `next` 用户要求：

- `ssmcp.client.capabilities`
- `ssmcp.resources.list`
- `ssmcp.resources.templates`
- `ssmcp.resources.read`

这四个原语仍记录本次实际调用目标的源码审计。执行其他实际工具操作，或通过经过身份验证的 `collab.bind` 显式绑定当前调用，继续适用正常的要求交付规则。目标跟踪单元回归与 HTTP 队列回归覆盖默认查询不领取、源码仍保留及显式绑定仍可交付；它们分别包含在本轮 **22 项 `code_mode`** 和 **9 项 HTTP** 通过结果中。[目标跟踪实现与单元回归](../../../../src/code_mode.rs) · [HTTP 回归](../../../../tests/two_tools.rs) · [执行记录](rust-gates.json)

## Release 构建与目录安装

在 Linux x86_64 开发机上，从 `778f6cc` 运行 `cargo build --locked --release --bins --example vt_snapshot -j1`，**150.277 秒完成，退出 0**。随后 `make install` 向独立 `DESTDIR` 安装，**0.065 秒完成，退出 0**。客户端和 daemon 均报告版本 `0.8.0`，安装副本与构建产物逐项匹配；构建后工作树干净。[构建与安装记录](release-build.json)

| 产物 | 字节数 | SHA-256 |
| --- | ---: | --- |
| `ssmcp` | 8,021,528 | `53882aa0fc9d8c95a6966c6191771931194858a3df6f586fb0644d92db815b50` |
| `ssmcpd` | 5,375,864 | `4efaf996193c075bb3328963c1e9728333e5a6f1430eb9d1f96896432a56862f` |
| `code-mode.js` | 37,747 | `57f014501bf0313f966935fbe1a05613aaf75816a569b65ff15346cc9d2a8bdf` |
| 测试用 `vt_snapshot` 解码器 | 438,592 | `409e630cd3390d105f7de74200b1cdb97fecbe0cadd9da50ef2ef63b3572e58b` |

本阶段的安装位置是独立验证目录。后续测试直接使用该目录中的 release 程序、runner 和解码器；版本化用户入口、系统安装包与网关切换分别记录，不由这次目录安装推定。

## UI 与连接流程验证

测试脚本和 release 二进制均对应 `778f6cc`。以下六组记录 **299 项检查全部通过**，各组命令均退出 0；它们覆盖实际终端渲染、输入、任务输出和连接流程，没有调用模型。[场景总表及产物来源](release-ui-gates.json)

| 场景 | 通过数 | 已检查的行为与环境 |
| --- | ---: | --- |
| [预览与主题](preview-e2e.json) | 38 | 实际合成器、固定示例事件及隔离 PTY；深浅主题、窄屏、Unicode 输入、语法高亮、直接滚动、过滤、侧栏与退出恢复 |
| [输入框与队列](composer-e2e.json) | 88 | 隔离 OpenSSH／OAuth 与实际 PTY；提交、取回、编辑、取消、精确成员绑定、响应附加、草稿保留及本地 reexec |
| [日志与审计布局](dashboard-e2e.json) | 84 | 实际命令和工作区；命令输出不重复、真实退出状态、审计源码、缩放、Git 状态、直接滚动及终端恢复 |
| [账号 relay](account-relay-e2e.json) | 37 | Loopback WebSocket 和合成 GitHub 提供方的账号连接流程；不使用 SSH 或模型 |
| [账号 relay 输入框](relay-composer-e2e.json) | 45 | 生产 Broker/router fixture 与 loopback HTTP/WS；真实队列交付、版本化取回编辑、同账号重载及安全交接 |
| [命令观察窗口](observer-e2e.json) | 7 | 记录型启动器与实际 viewer；PTY 输出、窗口复用、并发分配、输入 EOF、取消、重开窗口及现有终端命令 |

日志历史场景核对了 **128 行**保留边界：预期与实际最早记录均为 541，重复 Home 保持偏移 128，End 回到偏移 0。该结果验证当前有界回看行为；完整任务输出仍按任务游标和归档接口读取。[边界实测](dashboard-e2e.json)

两个重载场景分别核对了不同授权语义：SSH 登录码连接重载后旧授权失效；命名账号 relay 的客户端重载保留账号身份和已有身份授权。两者均检查新客户端 ID、草稿恢复、确定未交付要求的交接及已交付记录不重放。账号 relay 场景使用前述 debug fixture 的生产路由实现和合成 GitHub，未验证公网 TLS 或真实 GitHub 登录。[SSH 场景](composer-e2e.json) · [账号场景](relay-composer-e2e.json)

### 动态能力与资源 helper

`scripts/test_client_capabilities.mjs` 通过实际 `code_mode` worker 的 JSON 传输执行 **5 项检查，0 失败、0 skipped**，对应已安装 runner 的 SHA-256 见上表。检查覆盖未来原语的动态调用、按客户端读取新鲜能力清单、资源游标与未知字段保留、错误及参数校验、旧调用与资源 helper 的请求兼容。[Node 结果](capabilities-node.json) · [脚本](../../../../scripts/test_client_capabilities.mjs)

这些检查验证 JavaScript helper 接线及响应语义，宿主回复由 fixture 提供；真实设备上的浏览器、桌面与权限交互仍需各自验收。六组 UI／连接检查 299 项，加上 007 路由 20 项和 Node 5 项，合计 **324 项功能检查**。Rust 用例与下面的合成工作流样本不计入这个数字。[汇总](validation.json)

### 合成工作流样本

另执行了一次合成协议与工作流检查：500 个合成工具、两次目录扫描、8 条命令和 0.5 秒空闲阶段。扫描和空闲后复查均得到 **617 个工具、10 页**，目录摘要一致；8 条命令的结果已核对，客户端正常退出，运行期间二进制哈希未变。该次采样耗时 **1.673 秒**，报告状态通过。[完整记录](synthetic-benchmark.json)

报告包含客户端进程的内存采样，排除子运行时；本次 `budget_mib` 为 `null`，没有设置性能或内存门槛。该结果用于确认当前合成流程可运行，不用于推导真实模型负载下的性能、长期常驻内存或相对旧版的改善。

## 本轮验收状态

尚未生成的证据以下仅列文件名，待实际归档后再加入链接。预期数量和未执行的 ignored 用例不作为通过结果；构建、UI、打包和部署阶段分别记录。

| 验收项 | 当前状态 | 证据与范围 |
| --- | --- | --- |
| Rust 格式、严格 Clippy、默认及显式 ignored 测试 | **组合覆盖通过** | [rust-gates.json](rust-gates.json)；保留前次失败及源码复用依据，见上文 |
| Release 构建与独立目录安装 | **通过** | [release-build.json](release-build.json)：`778f6cc` 构建、目录安装和产物一致性 |
| 客户端 UI、composer、队列及 reload 场景 | **299 项通过** | [release-ui-gates.json](release-ui-gates.json)：六组场景的命令、二进制与 fixture 来源 |
| 007 v5 与真实路由连接保持 | **20 项通过** | [007-routing-e2e.json](007-routing-e2e.json)：实际 Hello、连续调用、同一连接身份及 fixture 清理 |
| 客户端动态 capabilities/resources 接线 | **5 项通过** | [capabilities-node.json](capabilities-node.json)：Node worker 接线；与 HTTP 队列回归分别记录 |
| 合成协议与工作流样本 | **通过** | [synthetic-benchmark.json](synthetic-benchmark.json)：单次合成样本，没有性能门槛 |
| 本轮安装包 | **pending** | 固定打包提交、包摘要及包内产物与已测试 release 的一致性 |
| 本轮隔离 staging | **pending** | 独立 unit／端口／状态目录、真实检查结果、清理回执及生产状态前后核对 |
| 版本化用户入口 | **pending** | 新候选目录、入口切换与默认客户端保留核对 |
| 正式 Release 与 crates.io 发布 | **未执行** | 本页记录候选验收，不声明正式发布或其他平台构建完成 |
| 生产网关切换 | **未执行** | 需独立操作与回执；不由构建、打包或 staging 成功推定 |

007 轻量场景的复现入口如下；`--fixture-binary` 使用 [fixture-binary.json](fixture-binary.json) 中的 `binary` 路径，命令本身不是运行证据。

```bash
python3 -B scripts/e2e_007.py \
  --binary /ABS/ssmcp \
  --fixture-binary /ABS/lib-test-binary \
  --routing-only \
  --report /tmp/ssmcp-007-routing.json
```

## 结果解释边界

队列的 `delivered` 表示要求已附加到符合目标的 `code_mode` 响应，不表示模型已阅读或执行。完整队列需要协议 v7，源码审计需要 v6；007 保持 v5 的恢复能力，不提供这两项完整客户端功能。[客户端界面契约](../../../../docs/client-ui.md) · [007 实现](../../../../src/client/zero.rs)

Release 客户端、release daemon 与 debug 库测试 fixture 分别承担不同的验证角色。合成账号或工具只能支持其覆盖路径的结论；CUA facade 或能力目录接线测试不能代替真实浏览器、桌面环境与权限交互验收。本轮不复用旧候选的内存数值，也不依据合并成功声称性能改善或发布完成。

归档 JSON 的 `_publication.private_paths_normalized` 标明私有机器路径已规范化。`_publication.source_object_sha256` 按所列算法对原始 JSON 对象计算；嵌套的 `report_sha256` 仍对应规范化前的原始私有文件，不能用来核对当前公开副本。失败报告保留当时的状态、退出码与失败目标，后续修正另行记录。

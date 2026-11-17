# ssmcp 0.6.2 开发验收：浏览器控制与使用效率

**本轮本地开发验收矩阵已通过。用户现有 Chrome 的扩展控制、正式 CI、发布、安装和线上部署仍未完成验收。** 本报告记录 2026-11-17 的实际结果，不能代替统一 Harness 架构全部 12 项验收。

## 版本和证据范围

实现提交为 [`eb86b8ff5d25d6c0222552d80f4f7a2aa93e7f89`](https://github.com/LIghtJUNction/ssmcp/commit/eb86b8ff5d25d6c0222552d80f4f7a2aa93e7f89)，相关 [PR #10](https://github.com/LIghtJUNction/ssmcp/pull/10) 已合并为 [`ec199f386b6b0e30619bd68ac5cece7b138f69ce`](https://github.com/LIghtJUNction/ssmcp/commit/ec199f386b6b0e30619bd68ac5cece7b138f69ce)。验收记录确认两个项目副本均已 fast-forward 回 `main` 且工作区干净，详见 [合并状态](evidence/source-status.json)。后续 [`76613f3a0506f00a74b23e59bdd02a3c828fd6fc`](https://github.com/LIghtJUNction/ssmcp/commit/76613f3a0506f00a74b23e59bdd02a3c828fd6fc) 只修改 `scripts/e2e.py` 与 `scripts/e2e_unified.py`。只读比较确认 `src/`、`Cargo.toml` 和 `Cargo.lock` 在两提交之间未变化；三组 E2E 使用同一对冻结二进制。[提交比较](evidence/source-comparison.json) · [构建收据](evidence/final-r3/binaries.json)

原始本机材料位于工作区的 `target/acceptance/browser-efficiency-20261117/`。本报告旁保留选定的 JSON 收据、报告和失败摘要；公开报告内容隐藏了主目录和临时 PID；只有 [原日志索引](evidence/native-log-index.json) 保留操作者给定的项目绝对路径，便于定位证据。Clippy 中断日志的原文件为 102 B，打包的是替换项目绝对路径后的 41 B 脱敏副本，未打包原文。其他原始日志全文、令牌和浏览器敏感存储均未收录。原始文件与脱敏副本的 SHA-256 分开记录。[证据清单](evidence/evidence-manifest.json) · [机器可读报告](verification.json)

## 本地结果矩阵

| 验证 | 结果 | 范围与证据 |
| --- | --- | --- |
| Rust `--all-targets` | **555 passed / 0 failed / 21 ignored** | 18 个结果摘要含零用例套件；library 469 / 0 / 7；[gate](evidence/final-r3/gates.json) 与 [日志摘要](evidence/final-r3-observed-summary.json) |
| Clippy `--all-targets -- -D warnings` | exit 0 | [final-r3 gate](evidence/final-r3/gates.json) |
| `tools.search` | **13 passed** | 实际 Node worker 的 JSON transport；[日志摘要](evidence/final-r3-observed-summary.json) |
| CUA facade / execution helpers | **5 / 5 passed** | 两组独立脚本；[日志摘要](evidence/final-r3-observed-summary.json) |
| release.py 单元测试 | **46 passed** | Python unittest；[日志摘要](evidence/final-r3-observed-summary.json) |
| Browser E2E | **51 checks passed** | 本轮拥有的隔离 Chromium 与受控 CDP 连接；[报告](evidence/final-r3/e2e-browser/report.json) |
| Daemon/watchdog E2E | **145 checks passed** | 实际 Linux systemd user manager；[报告](evidence/final-r3/e2e-daemon/report.json) |
| Unified E2E，修正后第二轮 | **120 checks passed** | 实际本机 SSH 公钥、OAuth PKCE、HTTP MCP、独立进程和 PTY；[报告](evidence/final-r3/e2e-unified-r2/report.json) |
| 原生 `ssmcp.browser.discover` | **passed** | 实际 HOME 的只读元数据调用，浏览器连接前后均 false；[报告](evidence/final-r3/live-discovery/report.json) |

final-r3 的格式、长输出专项、Rust、Clippy、三个 Node 辅助测试组、release.py、构建与差分检查共 10 个 gate 全部 exit 0。长输出专项的 1 项通过、discovery 的 9 项以及新增 audit 的 2 项已经包含在 Rust 总数内，不再累加；ignored 不计作通过。E2E 中多个检查可以验证同一流程的不同条件，其数量也不当作彼此独立的用户需求数量。[gate 收据](evidence/final-r3/gates.json) · [精确测试摘要](evidence/final-r3-observed-summary.json)

| E2E | 报告内部计时 | 进程收据计时 | 退出码 |
| --- | --- | --- | --- |
| Browser | 6.199 s | 6.383 s | 0 |
| Daemon | 32.497 s | 32.669 s | 0 |
| Unified-r2 | 17.519 s | 17.605 s | 0 |

报告内部计时覆盖脚本记录的测试区间，进程收据计时还包含进程启动等开销；两者分别保留，未进行性能倍率推算。[Browser 收据](evidence/final-r3/e2e-browser/receipt.json) · [Daemon 收据](evidence/final-r3/e2e-daemon/receipt.json) · [Unified 收据](evidence/final-r3/e2e-unified-r2/receipt.json)

## 本轮改动及其验证

Node 运行时的成功探测可按真实可执行文件身份复用，`code_mode` 的准备工作移至受限的非阻塞任务；检测到文件变化或失败时不沿用旧成功。前序专项为 Node 7 项、Rust code_mode 17 项通过 / 1 ignored，最终完整 Rust gate 也通过。这里验证的是进程与协议行为，未给出未经测量的速度倍率。[机器可读报告](verification.json)

`tools.search` 在 `code_mode` 内检索注册表页面，默认只返回简短目录；请求 Schema 时保留整个原生 descriptor。搜索有页数、条数、输出大小和 cursor 校验，并沿用每页的授权检查。Browser E2E 中一次搜索读取 2 页、检查 100 个工具，返回 16 个浏览器匹配项，并核对 descriptor 与目录相同。它减少模型收到的目录内容与手工分页，不消除首次底层分页请求；broker 升级后才提供这个 helper。[搜索专项](evidence/final-r3-observed-summary.json) · [实际搜索观测](evidence/final-r3/e2e-browser/report.json)

跨标签页操作由实际 Chromium 验证。测试启动 `/usr/lib/chromium/chromium`，报告产品版本为 `Chrome/157.0.8103.38`，使用 ssmcp 拥有的临时隔离 profile。A 标签页的 Promise 等待时，B 标签页先完成原生调用，再显式释放 A；A 在 5 秒截止时间之前正常结束，没有靠超时放行。本次观察区间为 0.066 s，仅作为次序与并发行为的证据。测试同时验证了受控 loopback attach、断开后保留外部拥有的测试浏览器，以及清理 ssmcp 自己启动的浏览器。[Browser E2E](evidence/final-r3/e2e-browser/report.json)

Playwright driver 只发现已安装的机器接口，先验证版本，再以 `--extension` 启动原生 MCP 接口并握手、分页枚举；工具使用 `playwright.<native_name>` 命名空间。前序 Playwright 5 项、RPC 11 项协议回归及最终 Rust gate 已通过。实际用户扩展尚未连接，因此这些测试不代替真实扩展批准与工具调用；发现过程不运行 `npx` 安装依赖，也不调用 Harness 模型。[机器可读报告](verification.json)

metadata-only AuditMode 只由 Playwright 选用。新增两项回归验证事件进入终端、缓冲和归档之前已使用元数据模式，同时原生参数、结果及待批准请求保持完整。其他 provider 的既有审计契约保持原状；这不表示能观察所有外部程序的文件操作。[审计测试摘要](evidence/final-r3-observed-summary.json)

Daemon E2E 使用真实 Linux systemd user manager，验证独立会话、SSH 断线重连、进程崩溃后的重连、手动停止、会话移除、状态与日志的凭据隔离。收据确认清理完成且无清理错误；其观察到的重连时间只属于本轮测试条件。macOS launchd、Windows Task Scheduler、注销/重启自启动和真实 GitHub 身份提供者登录不在本轮覆盖内。[Daemon E2E](evidence/final-r3/e2e-daemon/report.json)

Unified E2E 在不暴露已安装 Harness 的受控环境中，验证 Shell 并发、UTF-8、长输出存储、文件与技能审计、客户端隔离、超时取消、父进程退出、过期清理、reload 和断线。12 个报告分段均通过，结束时 `remaining_observed_processes` 为空；这不替代已安装 Harness 的原生 MCP 枚举、工具调用及审批专项。[Unified E2E](evidence/final-r3/e2e-unified-r2/report.json)

## 实际开发机的只读发现

新增原生 discover 工具于 **2026-11-17 12:49:19（UTC+08:00）** 通过本机 OpenSSH、公钥、OAuth PKCE 和 HTTP MCP 调用。它读取实际 HOME 中的受限元数据，provider PATH 保持隔离；调用前后 `browser_connected` 均为 false，模型调用为 0。发现共 **8 个 profile：Google Chrome 6 个，Chromium 2 个**，匹配到下列三个 manifest。[原生发现报告](evidence/final-r3/live-discovery/report.json)

| Manifest 名称 | 观察到的版本 | 匹配依据 | 可调用状态 |
| --- | --- | --- | --- |
| ChatGPT | 1.26.1102.11740 | 已观察扩展 ID | 未验证 |
| Playwright Extension | 0.5.0 | 已观察扩展 ID | 未验证 |
| ChatGPT search | 1.11 | 名称关键词 | 未验证 |

表内仅描述 manifest。enabled 与 paired 均未检查，名称匹配的 search 项也没有被当作已授权的浏览器控制桥。本次 `limited=false` 表示扫描未耗尽预算，不表示所有文件都可访问：报告另有 2 个被拒绝的 JSON 文件和 7 个不可读进程可执行路径。扫描启用硬链接检查，但不声称能对同时变化的文件系统建立原子快照。[原生发现报告](evidence/final-r3/live-discovery/report.json)

**用户现有 Chrome 的扩展控制仍未实际完成。** 新发现工具在观察到的路径中未找到 Playwright MCP 入口，未启动协议握手；初始有界清单也未找到已安装的 `@playwright/mcp`。前序清单观察到的用户 Chrome 为 `157.0.8103.78`，与 E2E 隔离 Chromium 的 `157.0.8103.38` 分开记录。ChatGPT Browser 插件曾返回 originating-context 不可用，官方 Claude 扩展未在已观察范围发现；不能把扩展安装、fixture 或 Chromium 测试说成用户登录会话已可控。尚未观察到真实的待处理批准请求，未来连接仍须尊重原生批准。[原生发现报告](evidence/final-r3/live-discovery/report.json) · [已归档初始清单](https://drive.google.com/file/d/1dKFCcZmgw05I0w5lLOLzkcgWAdaZEe3I/view)

## 保留的失败和修正

| 运行 | 实际失败或中断 | 后续结果 |
| --- | --- | --- |
| final-r1 | Rust lib 466 / 1 / 7，exit 101；新增 discover 后旧工具数断言仍为 15 | 期待数同步为 16，final-r3 完整通过；[原收据](evidence/final-r1/gates.json) |
| final-r2 | Rust lib 468 / 1 / 7，exit 101；长输出最终状态回归失败 | 专项和完整 final-r3 通过；[原收据](evidence/final-r2/gates.json) |
| final-r2-remaining Clippy | 验收记录标记为中断；只有起始 Checking 日志，无完成收据，退出码未知 | final-r3 Clippy 是另一轮 exit 0；[保留日志](evidence/final-r2-remaining/clippy.log) |
| 第一轮 unified E2E | 报告 status=failed，完成 21 个检查后等待“四个 absent-Harness probes”超时 | Playwright 使 provider 数变为 5；脚本断言和环境隔离修正后 r2 120 checks 通过；[失败报告](evidence/final-r3/e2e-unified/report.json) |

历史 `v0.6.1` 正式工作流 [`43926079974` attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/43926079974/attempts/1) 与 [attempt 2](https://github.com/LIghtJUNction/ssmcp/actions/runs/43926079974/attempts/2) 仍为失败，Linux lib 分别为 344 / 2 / 6 和 345 / 1 / 6，均涉及 Node compatibility probe 的 3000 ms 边界。它们属于固定旧提交 `ff09364bdc3ec85d7debd3675c782bfce5d04045`，本轮本机通过不改写旧状态；整理报告时没有再次重试工作流。[历史失败摘要](evidence/history/release-0.6.1-failures.json)

## 冻结产物和剩余验收

| 冻结产物 | 实际版本 | SHA-256 |
| --- | --- | --- |
| `ssmcp` | 0.6.2 | `672fb50a2a1173437e8ea1702cba8cbea95e7dd506fc9a0c48c7e10f46d28900` |
| `ssmcpd` | 0.6.2 | `eba663591eca9c9e3bfce8dc9ecf0f72ccf7603884dc4983262fba76bd8bdc54` |

这些校验值标识本轮冻结并参加 E2E 的二进制，不表示已安装或部署。[构建收据](evidence/final-r3/binaries.json)

| 待完成项目 | 所需独立证据 |
| --- | --- |
| 用户现有 Chrome 的原生扩展连接 | 已安装 MCP server、真实 handshake、原生批准及工具调用结果 |
| ChatGPT / Claude 扩展可用性 | 对应上下文或原生机器接口实际可用，不能靠安装推断 |
| 0.6.2 正式 CI 与发布 | 新工作流 URL、结论、tag、资产及校验值 |
| 开发机安装与线上部署 | 实际运行版本、部署收据与线上协议检查 |
| 其他平台生命周期 | macOS / Windows 原生服务管理器及必要的注销/重启验证 |
| 完整架构验收 | 原始 12 项中尚未覆盖的已安装 Harness 动态工具和审批专项 |

截至本报告快照时，Google Drive 中已验证的是初始精简清单，ID `1dKFCcZmgw05I0w5lLOLzkcgWAdaZEe3I`，2409 B。最终验收材料的归档位置另附发布回执。[初始清单](https://drive.google.com/file/d/1dKFCcZmgw05I0w5lLOLzkcgWAdaZEe3I/view)

# ssmcp 0.6.2 更新说明

日期：2026-11-17（Asia/Taipei）。本页说明本轮源码变更；正式发布、制品、安装与部署结果以固定提交的实际验收记录为准。

0.6.2 在 main 现有的统一注册、独立 Shell、daemon/watchdog 和终端显示之上，改进运行时准备、浏览器并行操作及工具发现。Broker 对外仍只有 `list` 和 `code_mode`，原生工具继续通过统一目录与 SSH 路由。

## Node 探测：复用成功结果，准备阶段不阻塞异步任务

Linux 服务端按实际 Node 可执行文件的身份缓存成功的兼容性验证。文件路径、设备、inode、权限、大小或时间信息变化时重新验证；失败不写入成功缓存，验证期间文件变化也会明确报错。并发的首次请求共用一次受控探测，避免为每个代码请求重复启动相同检查。[运行时解析](../src/program.rs)

`code_mode` 把运行时查找和命令准备移到有界的阻塞工作线程，不占住异步执行器。请求取消后，已经开始的探测仍由原来的监督逻辑负责超时和回收；准备槽位有独立上限。Node 参数验证、bubblewrap 隔离和进程资源限制继续生效，未通过延长原有探测期限来隐藏故障。[代码模式实现](../src/code_mode.rs)

## 浏览器：按标签页隔离，并先识别现有会话

CDP 页面操作改为按标签页保持各自的连接、快照状态和顺序；不同标签页可以并发处理。对同一页的导航、快照以及基于元素引用的点击和输入，仍按依赖顺序等待。该变更改善任务隔离，不代表已经测得特定速度倍率。[浏览器实现](../src/browser.rs)

新增 `ssmcp.browser.discover`，只读观察浏览器进程、扩展清单与本机入口，并区分 Google Chrome、Chromium 等来源。扫描有数量和字节预算，达到边界时返回限制与诊断；Unix 元数据文件还检查链接数，不把不安全的链接当作正常清单读取。它不启动或重启浏览器，不读取 Cookies、历史记录或扩展 token 存储。[发现实现](../src/browser/discovery.rs)

用户指定正在使用的 Chrome 时，应先读取发现结果和 `ssmcp.harnesses.list`，再选择已有 CDP 端点或实际可调用的扩展桥。安装了扩展不等于已经启用、配对或获准控制；没有可用连接时报告具体缺口，不用独立 profile 或另一浏览器代替。[现有 Chrome 流程](browser-existing-session.md)

## Playwright：使用原生扩展 MCP 协议

新增可选 Playwright 提供者。适配器查找已有的 `playwright-mcp`，或使用 `SSMCP_PLAYWRIGHT_MCP` 指定的已安装入口；JavaScript 入口通过真实 Node 执行。版本检查后固定使用 `--extension` 启动，再完成 MCP 初始化和分页工具枚举，成功后才注册实际的 `playwright.*` 工具。缺少入口或握手失败只影响该提供者。[原生适配器](../src/backend/playwright.rs)

调用转发保留原生名称对应关系、完整 Schema、结果、错误与待处理请求。`ssmcp.playwright.events/requests/respond` 用于原生事件和请求的观察及回应，浏览器扩展自己的连接确认和批准仍由原生流程决定。发现不运行 npx/npm/corepack 去安装依赖，不提取扩展 token，也不调用 Harness 模型来代理浏览器操作。

## 工具目录：按需搜索，完整 Schema 仍可取得

`tools.search(clientId,{query,limit,include_schemas,cursor,max_pages})` 在代码模式中按名称或描述查询目录。默认返回实际注册名称、路由命名空间和短说明；`include_schemas:true` 返回整个原生 descriptor，保留 input/output Schema、注解和扩展字段。原来的 `tools(clientId,cursor)` 完整分页接口保持兼容。[worker 实现](../src/code_mode.js)

续查游标绑定客户端与查询选项，页内续查重新读取并校验页面摘要，每页继续经过原有授权路径。检索有分页、条数和输出预算；单个 Schema 超限会明确提示使用原始目录分页，不截掉字段或悄悄跳过工具。它减少返回给模型的无关内容和手工分页往返，首次扫描所需的底层目录请求仍然存在。[目录检索说明](backends.md#按需查询目录)

## 审计：显示状态元数据，原始结果保持保真

Playwright 的审计副本在进入终端、事件缓冲和现有日志存储前，只保留调用 ID、工具名、状态、字节数及原生数值进度。参数、结果正文、通知自由文本和 stderr 正文不会复制进这些审计记录；省略正文有明确标记。原生请求与返回仍走原来的协议路径，其它提供者保留已有的脱敏策略。[审计副本](../src/backend/events.rs) · [原生 RPC](../src/backend/rpc.rs)

这个变化发生在审计副本上，不改写返回给 agent 的原生结果，也不改写已保存的输出字节。独立命令的实时输出与原生结果查询仍按各自接口提供；外部工具没有报告的文件操作仍不能宣称已被完整审计。

## 升级和现场限制

`tools.search` 属于 broker 的 JavaScript worker，需要更新服务端；`ssmcp.browser.discover`、按页 CDP 操作与 Playwright 适配器属于客户端，需要更新客户端并重新发现能力。帮助文本已经区分两端，但源码文案更新本身不表示线上接口已经更新。

2026-11-17 的开发机只读清单观察到 Chrome `157.0.8103.78`、ChatGPT 扩展 `1.26.1102.11740` 与 Playwright Extension `0.5.0`；当时没有观察到可用 CDP 入口，也未在检查范围内找到 `@playwright/mcp`。ChatGPT Browser 工具返回 originating-context 不可用，已观察的 Chrome profile 与加载路径中未找到官方 Claude 扩展清单。这是该机器当时的状态，不是产品固定版本要求，也不证明实际扩展控制已完成。[现场清单](reports/browser-existing-session-20261117-inventory.json)

## 验证与历史记录

本轮完整的本地结果、前序失败、浏览器发现样本和产物校验值见[浏览器效率开发验收报告](reports/browser-efficiency-20261117/README.md)。其中 Browser、Daemon/watchdog、Unified E2E 分别通过 51、145、120 项检查；现有用户 Chrome 的扩展连接与正式部署另行验收。

Linux 本机最终 `final-r3` 的 Rust all-targets 测试合计 **555 passed / 0 failed / 21 ignored**，Clippy `-D warnings`、工具检索 13 项、CUA facade 5 项、执行助手 5 项和发布脚本 46 项回归均通过。源码构建的两个二进制都报告 `0.6.2`。跳过项、真实浏览器 E2E 和正式发布流程分别记录；这不是统一 Harness 全部 12 项验收完成的声明。

保留两轮前序失败：`final-r1` 为 466 passed / 1 failed / 7 ignored，原因是新增 `discover` 后旧工具数量断言仍期待 15 项；`final-r2` 为 468 passed / 1 failed / 7 ignored，长输出测试没有先确认任务完成就断言 artifact 完整。后者现对同一任务观察到终结状态，再严格检查退出码、EOF、capture 错误与完整内容；生产命令超时和输出策略未改变。原日志缺少当时的任务状态，不能把旧失败唯一归因于观察期限。另一次 Clippy 检查因连接重建中断，没有完成结果，未计为通过。

`v0.6.0` 与 `v0.6.1` 既有标签保持固定，不移动或重写。`v0.6.1` 的[正式发布 attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/43926079974/attempts/1)和[attempt 2](https://github.com/LIghtJUNction/ssmcp/actions/runs/43926079974/attempts/2)均因 Linux x64 的 Node 兼容探测超时而失败，后续发布步骤未完成。本轮修复与本机结果单独记录，不替代这些历史失败，也不提前宣布 0.6.2 已发布或已部署。

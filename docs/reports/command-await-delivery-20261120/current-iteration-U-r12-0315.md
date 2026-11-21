# SSMCP 0.10.25：候选版本检查通过，继续交互与发布验收

本页汇总候选版本 U 的本地检查结果和新发布核验器的交付状态。检查证据截至 **2026-11-21 03:09:06 UTC**；核验器文档已提交为 [`9f46b2a`](https://github.com/LIghtJUNction/ssmcp/commit/9f46b2af474b443abe39f64c2f5651f783ef1321)。当前发布状态为 **待验收、未发布**。

阅读顺序：源码改动、已完成检查、发布核验器、后续验收、结论与证据。完整历史保留在[迭代总报告](README.md)。

## 这次源码改动解决什么

候选提交 [`784370b`](https://github.com/LIghtJUNction/ssmcp/commit/784370b5fe1b817c788273a5f7b1ab826c743965) 以 `647bce2` 为唯一父提交，只修改五份 Python 交互验收脚本中的六处断言。脚本已经显式选中了 Next caller，期望的收件人值因此应为 `next`。修正让验收脚本准确检查这一选择状态，并继续保留尚未选择收件人时的空值检查。

该提交没有修改 Rust 产品实现；六份发布与安装配方的字节保持已审阅版本。接下来的交互验收将使用本次重新构建并绑定 U 的产物，检查草稿、焦点、光标、收件人和递送行为。

## U 阶段检查点：r12 Gate 已完成，交互验收与发布待续

观察截止：2026-11-21 03:09:06 UTC。本次结论仅归属于源码 **U `784370b5fe1b817c788273a5f7b1ab826c743965`**、tree `2c60d011da8c2b31aefd80367bf7f19e28f10b62` 及其 r12 完成回执。候选版本为 **0.10.25**。Gate 于 02:53:03.910 UTC 完成，外层退出记录于 02:53:04.024629 UTC 完成；35 个阶段的退出码均为 0，两层启动/控制回执也均为 0。[原件身份和逐阶段结果](gate-checkpoint-U-r12-0309.json)

### 已完成的检查及计数

| 范围 | 本轮已核结果 | 计数边界 |
|---|---:|---|
| 隔离环境库测试 | 2,050 通过，0 失败 | 本阶段另有 23 ignored、6 filtered；3 个嵌套子进程的单项通过摘要不累加 |
| 宿主环境库测试 | 6 通过，0 失败 | 与隔离库测试按清单分区 |
| 普通 integration | 19 个目标、101 通过 | 不包含下行显式 ignored worker |
| 显式 ignored worker | 1 通过 | 单独执行的 `integration-zuse_worker_entry` |
| 另行执行的 ignored 组 | 4 组、14 通过 | 分别为 2 + 7 + 3 + 2；不混入普通 integration |
| 构建、策略与静态检查 | 对应阶段均退出 0 | 包含策略检查、fmt、Clippy、Windows GNU 目标 Clippy、库/integration 编译及 release/decoder 构建 |

Gate 记录了各阶段的 ignored 与 filtered 范围。Windows GNU 项目覆盖跨目标静态检查；Windows 原生运行验收由正式平台流程单独记录。

### 17 项原始目标：已在同一库测试日志逐项核对

已把目标映射中的 17 个完整测试名逐条对到本轮原始库日志：每项恰好出现一次 `... ok`，映射行号全部一致。它们是上述 **2,050 项通过的子集**，不增加通过总数。

| 目标组 | 项数 | 库测试所覆盖的边界 |
|---|---:|---|
| 原生接收与回执 | 5 | 显式目标选择、缺失目标不回落到 Next caller、回执与草稿精确对应 |
| 回复归属 | 2 | 回复与重试回执保持原线程归属 |
| 文件引用 | 5 | 不可用引用保留原文与提示；覆盖预算、重试冻结和队列确认 |
| 技能与补全 | 5 | 技能来源优先级、补全范围，以及明确选择/发送之后才递送 |

原生 inbox 中的 `@`、`$` 文字保持字面量。后续 UI/runtime、P1 验收继续核对选择收件人、编辑草稿和实际递送的完整交互。

### 构建身份与尚未完成的工作

完成回执记录五件本地 ELF：库测试 fixture、`ssmcp`、`ssmcp-frontend`、`ssmcpd`、`vt_snapshot`。五件均记录 Cargo `fresh=false`，并记录实际链接产物与私有副本的字节数、SHA 一致；源码/依赖前后保护检查通过，九项记录输入保持不变。此次独立复核覆盖原始回执及其绑定字段；五个大型二进制的字节核验采用已保存的构建回执。各件大小和完整 SHA 已列入 [事实表](gate-checkpoint-U-r12-0309.json)。

上述五件 ELF 覆盖本地构建；六平台正式发行包由后续发布流程生成并验证。**U 的独立 UI/runtime、P1 验收、正式发布与安装验证仍待完成**。性能 A/B 数据和历史测试结果继续按各自源码版本归档。

### 原件与复核范围

本次全文读取完成审阅记录、两层退出回执和 17 项映射；对 1,282,781 字节的最终 Gate 原件做全文 hash 核对，并选择读取源码身份和全部 35 项 checks，嵌入记录保留在原件中供后续按需复核。原始库日志为 557,741 字节，全文 hash 匹配后逐项核对上述结果与外层摘要。

17 项映射生成于 02:50:35，当时总 Gate 尚在运行，其中记录的中间回执 hash 保持历史含义。当前“Gate 完成”依据为最终原件 `cf17822a03c045fff67b2015e196fee0e1edaf45df55b0761331080eac496000`，不回写旧快照。所有原件的完整字节数、SHA 和复核范围见 [gate-checkpoint-U-r12-0309.json](gate-checkpoint-U-r12-0309.json)。此阶段执行证据读取和报告准备。

## 新发布核验器：把请求、构建与公开文件连接起来

手动从 main 发起发布时，需要分别记录两个提交：**S** 是产品与版本标签指向的源码；**P** 是本次 main 上的工作流定义。核验器会先检查这两个身份，再逐个检查工作实际检出的源码及其产物，使最终公开文件能够追溯到已验收产品。

[独立公开目录](https://github.com/LIghtJUNction/ssmcp/tree/9f46b2af474b443abe39f64c2f5651f783ef1321/docs/reports/command-await-delivery-20261120/publication-verifier-manual-main-unbound) 已包含三份代码、完整差异、使用说明、capture schema 和两份准备记录，共八个文件。三份代码只完成语法与独立静态审阅，**尚未运行核验器**；最终 S、P、run 和原始 dispatch 回执仍为 **UNBOUND**。

核验器已经实现两条检查链：

1. 原始 dispatch 命令、输入和 HTTP 返回值，与独立执行回执共同绑定唯一的原始运行。
2. 固定运行中的 job 日志、源码检出、Actions artifact ZIP、包内文件和公开 Release 文件逐层对应。

旧核心合同继续保留：19 个精确 job、18 success 与 deployment skipped、16 个发行资产、四个 gitlink、完整源码文件集合，以及官方 crate 和两类依赖图。Windows sidecar 按生产端的 CRLF 处理，Unix 按 LF 处理。具体检查规则及信任范围见[使用说明](publication-verifier-manual-main-unbound/README.md)。

八份公开文件均已逐字读回，旧 v25 核验器原字节保留。正式使用时还需绑定实际运行，并取得完整日志、有效 artifact 和公开产物。

## 接下来的验收顺序

先完成 U 的独立 UI/runtime 与 P1 验收，取得与本次源码及构建产物对应的实际结果。随后由发布执行方固定最终 S 和 main 工作流 P，保留真实 dispatch 回执，完成六平台流程及公开产物核验。安装验证在正式产物确认后进行。

正式工作流和安装操作继续由发布执行方在对应验收阶段推进。当前会话继续完成上述已授权工作。

## 结论与证据

| 结论 | 证据 | 当前状态 |
|---|---|---|
| U 的本地 Gate 已完成 | [35 阶段与退出回执事实表](gate-checkpoint-U-r12-0309.json) | 已通过 |
| 17 项原始目标有逐项库测试结果 | 同一事实表中的完整测试名、原日志行号与摘要 | 已逐项核对；属于 2,050 项子集 |
| 交互脚本准确检查已选择的 Next caller | [U 的精确提交差异](https://github.com/LIghtJUNction/ssmcp/commit/784370b5fe1b817c788273a5f7b1ab826c743965) | 源码审阅完成；实际交互验收继续 |
| manual-main 核验器已交付 | [固定提交下的准备清单](https://github.com/LIghtJUNction/ssmcp/blob/9f46b2af474b443abe39f64c2f5651f783ef1321/docs/reports/command-await-delivery-20261120/publication-verifier-manual-main-unbound/preparation-manifest.json) | 静态完成，运行绑定待补 |
| 正式发行与安装可验收 | 后续真实运行与公开产物 | 待完成 |

## 五项文稿复核

- **改动价值：** 本页明确说明收件人断言修正和发布身份核验各自解决的问题。
- **表述清晰：** 本地 Gate、真实交互、正式发布和安装分阶段标明状态，S/P 的含义已定义。
- **实测支撑：** 通过数来自本次原件；17 项子集、嵌套摘要与显式 ignored 测试按各自范围计数。
- **评价完整性：** 当前待执行项目列在验收顺序中，完成后再追加实际结果。
- **方法可靠性：** 核验器依赖已审阅配方、固定 actions 和 GitHub runner；独立原始执行回执与实际文件字节用于建立可复核关联。

[Google Docs 当前报告](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit) · [上一阶段完整历史](https://github.com/LIghtJUNction/ssmcp/blob/7cc6e79e85f134cf62993089d31969d06050e3bd/docs/reports/command-await-delivery-20261120/README.md)

# SSMCP 当前迭代：三项夹具与 daemon 场景已通过，1d6 等待最终构建

**更新至 2026-11-22 01:44:05 UTC。** 当前候选为 [1d6b5532](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc)。原先阻挡本机流程的 Questions、Interactions、relay-composer 已分别完成完整候选场景，daemon 在使用现有真实用户 bus 后也已通过。三份脚本的文本已纳入当前候选，运行使用的是 12a/V29 产品二进制。

**1d6 的 V31 最终构建描述符尚未返回。** 执行侧正在等待全局 `rustc-serial` 锁，并只读核对持有者归属，没有停止未知任务。新六平台 CI 尚未发起；正式 Release、H2 发布核验和独立服务器升级均未执行。

## 源码变更与验证来源

| 来源 | 相对父提交的改动 | 当前证据 |
| --- | --- | --- |
| [12a84780](https://github.com/LIghtJUNction/ssmcp/commit/12a847803e952420e7b4534f11fd57ae64ee5c32) | 修正 unified 的空启动清理事件匹配，追加发布说明 | V29 构建、库、账户核心、unified 和 dashboard 已有实际通过结果 |
| [083ef018](https://github.com/LIghtJUNction/ssmcp/commit/083ef018e13cdf6672637d41ea1bd13542185d5a) | 仅修改 Questions、Interactions、relay-composer 三份脚本，53 行新增、6 行删除 | 三份完整候选分别通过；没有 Rust、依赖、发布配方或发布说明改动 |
| **1d6b5532** | 恢复 Questions、Interactions 的可执行模式：100644 → 100755 | 两文件 blob 完全相同、内容零变化；relay-composer 保持原样 |

当前完整身份为 `1d6b55320fff6533663dad2627891960b3b5c5cc`，tree 为 `fc28406bb20a3d1f6f9441ca34016357ab0ff39c`，父提交为 `083ef018e13cdf6672637d41ea1bd13542185d5a`。083 上传时误改了两份脚本的可执行位，1d6 用正常后继提交恢复。静态树与 blob 比较确认了这次纯模式修正；最终执行仍按 1d6 单独绑定。

## 新取得的完整场景结果

| 场景 | 结果 | 实际运行来源 |
| --- | ---: | --- |
| Questions | **43 项通过** | 12a/V29 二进制＋完整候选脚本；该文本已纳入 083，1d6 保持相同字节 |
| Interactions | **172 项通过** | 同上 |
| relay-composer | **46 项通过** | 同上；1d6 未改该脚本 |
| daemon | **182 项通过** | 12a/V29 程序，测试环境使用已存在的真实用户 bus；回执为 `cleanup_confirmed: true`、`errors: []` |

这些是实际完整场景结果，覆盖本轮已知的本机阻挡；当前尚未汇成 V31 的整套执行成绩。原失败回执仍保留，后继成功不改写原运行。

三份夹具修正保留了原有成功与拒绝条件：

- **Questions**：返回普通输入时，核对真实视图、输入焦点、可见光标及准确的待答数量。存在新 pending 提示时不再用旧标题坐标判定是否返回；无待答问题的旧分支继续保留。两次非法消息请求仍须被明确拒绝，错误参数说明同步包含 `finish_task`。
- **Interactions**：先取得明确的 `collab.bind` 回执，再核对原真实用户答案恰好交付一次。pending、delivered、答案来源、草稿和无额外键盘输入等断言保留。该场景现在验证显式绑定路径。
- **relay-composer**：复用已准备的私有 Git 符号链接，同时严格核对其解析后的身份等于实际安装的 Git，解决重复创建链接的 `FileExistsError`。

daemon 的前一次尝试在环境前置处被阻塞，执行了 0 项断言：私有 `XDG_RUNTIME_DIR` 隔离了既有用户 systemd bus。随后使用已核实的真实用户运行环境完成 182 项检查，并确认清理无错误。前置阻塞与后续成功分别记录。

## 已有通过项仍按实际来源记账

### 12a/V29

严格格式与 Clippy、候选构建、**2,079 项常规库测试、另行执行的 2 项必需隔离检查、9 项 wire 检查**已通过；其余 11 项 ignored 未执行。账户核心流程通过，完整 **unified 为 105 项检查，原 unit 为 4 项，dashboard 为 80 项**。

原 UI 十个叶场景中，以下七个原场景已取得通过通报；另三项使用上一节所列完整候选脚本完成：

| 原 UI 叶场景 | 已取得结果 |
| --- | --- |
| preview | 通过；本快照不补写未取得的计数 |
| canvas | 72 项通过 |
| composer | 87 项通过 |
| history | 通过；本快照不补写未取得的计数 |
| presentations | 57 项通过 |
| context | 71 项通过 |
| filepaste | 145 项通过 |

七项完成状态已由本轮执行协调者在 01:24 的进展更新中确认。这里保留“原场景”与“候选脚本场景”的来源差别，等待最终候选的统一描述符。

### eb319/V28

下列先前结果继续有效地属于 [eb319166](https://github.com/LIghtJUNction/ssmcp/commit/eb319166ec8d09deafe4a0da4e6bbe2e04b3705c)：

| 范围 | 已取得结果 |
| --- | --- |
| 原有 CLI 矩阵 | 12 组全部通过 |
| worker 双池 | 202 项检查、7 次真实 cgroup 观察通过 |
| Agents 面板 | 55 项检查、5 组真实 PTY 快照 |
| 用户等待 | 189 项检查通过，属于 CLI 矩阵细分 |
| OAuth 补充 | 19 项检查、7 次 CLI 调用通过 |
| 文件管理与清理 | 27 项检查、9 次 CLI 调用通过 |
| 其他补充 | gap、007-prefix、update 共 3 组通过 |

worker 使用合成执行器验证真实资源限制、队列及清理；快照、CLI 调用次数和检查数各自列示，不相加成为总测试数。最终 worker、Agents 面板与 OAuth 等绑定仍按既有协调顺序，等待当前候选的构建交接。

更新场景当时返回 `kept_newer`：候选 0.10.26 高于公开的 0.10.20，工具仓库为 current。该次保留了较新客户端，没有下载替换二进制。

## 当前等待的是编译锁

083 的 V30 曾在 180 秒的准备等待中超时。随后离线 fetch 仍在等待全局 `rustc-serial` 锁，执行侧通过锁记录确认有真实持有者，并继续只读核对进程树及所属任务。因此，前次超时不能直接归因为网络故障。

当前工作树已选择干净的 1d6，V31 使用独立运行目录；原 V30 运行及超时回执保留。此快照尚无 V31 最终产物描述符，也没有新增 CI 请求。已协调将六平台 CI 与本机排队并行推进，本机原有验收继续保留。截至 01:44:05 UTC，GitHub 查询尚未返回匹配 1d6 的运行。

## 六平台 CI 与审计失败历史

最近已完成的六平台运行仍为 [44967293207，attempt 1](https://github.com/LIghtJUNction/ssmcp/actions/runs/44967293207)，来源 eb319：**四个 Windows/macOS 作业通过，两个 Linux 作业在统一执行与审计隔离步骤失败**。

原失败后的诊断在保持旧判定不变的情况下，提前保存了 37 条真实记录：1 个完整的自有空启动清理事件，加上 36 条 code_mode 事件，对应本客户端的 8 个 run。12a 随后只接受这一完整匹配的合法启动事件，保留客户端与 run 的隔离断言，并已完成本机 unified 105 项及 unit 4 项通过。

旧 CI 失败结果保持原样，新六平台运行尚无结果。更早的 508 工具清单失败和 CLI 9 通过／3 失败，也保留在[历史报告](https://github.com/LIghtJUNction/ssmcp/blob/79d5ec9854aeeeecf082128ce2004b8929ce0286/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)。

## 产品范围保持不变

本轮后续提交只处理验收脚本及其模式，以下产品内容继续按既有源码和对应回执报告：

- **消息、引用与人工结束**：文件缺失时保留原文和警告；原生消息保持字面正文及精确收件身份。交付回执与处理完成分开。仅匹配且已消费的真实 Human Yes 和最终 `may_finish: true` 允许结束；有界等待未获批准时要求新的阻塞续等。
- **worker 与工具仓库**：worker 使用 Linux 用户 systemd、cgroup v2 和同系统用户的已注册槽位计算资源上限，作用于池拥有的进程树。普通 Git 工具仓库支持 RC 和用户覆盖，更新保留本地修改，宿主身份与授权边界继续生效。
- **文件登记与清理**：默认临时保留时间为 24 小时、容量阈值为 512 MiB，支持自定义规则及启动、周期、reload 调度；普通用户文件、持久输入、未知文件和有效活动租约对象保留。先捕获再核对身份、无覆盖恢复的规则没有改变。
- **门户与支付**：简约首页、三步入门、中英文分层指南、真实价格与订单状态已在源码中。既有 1,440 px／390 px 视觉证据来自匿名或合成页面；代码一致性把其关联到后续来源。支付通知核验后才入账，页面回跳不确认付款，跨月到账仍需管理员核对退款。
- **独立 Zuse CLI 与输出展示**：桌面和浏览器操作继续交给独立 CLI，SSMCP 保留安装与版本管理。执行输出按实际返回页展示，较长输出通过游标或归档读取。

三批门户提交、文件身份修复、历史工作区归档清理和各来源的回执摘要，继续保留在[上一份完整当前报告](https://github.com/LIghtJUNction/ssmcp/blob/bba557f2c710fe3b0e65883db29da8573e2e7014/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)，本次不重复展开全部实现和旧日志。

## 发布说明、后续文档与服务器升级

[0.10.26 发布说明](https://github.com/LIghtJUNction/ssmcp/blob/1d6b55320fff6533663dad2627891960b3b5c5cc/docs/release-notes-0.10.26.md) 已在 12a 追加 worker、文件清理、工具仓库、finish_task、门户及付款体验；083 和 1d6 都未改它。原 9,447 字节保留，新文件为 13,772 字节，完整读回摘要仍为 `1a0b81c5c54c64d5ae6cc9bbab47f068d410663b07024705e9231de90378ec5f`。

另外两篇 session 文档的历史示例清理继续放在正式发布后处理。静态核对确认，当前公开工具目录为 `list`、`code_mode`、`collab`、`await`、`messages` 五项；旧 invite／SSH 说明需要清楚标识退役状态，不能暗示仍可执行。后续将按届时的实际文档增量修订，保留期间新增内容；当前候选不为此修改。

H2 准备仍停留在静态来源核对，没有 import 或执行。六份发布配方及相关 Cargo、发布说明文件沿后继差异保持，正式发布 S/P 与 Release run 尚未绑定。独立服务器升级已获授权，尚未执行；Release 工作流内的服务器部署开关仍关闭。升级前客户端 0.10.25、服务器 0.10.20 和已有凭据查询成功的基线仍保留，当前没有新版线上安装结果。

- [当前候选 1d6b5532](https://github.com/LIghtJUNction/ssmcp/commit/1d6b55320fff6533663dad2627891960b3b5c5cc)
- [三份夹具修正 083ef018](https://github.com/LIghtJUNction/ssmcp/commit/083ef018e13cdf6672637d41ea1bd13542185d5a)
- [上一份完整阶段报告](https://github.com/LIghtJUNction/ssmcp/blob/bba557f2c710fe3b0e65883db29da8573e2e7014/docs/reports/command-await-delivery-20261120/current-iteration-unified-source-and-cli-matrix.md)
- [结构化事实与回执身份](unified-source-and-cli-matrix-public-facts.json)
- [Google 进展文档](https://docs.google.com/document/d/1Nq6DQk1EteNRucZo8J5RvhPI8PVUaans4wLgAaWrTkY/edit)

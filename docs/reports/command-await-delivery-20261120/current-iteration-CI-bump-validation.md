# SSMCP 当前迭代：CI bump 与 Git 同步完成，最终候选已构建

**更新于 2026-11-21 18:01:30 UTC。** 固定源码 [4f73f56](https://github.com/LIghtJUNction/ssmcp/commit/4f73f5684279ecae1a8e8486e4332c20bdbaab95) 的完整库回归、发布脚本检查、Linux／Windows GNU Clippy 和候选构建已通过。开发机主仓库已实际同步，独立验证工作区保持干净。**新的交互、真实 Zuse 下载与安装结果仍待收齐；本轮目标继续为 v0.10.25 GitHub release。** [阶段事实与回执身份](ci-bump-validation-and-git-sync-public-facts.json)

## CI 的版本号入口

[版本号工作流](https://github.com/LIghtJUNction/ssmcp/blob/4f73f5684279ecae1a8e8486e4332c20bdbaab95/.github/workflows/bump.yml) 支持 `none`、`patch`、`minor`、`major` 和 `custom`，默认使用当前版本。另有 `dry_run` 预览与 `publish` 选择。`none` 可以准备标签并发布当前版本，不表示完全没有写入。

[准备与触发逻辑](https://github.com/LIghtJUNction/ssmcp/blob/4f73f5684279ecae1a8e8486e4332c20bdbaab95/scripts/bump_release.py) 从工作流最初的 `github.sha` 计算版本，重复运行复用同一版本提交与标签，不从新的 main 再递增一次；只允许正常快进，并拒绝移动已指向其他提交的同名标签。

Release 请求显式包含标签、`build_only=false`、`deploy_server=false`。同一个 Bump run 的后续 attempt 会在 POST 前拒绝自动重发；首次请求失败或响应不确定时保留原始请求与 HTTP 输出。这是 helper 层的单次调用约束，不能写成网络端保证只执行一次。新增 12 个测试定义已进入本轮实际通过的 **118 项发布脚本检查**；dispatch 部分使用模拟响应，本快照没有真实 Bump Actions 发布的运行成绩。

## 固定源码已完成的检查

| 范围 | 实际结果 |
|---|---|
| Node | 64 项通过 |
| 发布与版本号脚本 | 118 项通过 |
| 部署策略脚本 | 35 项通过 |
| 完整库回归 | 私有组 1971 通过、0 失败、12 忽略、6 过滤；host 组 6 通过；必需 code-mode 组 2 通过 |
| 严格 Clippy | Linux、Windows GNU 的 all-targets／`-D warnings` 均以 0 退出 |
| 候选构建 | 三份生产程序和 VT 解码器全部完成；实际构建回执已由原验收协调方独立读回 |

新客户端为 **14823632 字节**，SHA-256 为 `f87fa3eb4afb6fbe7f9e93773a1c0bbcc775d23f941b8f182a6cc8932bc3b1d9`。其余三个产物虽然与旧候选字节相同，仍记录为本轮真实构建产物；不能据此推定新候选的产品交互已通过。

这些结果按各自原始范围列出。Windows GNU 项是目标编译检查；各测试组不拼成可能重复计数的总数。前一轮 `2c94bac` 的目标保存测试曾在完整回归中超时，原记录保留。同一程序的定向检查随后在 0.13 秒内通过，最终源码的完整库回归也通过；原预算、并行数和断言均未改变，**当前未证明旧超时的根因已修复**。

## 开发机 Git 仓库已实际同步

执行侧先正常 fetch，再将主工作区 `main` 从 `02aa4dc` 快进到 [3730c03](https://github.com/LIghtJUNction/ssmcp/commit/3730c039ec15a33af7bb3b56a9811e5649a99f85)。此次未递归更新子模块或自动暂存用户修改。三份本地依赖仓库的 HEAD、差分和未跟踪文件清单哈希前后相同，用户已有文件及修改得到保留。原验收协调方已独立读取实际同步回执。

退役的 `crates/zuse` gitlink 移除后，其本地目录保留为未跟踪目录。独立验证工作区固定在 `4f73f56`，状态干净。这里记录的是实际主工作区快进，单次远端引用刷新另行保留。

## exec.read 展开正文的新增反馈

用户最新截图要求展开后显示具体读取内容。固定候选已包含当前 `exec.read` 返回页的 stdout／stderr 正文、游标、EOF、截断标记和任务状态；两路输出与元数据共用 128 KiB 本地预览上限，原始工具返回保持原样。

该实现针对已核实的原生 `exec.read` 成功结构化响应，并更新活跃连接中的对应请求行。它显示本次读取，不自动补回历史事件或取得整个任务日志。静态复核确认相关四份源码与此前合入主线的实现一致；尚未拿到截图对应的运行二进制与原始工具响应，因此本报告不把截图单独判成旧版故障，也不声称当前现场界面已经更新。

## 下一步与发布边界

新的候选包含下载 TLS 修复、等待详情与终端审计界面的测试适配。继续完成既定交互及生命周期验证；此前四组端到端通过、两组失败和真实下载 panic 保留在[上一阶段报告](current-iteration-CLI-e2e-and-CI-bump.md)。终端结果按实际输出／游标隔离、选择恢复等合同记录，不宣称每个 PTY 的当前 VT 画面均已验证。

六份正式 Release 配方在固定 `4f73f56` 上的字节数、SHA-256 和 Git blob 身份全部与原 pins 一致。Bump 的两个 job 属于另一个工作流，不加入 Release 的 19 个 job；106→118 的测试数量变化也不会被现有 H2 固定规则拒绝。[H2 静态副本](publication-verifier-git-source-zuse-cli-unbound/README.md) 无需因此改写，仍待最终源码、原生请求、实际运行和公开文件的绑定与执行核验。

本轮继续计划 **v0.10.25 GitHub release，不部署服务器、不发布 registry crate**。目前尚未取得这份候选的新版本发布完成回执；Bump Actions 的回执也不会冒充本次原生发布的执行回执。旧失败、清理和历史权限／EOF／H2 附录保留其原始归属。

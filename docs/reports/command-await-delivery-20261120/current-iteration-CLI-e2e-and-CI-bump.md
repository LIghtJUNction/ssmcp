# SSMCP 当前迭代：主线已合并，修复端到端与下载问题

**更新于 2026-11-21 17:30:25 UTC。** [PR #174](https://github.com/LIghtJUNction/ssmcp/pull/174) 已在 17:14:53 UTC 合入主线。候选发布程序已成功构建；端到端六组检查为 **四组通过、两组失败**，安装包／安装器检查通过，真实 Zuse 下载另发现一项 TLS 配置导致的 panic。三项修复已提交到后续分支，新的固定源码已开始重新构建，等待受影响候选的实际验证回执。**本轮继续完善 CI 的版本号 bump 流程，目标为 v0.10.25 GitHub release。** [本阶段事实与回执身份](cli-e2e-and-ci-bump-public-facts.json)

## 当前源码与已完成功能

主线为 [`3730c03`](https://github.com/LIghtJUNction/ssmcp/commit/3730c039ec15a33af7bb3b56a9811e5649a99f85)，其源码树与已测试的 `83b5bc5` 完全相同。此次是单父 squash 合并，`83b5bc5` 并非新主线的祖先；因此保留原始测试的源码归属，不把树相同写成新的执行结果。报告组与原源码负责人均未执行此次合并。

主线已包含独立 Zuse CLI 的版本／下载／安装／卸载管理、旧控制兼容入口移除、红色等待提示与剩余倒计时、绑定请求的 Human 结束决策，以及 `exec.read` 当前批次输出展开功能。此前库、脚本、Clippy 和集成检查的具体范围保留在[回归阶段报告](current-iteration-CLI-regression-83b5.md)。

## 这轮实际验证结果

| 范围 | 已确认结果 |
|---|---|
| `83b5bc5` 候选构建 | release／opt z／test=false 构建完成，客户端、服务端、frontend、VT 解码器四份产物身份已固定，原验收协调方独立读回 |
| 六组端到端检查 | 四组通过、两组失败；任务以 1 退出，输出均结束，原验收协调方已完整读取 |
| 等待用户测试 | 红色、粗体、窄屏和普通交付等前 90 项检查通过；随后 `/wait` 中精确超时条目可见性断言超时，结束决策分支尚未到达 |
| 终端测试 | 旧 fixture 期待 Shell／F7 切换界面，实际已是 CODE_MODE 审计界面；主 PTY 已创建，后续行为断言需要按保留接口继续验证 |
| 安装包／安装器 | 实际检查以 0 退出，原验收协调方已独立读回完整输出与回执身份 |
| Zuse v0.4.2 生命周期 | version-before 正常；download 以 101 退出，整个任务以 1 退出，尚未取得这一组通过结果 |

这些结果属于 `83b5bc5` 的实际候选。报告组没有另行执行产品或 Cargo；新修复的通过结论将以重新构建后的实际回执为准。

## 已提交的三项修复

- [`56a1a5a`](https://github.com/LIghtJUNction/ssmcp/commit/56a1a5ab44185c3fd5354d175f8fe1921ccccab6)：在 `/wait` 检查前补上真实内容焦点和选中操作，让长条目的目标行进入视图；保留同一帧中的精确等待 ID、超时状态和原预算断言。
- [`88e6c33`](https://github.com/LIghtJUNction/ssmcp/commit/88e6c3316522555f0cd7fb8fbfe4c3d56c202209)：真实下载触发了 reqwest／rustls 未配置加密实现的 panic。下载函数改用项目统一的 `http_client::builder()`，其余 HTTPS、超时及下载限制配置保持原位；实际下载是否恢复仍待重建验证。
- [`2c94bac`](https://github.com/LIghtJUNction/ssmcp/commit/2c94bacb2013e9a5c4d4dd13bcbeea2366252525)：终端 fixture 对齐当前审计界面，使用明确的 PTY 选择、读写、关闭接口验证独立输出和游标；同时保留隔离、恢复、归因、后台任务与 termios 相关断言，检查 F7 Copy 与 F6 焦点导航。

报告组已读取这一组固定源码差分。后续分支当前为 `work/zuse-cli-ui-fixes-20261121`；该组修复尚未在本快照中取得新的完整运行通过结论。

## CI bump 与发布边界

新增要求是发布新版本，并完善 CI 的版本号 bump 支持。原负责人正在修改现有 `bump.yml` 的版本入口和固定基线逻辑；本快照尚未读到最终 CI diff。本轮仍计划 **v0.10.25 的 GitHub 发布，不部署服务器、不发布 registry crate**。

最终 source／tag 准备已撤销，待修复与 CI 流程固定后重新绑定。[`840629d` 的 H2 副本](publication-verifier-git-source-zuse-cli-unbound/README.md) 继续作为未绑定、未执行的静态准备保存。实际版本来源、dispatch 输入、源码与工作流提交关系、job／step 编排将按真实修改核对，旧准备不构成新流程的通过结果。

报告组于 2026-11-21 17:23:09 UTC 实际读取 GitHub latest，仍返回 [v0.10.20](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20)。本轮新版本发布尚未完成。前序失败、清理记录和旧权限／EOF／H2 附录保留原历史归属。

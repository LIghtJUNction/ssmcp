# SSMCP 当前迭代：首批轻量检查通过，清理完成，迁移继续集成

**更新于 2026-11-21 16:06:25 UTC。** 本记录依据固定 `031fc6e` 的首批检查、原执行方的清理回执，以及报告组独立读取的 GitHub 状态。**新候选尚未冻结，也尚无本轮迁移的完整 Rust、产品验收或发布通过回执。** [阶段事实与回执文件身份](cli-first-checks-cleanup-progress-public-facts.json)

## 最新范围

桌面与浏览器主操作继续直接使用 **Zuse CLI**；SSMCP 的 computer 工具仅保留版本查询、下载、安装、卸载。红色等待提示、符号与剩余时间显示，以及结束前绑定具体请求的 Human 决策流程继续集成。

最新用户反馈要求：**仅移除默认 `Super + Alt + C` 快捷键，保留现有显式控制开关，不动正在直播的机器。** 最终源码需按这一范围对齐；先前分支中撤掉显式开关的做法不作为最终已确认范围。

## 首批真实检查及当前编译状态

原执行方回报，以下五个现有 Node 脚本在固定 [`031fc6e`](https://github.com/LIghtJUNction/ssmcp/commit/031fc6ed108d0642793b60f099992574b62a5de4) 上均以 **0 退出**：

- `test_cua_facade.mjs`
- `test_client_capabilities.mjs`
- `test_execution_helpers.mjs`
- `test_rtk_helpers.mjs`
- `test_tools_search.mjs`

原验收协调方随后只读复核了回执的哈希、源码／树身份和这五个退出码。报告组没有重新运行检查，也没有把五个脚本等同于五个测试用例或完整验收。

最后收到的 Cargo 准入回执是 **共享编译所有权锁等待后阻塞，Cargo 尚未执行**。原负责人正在与占用方协调交接。这是启动条件受阻，不能记为 Rust 测试失败，也不能记为编译通过。

## 清理结果

原执行方的两批回执记录删除 **65＋7＝72 个已确认闲置的标准构建缓存子目录**。路径 `du` 累计为 **127,444,672,512 字节，约 118.7 GiB**；最后一份回执于 **15:49:28 UTC** 记录磁盘可用空间 **461,121,187,840 字节，约 429.5 GiB**。路径累计占用不等于净释放量；并发文件变化及统计口径应分开看待。原负责人报告共享 target 与源码 worktree 均保留。

另有 **12 个远端已合并分支、2 个本地分支**的删除回执。报告组于 **15:57:36 UTC** 独立读取完整 GitHub 分支列表，确认所报的 12 个远端名称均已不在列表中，当前为 **50 个分支**；`main`、源码迁移工作分支和当前报告分支仍存在。本地分支与缓存清理结果保留执行方回执归属。

## 接下来完成什么

源码静态审查已核对 Windows ZIP、PATH 入口及退役快捷键的对应修正；截至固定 [`c7eebfd`](https://github.com/LIghtJUNction/ssmcp/commit/c7eebfd25fa4b1a3e83ebc5a7cd07eab1bd0cc39)，已读取 finish 取消前的调用方检查、完成回执及代次绑定修正，先前遗漏的 Rust 结构字段也已补齐；这些是源码层面的核对。仍需收口最新显式开关范围、剩余集成问题、最终锁文件与源码身份，再据实际实现开展必要验证。

报告组 **15:59:39 UTC** 读取 GitHub `releases/latest`，SSMCP 返回的是 [v0.10.20](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20)；本轮迁移尚未取得新版本发布回执。另已独立确认 Zuse 的 GitHub 正式发行包更新到 [v0.4.2](https://github.com/LIghtJUNction/zuse/releases/tag/v0.4.2)，但本报告没有先前 SIGABRT／EPERM 反馈已解决的实机证据。

旧 `02aa4dc` 的 [Gate20 通过记录](current-iteration-Gate20-passed.md)及 N20 里程碑保持原来源，不转移给新源码。旧 H2、内置控制权限和 EOF 附录保留历史归属；最终发布验证需按实际新配方与三个已观察到的库 gitlink 重新对齐。

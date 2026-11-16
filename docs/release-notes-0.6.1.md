# ssmcp 0.6.1 更新说明

日期：2026-11-17（Asia/Taipei）。本页说明源码变更；发布和部署结果以对应工作流与验收记录为准。

0.6.1 基于固定的 0.6.0 提交补充小范围修复，继承统一工具注册与原生转发、独立 Shell、daemon/watchdog 和任务／Git 侧栏。Broker 对外继续只有 `list` 与 `code_mode`，使用方式见[0.6.0 更新说明](release-notes-0.6.0.md)。

## 已验证并继承：Unicode 审计修复

[修复 2f844fbb](https://github.com/LIghtJUNction/ssmcp/commit/2f844fbb38ad98228cb944938b096ad1b5aef696)解决了中文输出触发的 panic：审计代码把分隔符的 UTF-8 字节索引固定加 1，可能落到多字节字符内部；现在按字符边界和实际字节长度定位。

旧源码加同一回归测试已重现故障，确认重新编译后的完整 crate 指定测试通过；首次误用旧缓存的 green 运行仍记录为失败。12 列终端对有效 UTF-8 块的逐字显示、秘密遮盖，以及真实 SSH/MCP 中 stdout、stderr 各 60 字节和后续连接调用均已验证。该修复已包含在 0.6.0 标签源码中，不是本补丁版首次引入。

## 本次修复：OutputStore 首次清理锁创建

清理锁的创建改为独占创建（`O_CREAT | O_EXCL`），仅在 `AlreadyExists` 后不带创建标志打开既有文件；其他错误继续返回。两条路径均保留私有权限、所有者与不跟随符号链接的检查，输出隔离、清理互斥、保留期和配额保持原有契约。

该调整针对 macOS 同名文件首次并发创建的异常 `ENOENT` 路径。[Go 问题 #81246](https://github.com/golang/go/issues/81246)及其[原生 C 复现项目](https://github.com/Soph/macos-openat-enoent)报告了相符的系统调用条件；外部复现不能代替 ssmcp 自身的 red/green。本机格式检查及 20 项 OutputStore 专项测试已通过。测试新增 32 轮、每轮 8 个线程在全新私有目录中竞争首次创建锁，并检查原有锁的内容与 inode 保持、符号链接被拒绝。跨平台结果见 [GitHub Actions](https://github.com/LIghtJUNction/ssmcp/actions/workflows/release.yml)，以固定提交的实际验收记录为准。

## 标签与发布状态

`v0.6.0` 保持指向 `417f3838533abb265fa538ba4d246aa76fc322dd`，不移动或重写。[正式发布运行 43920711939](https://github.com/LIghtJUNction/ssmcp/actions/runs/43920711939)在 macOS Intel 的 OutputStore 初始化阶段出现清理锁 `ENOENT`，22 项测试失败，发布步骤跳过。截至原始记录时间 **2026-11-16T17:48:04.348Z**，GitHub Release 和 crates.io 的 0.6.0 查询均为 404；此前同源码的预检成功不等于正式发布成功。

0.6.1 发布分支从该固定提交起步，只加入必要补丁；修复再合入更新后的 main，保留其他会话的原生计算机控制变更。最终 CI、制品、安装与部署回执将追加到验收记录，不为补写结果回改已打标签的源码。

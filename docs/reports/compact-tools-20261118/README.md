# 默认折叠工具活动：实现与本地验收

日期：2026-11-18。源码提交：`cfaa94eb051cb0967220224ca7aebbbd150ab8ed`。分支：`feat/compact-tools-20261118`。[PR #30](https://github.com/LIghtJUNction/ssmcp/pull/30)。

## 已完成的行为

默认活动区把普通工具输出收成一行，显示调用摘要和实际状态。命令开始时显示 `running`，完成后保留真实退出码；超时、取消和失败分别显示对应状态。关联事件按来源及请求、任务标识合并，并发任务保持独立。文件编辑 diff 始终展开。

点击命令行可以展开和收起输出。内容区获得焦点后，↑/↓ 选择记录，Enter 或 → 切换展开，← 收起。输入 `/raw` 切换到原始展开日志，再输入一次或使用 `/logs` 返回折叠活动。文本筛选搜索保留的详情并展开匹配项。切换显示保留输入草稿。

折叠数据最多 64 条、合计 128 KiB 文本及关联标识，每条详情最多 8 KiB。截断有提示。原始显示继续使用原有有界终端缓存，因此 `/raw` 不恢复已经淘汰的历史；完整输出仍由任务游标或输出归档读取。

## 验证结果

| 检查 | 结果 | 范围 |
| --- | --- | --- |
| `cargo test --locked client:: --lib --quiet` | 312 passed, 0 failed | 客户端与新增折叠状态、并发关联、筛选、输入、滚动测试 |
| `cargo fmt --all`、`git diff --check` | 通过 | Rust 格式与补丁空白 |
| `cargo build --locked --bin ssmcp --example vt_snapshot` | 通过 | 独立 target 中的开发构建 |
| `scripts/e2e_client_ui.py` | 42 checks passed | 实际生产合成器、隔离 PTY、固定示例事件 |
| 人工源码复核 | 完成，发现项已修复 | 完成状态、异常关联、筛选导航及原始输出接线 |

[PTY 机器可读结果](pty-result.json)保存完整检查列表。新增交互检查覆盖默认折叠、退出码、编辑 diff、鼠标展开/收起及 `/raw` 往返；原有检查覆盖浅/深主题、窄窗口、草稿、筛选、审计、成员、队列、缩放和退出恢复。回归脚本改为显式打开审计详情，并按语义识别包含项目名的标题。

图片来自实际 VT 单元格快照，使用仓库的 `render_ui_snapshot.py` 转成 PNG；它们是固定示例，不代表正在运行的生产任务。

### 默认折叠

[默认折叠：命令一行保留状态，编辑 diff 展开](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/compact-tools-20261118/ui-chat.png`）

### 展开一个命令

[展开命令后的输出与保留的退出状态](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/compact-tools-20261118/ui-command-expanded.png`）

### 原始日志模式

[输入 raw 命令后的原始展开日志](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/compact-tools-20261118/ui-raw-activity.png`）

## 候选构建与当前安装

候选程序位于：

`/home/lightjunction/Documents/GITHUB/ssmcp/target/compact-tools-20261118/debug/ssmcp`

该程序是开发构建，源码为上述提交。SHA-256：`10ab5f3dc0336de6fa541ad87f1001882e93a94e12bbf726775646ae55bb739f`。配套 VT 解码器位于同一目录的 `examples/vt_snapshot`。没有以这个候选程序覆盖 `/usr/bin/ssmcp` 或 `~/.cargo/bin/ssmcp`，也没有由本分支执行任何在线客户端重启。

当前开发机另有 0.9.0 更新和 PR #25 的集成工作；本分支基于当时最新远端 main `68d87539e97efce78b0e2b90fdac3c6324f46e01`。这份报告证明该独立分支的本地行为，尚不证明本次功能已经进入正在运行的 0.9.0。后续应由该更新的协调者合入本分支，再按其构建与会话保护流程激活，避免覆盖并行更新。

操作说明见 [客户端 UI](../../client-ui.md#默认折叠的活动视图)。

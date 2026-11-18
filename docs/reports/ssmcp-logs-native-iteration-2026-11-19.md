# ssmcp 日志面板与原生能力迭代记录

日期：2026-11-19（Asia/Taipei）

## 结论

日志面板、相邻 Tasks、共享草稿和外部 harness 显式懒加载已汇入 [PR #39](https://github.com/LIghtJUNction/ssmcp/pull/39)。核心源码检查点为 [b9596e6303628706fd2656bf80c6e4e928dfdacb](https://github.com/LIghtJUNction/ssmcp/commit/b9596e6303628706fd2656bf80c6e4e928dfdacb)。它也包含 [PR #40](https://github.com/LIghtJUNction/ssmcp/pull/40) 的运行时预检与事务锁修复。

本次没有完成当前客户端替换、生产部署或正式发布。新的组合候选仍需要实际构建和完整相关验收；开发机连接及随后当前工作区不可用使这一步无法继续。本文记录的是已确认状态和恢复入口。

## 用户要求与实际落点

| 要求 | 当前代码/入口 | 状态 |
| --- | --- | --- |
| 右键打开独立日志面板 | PR #39 的 Logs 逻辑列，主会话保留用户/助手消息和紧凑工具卡 | 已推送，组合候选真实验收待完成 |
| Logs 右侧 Tasks | 原生任务列表，显示实际 job ID、命令及退出状态 | 已推送；新增夹具严格核对真实任务身份 |
| 横向画布和草稿保留 | 各列保留选择与滚动状态；共享草稿、光标、收件人；在 Logs 输入仍留在 Logs，Esc/右键返回来源 | 已推送并同步旧 UI 断言 |
| 默认只用原生工具 | 五个外部 provider 初始 not_loaded；不扫描 PATH、不运行 --version、不自动启动或轮询 | 已推送，复用 PR #34 的实现 |
| 明确使用时再加载 harness | ssmcp.harnesses.load 只加载选定 provider；并发共享初始化，失败缓存，refresh 显式重试 | 已推送；加载动作本身不发起模型回合 |
| 历史会话元数据与关键词搜索 | [PR #45](https://github.com/LIghtJUNction/ssmcp/pull/45) 的 history.list/metadata/read/search | 独立现有成果，避免重复实现；尚未并入本候选 |
| Markdown 记忆 | 本地 17 文件候选，与 PR #47 另一个候选存在同名 API 分歧 | 未提交、未编译，不能称为交付能力 |

## 验收证据的范围

运行时实际 release 二进制在普通环境及全部打包 systemd 限制下各通过 7 项安装检查，完整 code_mode 模块 33 项与实际 CLI 4 项通过。[具体源码、哈希和边界](ssmcp-runtime-preflight-2026-11-19.md)

运行时分支最后一次 [CI 44372266403](https://github.com/LIghtJUNction/ssmcp/actions/runs/44372266403) 的 macOS Intel/ARM、Windows x64/ARM 四项成功。Linux 两项经过 Rust、真实沙箱、服务限制等门禁后，在过期 readUri E2E 断言失败；修正已进入 b9596e6，尚未取得组合候选最终结果。

本报告所在检查点还保存了三份后续 UI 夹具和 CJK 快照修复。三份夹具按此前完整读取的差异恢复，计算所得 Git blob 哈希分别为 c737709fa2e1df9c3a0e35b49a317cb25a853b2d、cacabe61e96e5dedf5262688a313178676b02cf0、242d12132a061f1e79e6713f667b18efa17e55db，与失联前 git diff 显示的目标一致。

三份后续 UI 夹具（composer、questions、input_history）在环境失联前完成 Python AST、--help 和 scoped diff --check，未发现确定缺陷。这些检查只验证语法与静态状态流，不能替代真实 PTY 验收。它们保留了收件人选择、持有编辑、问题表单、恢复会话和实际 Tasks job 的覆盖。

[PR #43](https://github.com/LIghtJUNction/ssmcp/pull/43) 记录 b8c672a3672f8dbef14b3ac67c864a6330556b01 的 590 个 client/context 测试和 67 个真实 PTY 检查通过。Git 树比对确认该版本与 b9596e6 的 src/client/terminal.rs 和整个 src/client/terminal/ 内容相同，但 backend、client 入口、历史/上下文和其他文件不同。因此这些结果只是明确范围的既有 UI 证据，不是当前组合树的全套通过证明。PR #43 后续配额状态栏也不能继承旧计数。

快照渲染的 CJK 修复来自 PR #43：先画完所有单元格背景，再绘制字符，防止相邻格背景覆盖宽字符。该改动属于验收图像生成，不改变终端产品逻辑。

## Markdown 记忆：需要先统一契约

本地候选已经写出四个原生操作、JavaScript helper、标准 Markdown 文件头、条件更新、私有目录与工作区边界、磁盘任务调度和有界搜索。list/read/search 被归类为观察操作，write 保持交互语义；源码包含 14 个模块测试、2 个代码模式测试和端到端夹具扩展。没有成功执行的编译或测试结果可以报告。

[PR #47 的接续记录](https://github.com/LIghtJUNction/ssmcp/blob/93d59cb64e56151b02402e1cc085221940388069/docs/reports/session-platform-20261119.md) 另有开发机提交 437ccdbfabbda1bbe9ac20dd46de7a060f5b2034，尚未取得其完整远端源码。

| 契约 | 本地候选 | PR #47 记录的候选 |
| --- | --- | --- |
| 文件标识 | id | name |
| 创建 | title/content，省略 ID | create_only |
| 更新保护 | expected_sha256 | revision / if_match |
| 默认作用域 | 连接主工作区 | 经过验证的线程 |
| 共享 | 工作区固定范围 | 可显式选择 session/workspace |
| 文件格式 | ssmcp.memory.v1 TOML 头 + Markdown | 恢复源码后确认 |

两套同名工具不能同时注册；线程绑定调用也不能静默降级为工作区共享。恢复后应先保留原工作树、核对实际提交，再统一参数、格式迁移和默认作用域。

环境失联前，根代理读到了补丁清单，记录 17 个文件及 81,990 字节补丁，预期 SHA-256 为 404a9f61f73fa6cc6903f96d79ed54056dbe182118a7442110b3cae670528c3d。[恢复清单](ssmcp-native-memory-recovery-2026-11-19.json) 只保存文件哈希和位置，**不包含补丁源码，不是代码备份**。原始工作树为 ssmcp-native-memory；补丁名为 native-memory-20261119.patch，清单名为 native-memory-20261119-manifest.json。工作区恢复后必须先验证实际文件。

## 恢复后的最短完成路径

1. 读取开发机 .local/logs-acceptance-b9596e6-20261119 的结果与日志，以及原任务和 retry1 单元状态。首次任务在 Git 拉取阶段超时；重试请求没有得到最终结果，不能直接重放。
2. 以 PR #34 的 native-first 0.10 候选为集成基线，纳入 PR #39 的运行时和最新验收修正，避免同时采用 PR #43 另一套重复配额布局。先核对各分支当前 HEAD。
3. 合入历史搜索 PR #45；恢复两份记忆源码并统一 API，保留线程、会话和工作区边界。
4. 固定唯一候选，构建并复制确切二进制；完成 Logs/Tasks/草稿、默认零 harness 进程、原生 SSH/HTTP、记忆契约及平台发布门禁。
5. 按已授权发布流程完成客户端交付并核对实际版本，然后同步最终报告。当前记录不代表这些步骤已经完成。

## 位置与同步

- [日志与原生懒加载 PR #39](https://github.com/LIghtJUNction/ssmcp/pull/39)
- [运行时 PR #40](https://github.com/LIghtJUNction/ssmcp/pull/40)
- [0.10 集成 PR #34](https://github.com/LIghtJUNction/ssmcp/pull/34)
- [Google Drive 中已有的上一版运行时报告](https://drive.google.com/file/d/1If0geYoDleeLs9SMG7Ut7rQBlyf5Ol-5/view?usp=drivesdk)

最新 Google Drive 上传尝试返回 no primary turn environment is available。上一版报告未被替换，本次最新状态以 GitHub 记录为准。

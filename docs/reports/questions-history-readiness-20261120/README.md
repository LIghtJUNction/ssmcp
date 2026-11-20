# Questions 与历史恢复验收

本次修复了精确本地收件人准备时序，并修复了在同一 History 页再次打开历史选择器后，Enter 仍落入空输入框的问题。产品改动已通过 [PR #146](https://github.com/LIghtJUNction/ssmcp/pull/146) 合入；这里保存各次原始结果及其源码边界。

## 结果与来源

下表每行代表一次独立运行，不跨批次、套件、平台或提交相加。检查数量沿用夹具原始输出，包含不同重启阶段的准备检查。

| 运行 | 产品 / 夹具 | 结果 |
| --- | --- | --- |
| 初次准备 | `c7086f9` / 原脚本 | 0 项；缺少预编译测试服务路径，属于驱动环境错误 |
| 原 Questions | `c7086f9` / `c7086f9` | 17 项后 `StopIteration`，复现正式预检的收件人准备失败 |
| 修正 Questions | `c7086f9` / `8e76841` | 完整 43 项通过；报告 12.885 秒，外层进程 12.993 秒 |
| 原历史焦点 | `c7086f9` / `8e76841` | 140 项后等待 Enter 恢复超时，保留原始失败报告 |
| 新 Rust 回归首次编译 | `ae8ede2` / `ae8ede2` | `E0616`：测试访问私有字段；没有运行测试或 PTY |
| 修正 Rust 回归 | 测试源码 `4221d55` | 4 项通过，0 失败；1836 项被过滤 |
| 完整 InputHistory | `ae8ede2` / `4221d55` | 完整 150 项通过；报告 32.451 秒，外层进程 32.629 秒 |

[acceptance.json](acceptance.json) 记录完整提交、源码树、命令、构建环境、二进制 SHA256、原始回执 SHA256、各阶段失败分类和证据文件。独立原生目标使用 jobs=1 与串行 rustc wrapper。

## 修复与证据

Questions 和 InputHistory 现在等待精确成员进入本地 steering roster，再通过已有选择器选择该成员，并验证目标是本地成员且没有同名原生收件人。Sessions 中出现一个同名条目不能证明这一独立投影已经就绪。[原失败](questions-original-c708-failure.json) 与 [43 项完整结果](questions-c708-fixture8e.json) 使用相同的 c708 产品二进制。

显式打开 `/resume` 或 `/archived` 列表成功后，产品把键盘焦点交给列表。定向回归通过真实字符输入与提交路径检查同一列内的切换；PTY 保留原有 Home + Enter 操作，没有添加 Tab 或固定等待。恢复后回到输入框的行为保持不变。[完整 150 项结果](input-history-ae8-fixture4221.json) 同时验证原始 JSONL 不变、旧请求和工具不重放，以及新要求只追加一次。

产品二进制构建自 **`ae8ede2a53b802899d57edcfccac933a9909eea3`**，PTY 夹具与预编译库测试服务来自 **`4221d5594b8ee0bd1fb9ea1d9783d0bb1bebd3f7`**。两者唯一差异位于 `cfg(test)` 回归模块；测试编译和 PTY 前后的产品二进制摘要一致。Questions 的 43 项结果仍绑定原 c708 + 8e，没有改记为后续源码结果。

## 真实 PTY 截图

PNG 归档保留原目录 `docs/reports/questions-history-readiness-20261120/`；下列链接指向云盘 ZIP，括号中是该目录内的文件名。完整清单见 [归档清单](../generated-png-manifest-20261121.json)。

[48 列归档恢复](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `archive-restored-48.png`） · [120 列归档恢复](https://drive.google.com/file/d/1rSdpBIFpEt7izp-hbq5RdwCJ5KyI-mDj/view)（归档内 `archive-restored-120.png`）

两张图都来自 **ae8 产品 + 4221 夹具** 的实际 VT 单元格，展示恢复后的原历史、`ARCHIVE_RESTORED_CONTINUATION`、中文与输入框。它们是归档恢复证据，**不代表 407084c 或后续统一 Tools 组整合后的最终 UI 验收**。实现者与独立审阅者均已实际打开两图，内容仅为本地合成夹具；未包含用户参考图片。

## 保留与限制

0 项准备错误、17 项原 Questions 失败、140 项历史焦点失败和 E0616 新测试编译错误分别保留，未合并成产品失败或成功总数。退出后的只读检查没有发现本任务残留进程、InputHistory 临时目录或 Rust picker 临时目录。

本报告不声明最终合并候选、多平台门禁或生产部署完成。正式预检、后续整合和发布由各自的精确源码回执确认；本证据 PR 不修改产品代码或启动新的测试。

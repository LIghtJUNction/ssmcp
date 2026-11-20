# zcalld 原生监听开发分支：构建验收证据

这份材料记录固定 `ef4394ac0928cd53cda5f25862851465ca97f009` 与 compat `be72b3aa3ef17032a360ea5b9a9d38fe40c4b622` 的本机验证。**32 个阶段全部通过。** 它属于尚未发布的监听开发分支，不能用来宣称现有正式 zcalld 0.4 已支持 `listen`，也不是 SSMCP 最终联合验收或发布回执。

## 实际覆盖

| 范围 | 结果 |
| --- | --- |
| Rust 工作区 | 115 通过、0 失败、0 忽略，包含 7 个监听回归 |
| 模型元数据 | 16 / 16，全部实际执行 |
| 上下文压缩 | 2 / 2，全部实际执行 |
| 私有网络测试 | 实际 socket peer 为 `192.0.2.42`，测试在独立网络命名空间内运行 |
| 构建、打包、暂存安装与重装 | 均通过 |
| 源码 | 759 个固定文件，前后清单一致 |
| 产物 | CLI、5 个 Rust 插件和 2 个新构建的 be72 Go helper，实际 SHA 与回执一致 |

本轮没有另外运行完整 `test-native-compat.py` 的所有独立链路；不能将上表改称“全部 native compatibility 测试通过”。它也不覆盖其他分支后续加入的取消或 identity/privacy 修复。实际 Go 工具链为 `go1.27.4-X:nodwarf5`，与正式工作流的 Go 1.26.8 不同。

## 原始材料与校验

压缩包 [native-ef439-gates32-20261120.zip](https://drive.google.com/file/d/1OlScR20aKm_dZ4yLj-Z4tKYvzjmgMKj7/view) 包含原始门禁报告、前后源码清单、metadata/compaction 逐项回执，以及摘要与 `SHA256SUMS`。打包没有改写原始 JSON。下载后可按以下摘要核对：

| 文件 | SHA-256 |
| --- | --- |
| 压缩包（226,264 字节） | `bbbdcce805e23c5cc054708c810e7fe2aac30c3db4f4d3c7c8036588d1740109` |
| `gate-report.json` | `de2cdb6c1226b6e09fa4a563293b6985539a1efd20fc972d186a048551f6c218` |
| `source-manifest.json` | `3b9c97790e500f60c09c6d1e9399614ee2a849c3d3797b3be9b61ac670a81589` |
| 冻结 `zcalld` | `e31740cec831a0fc39de8c8246957359fbabb71979f8cc351f3130ed173db090` |

原始门禁目录为 `/var/tmp/ssmcp-zcalld-ef439-feature-gate-20261120-retry-r2`。同一压缩包也已上传并回读确认至 [Google Drive](https://drive.google.com/file/d/1OlScR20aKm_dZ4yLj-Z4tKYvzjmgMKj7/view)。

## 与最终交付的关系

SSMCP 会话导航测试修复已经通过 [PR #122](https://github.com/LIghtJUNction/ssmcp/pull/122) 合入。该项定向 PTY 诊断与较早的 8c 构建属于各自的历史证据，不能重标为后续候选。

工具组总结、命令展示及最新 Z 依赖收敛后，最终联合验收将使用同一份 SSMCP 源码、对应的已通过构建回执，以及本包记录的 zcalld 冻结产物。最终 18 组交互测试、P1 往返、截图和正式发布结果另行记录。

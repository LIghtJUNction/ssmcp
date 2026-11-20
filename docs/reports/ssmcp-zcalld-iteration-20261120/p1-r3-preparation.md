# P1 终端验收控制器：r3 绑定

状态：控制器准备完成，独立静态审查通过；最终 P1 尚未运行。

本轮 SSMCP 已合入固定提交 [`407084c4fcc582f5fef01b772de838f04e2d5484`](https://github.com/LIghtJUNction/ssmcp/commit/407084c4fcc582f5fef01b772de838f04e2d5484)，源码树为 `f824b238cfa541fb0f8cddcc68e34278f2c62e1b`。实际构建、24 组交互场景、P1 和六平台结果将在完成后另行记录。

## 控制器核对什么

验收结论需要同时对应准确的源码、构建出的程序和实际执行记录。新控制器在启动 P1 前，核对 r3 门禁的完整阶段、原始日志、编译清单、运行工具和正式启动回执；任一来源或结果不一致都会拒绝运行。程序和证据复制到本次独有目录后，执行前后再次计算哈希。

P1 保留原来的四次本地模拟 provider 请求、三个执行回执，以及 120 列和 48 列的真实终端内容检查。新控制器沿用原有成功条件、源归档校验与清理规则，并显式绑定 Node、code-mode runner 和隔离环境。

## 固定代码

| 文件 | SHA-256 |
| --- | --- |
| `run_final_p1_r3.py` | `4550cf311ceca921a0b4e26031fdface4625765e8800832541ba46078915c38b` |
| `gate_r3_binding.py` | `18d13760eebc42e0d83aeffe6ec25e7f95dc6a25c532c86f6f5f5f2f6802e0c4` |
| `native_ef439_binding.py` | `4ff65e1a7d731196ed47779350fca25e482615acd9a9aac0b4f74987061b6541` |
| `gate_inventory_r3.py` | `b1a4c184c58ed32f8efaee34bb5e0636b07611a0dfd55643e1485fb16799c4fa` |

[完整交接包](../../../artifacts/iteration-evidence-20261120/ssmcp-p1-r3-handoff-18d13760-20261120.zip)：76,778 字节，SHA-256 `f7a2144b34b5c804dbd428b5abc4844b2ca3e67227679b1897ce99eb7ee21eb9`。包内含运行清单、输入模板、代码、真实 driver/launcher 参考和逐文件清单。旧 `f1a6…` 控制器与旧归档保留。

## 准备阶段的证据边界

七项纯 Python 契约检查验证了回执与文件哈希处理。最终 helper 仅补充 Cargo target 一致性判断，并完成静态核对；这些结果不计作 Rust、真实 P1 或 Windows 安装通过。[独立审查记录](p1-r3-independent-review.json) 对照了实际 r3 driver `1ad9ec76…`、launcher `99b78f15…` 和已保存的 native gate JSON，未发现具体阻断。

原生监听采用 [zcalld P1 开发分支](https://github.com/LIghtJUNction/zcalld/pull/1)的 `ef4394ac0928cd53cda5f25862851465ca97f009`，compat 固定为 `be72b3aa3ef17032a360ea5b9a9d38fe40c4b622`。已有 32 阶段构建记录只适用于这组原生源码；当前正式 zcalld 0.4 不包含该监听功能。P1 控制器必须同时取得本轮 SSMCP 的真实 passed gate 与完整 launcher exit，才能运行这组开发分支的连接验收。

本准备记录不提供人类结束许可。最终交付后，仍须通过专用结束请求取得当前用户的明确批准。

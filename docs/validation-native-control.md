# 原生电脑与浏览器控制验收

本轮验收源码 `2e5c55b232d2edd726242c38dc390535ac4d5fff` 已通过格式、静态检查、全部目标测试、协议端到端测试和真实 Chromium 无界面测试。开发机上的独立客户端 `ssmcp-native-controls` 已升级到对应二进制，并通过当前 ssmcp 插件重新验证工具目录与能力状态。公开 broker 保持原版本；当前可通过原生 `call` 使用控制接口，`image()` 便捷显示通道与控制命名空间简写需要服务端升级。[最终门禁](reports/native-control-final-gates-20261116.json) · [运行实例记录](reports/native-control-live-20261116.json)

## 证据范围与版本

文档交付以主分支 [5dc49038](https://github.com/LIghtJUNction/ssmcp/commit/5dc490388bca8519fdd6171f09b49699564afcbd) 为基线，保留同期合入的 daemon 清理改动。下列自动化计数与运行实例仍严格对应 `2e5c55b2`，不表示后续主分支源码已经重跑同一整套验收。

验收分为最终源码自动化验证、前一候选的真实桌面验证，以及当前运行实例检查。三者分别保留版本和测试范围，避免把早期桌面结果归入未执行的最终桌面重跑。

| 证据 | 对应源码 | 已观察结果 |
| --- | --- | --- |
| [最终门禁](reports/native-control-final-gates-20261116.json) | [2e5c55b2](https://github.com/LIghtJUNction/ssmcp/commit/2e5c55b232d2edd726242c38dc390535ac4d5fff) | 所有列出的门禁退出码为 0；记录时间为 2026-11-16 15:57:09 UTC |
| [协议端到端](reports/native-control-protocol-20261116.json) | `2e5c55b2` | 143 条断言记录通过，报告耗时 4.539 秒 |
| [无界面浏览器端到端](reports/native-control-headless-20261116.json) | `2e5c55b2` | 45 条断言记录通过，报告耗时 5.908 秒 |
| [真实 Hyprland 桌面](reports/native-control-desktop-20261116.json) | [6f18aba2](https://github.com/LIghtJUNction/ssmcp/commit/6f18aba2c9e2665c734143b357f03115050e545c) | 59 条断言记录通过，报告耗时 8.509 秒；早于最终焦点后验修复 |
| [运行实例](reports/native-control-live-20261116.json) | `2e5c55b2` | 2026-11-16 16:04:24 UTC 观察到服务运行、26 个控制工具及原生状态查询可用 |

上述耗时用于复核单次执行记录，不能据此推断性能分布。端到端数量统计报告中的断言记录，包含认证检查及重复协议验证，不代表同等数量的独立功能场景。最终门禁明确记录 `desktop_input_repeated=false`；最终二进制没有重复执行物理桌面输入。[门禁](reports/native-control-final-gates-20261116.json) · [运行范围说明](reports/native-control-live-20261116.json)

最终客户端二进制的 SHA-256 为 `a50f5f3f50368f6f086150ea9c8ac29cbe8a660e805f9878eb7c5d4b14b1c738`。同期构建的服务端为 `9946901a3c8192a6a9d65eb9cb94da3f5bbde195b1839b5ab1353f366b21eb32`。前一候选桌面测试使用的客户端为 `23b0b235b95fd8700db20bdbd3e59cc4c9dc74a81b73410bdeb409bac8e0c0cf`。报告保留原始记录的 SHA-256、源码文件摘要及二进制摘要，便于区分构建输入。[最终构建](reports/native-control-final-gates-20261116.json) · [桌面构建来源](reports/native-control-desktop-20261116.json)

## 最终门禁结果

| 门禁 | 结果 | 计数口径 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 通过 | 退出码 0 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 | 以警告作为错误执行，退出码 0 |
| `cargo test --locked --all-targets --quiet` | 419 通过、0 失败、20 忽略 | 汇总所有目标；其中库测试为 337 通过、6 忽略 |
| `code_mode::tests --include-ignored` | 16 通过、0 失败 | 独立专项执行，与全部目标测试重叠，不相加 |
| `cargo build --locked --bins` | 通过 | 两个二进制均记录摘要 |
| 协议端到端 | 143 条断言通过 | 真实 OpenSSH 公钥认证、HTTP 与 Linux PTY |
| 原生无界面端到端 | 45 条断言通过 | 真实 OpenSSH、OAuth PKCE、HTTP MCP 与 Chromium CDP |

忽略的测试不计入通过数量。上表由 [最终门禁原始摘要](reports/native-control-final-gates-20261116.json)、[协议报告](reports/native-control-protocol-20261116.json) 和 [无界面报告](reports/native-control-headless-20261116.json) 汇总。

## 已验证的控制路径

客户端内置 11 个 `ssmcp.computer.*` 工具和 15 个 `ssmcp.browser.*` 工具。当前独立客户端的两页目录共返回 73 个工具，其中控制工具 26 个，外部工具列表为空。电脑操作使用桌面系统接口，浏览器操作由 Rust 直接通过 CDP 连接系统 Chromium；控制路径无需编码代理或其他 MCP 服务器。[当前目录与状态](reports/native-control-live-20261116.json) · [接口说明](native-control.md)

最终无界面测试使用 Chromium `157.0.8103.38`。实际页面验证包括独立 profile 启动、标签页发现、观察得到的元素引用、Unicode 填写、空字符串清空、组合键删除、鼠标点击、键盘事件、滚动和截图。测试还覆盖导航后旧引用失效、拒绝未观察引用、拒绝非回环调试地址，以及断开时终止自有浏览器、保留外部附着浏览器。[45 条断言及浏览器版本](reports/native-control-headless-20261116.json)

在最终测试服务端上，截图通过两个受测 MCP 协议版本返回真实图片内容块，验证 JPEG 字节有效、大小受限，且图片编码没有重复写入元信息或文本。原生调用返回完整 MCP 结果，应检查 `isError`，从 `structuredContent` 读取结构化字段。页面快照按完整结果限制为 24 KiB，每个返回字符串最多 400 行；UTF-8 裁剪保留字符边界，并标记正文、元素或元信息的截断。每张截图最多 400 KiB。[截图断言](reports/native-control-headless-20261116.json) · [接口及输出限制](native-control.md) · [最终浏览器实现](https://github.com/LIghtJUNction/ssmcp/blob/2e5c55b232d2edd726242c38dc390535ac4d5fff/src/browser.rs)

前一候选的真实 Hyprland 测试覆盖显示器逻辑几何与缩放、桌面截图到逻辑坐标的映射、Unicode 键入、组合键替换、原生指针点击、拖拽释放、滚轮及不完整坐标拒绝。该次成功对应 `6f18aba2`，报告中的 `final_candidate_desktop_rerun=false` 明确限定其范围。[59 条桌面断言](reports/native-control-desktop-20261116.json)

最终源码补充了 Hyprland 聚焦后验：发送一次聚焦动作后，最多等待 500 毫秒读取实际前台窗口地址，只有目标地址匹配才返回成功。立即生效、延迟生效、不匹配、查询不响应四类真实辅助进程夹具由源码测试覆盖，并随最终全部目标测试通过；这组夹具不等同于在最终二进制上重做桌面交互。[最终电脑实现与测试](https://github.com/LIghtJUNction/ssmcp/blob/2e5c55b232d2edd726242c38dc390535ac4d5fff/src/computer.rs) · [最终全部目标门禁](reports/native-control-final-gates-20261116.json)

## 当前接入方式

开发机独立服务 `ssmcp-native-controls.service` 运行版本化二进制 `target/native-control-runtime/ssmcp-2e5c55b2`。观察时服务处于运行状态，配置 `Restart=always`，重启计数为 0。当前插件重新发现该客户端后，`computer.status` 报告 Hyprland 能力可用，`browser.status` 返回 `native=true`、`protocol=cdp`、`connect_supported=true`，浏览器连接已关闭。[运行实例证据](reports/native-control-live-20261116.json)

先通过插件 `list` 刷新连接目录，再在 `code_mode` 中按固定名称寻找当前连接。连接 ID 随生命周期变化，示例不固定它：

```javascript
const c = clients.find(c => c.name === "ssmcp-native-controls");
if (!c) throw new Error("请重新调用 list 检查客户端");
return {
  computer: await call(c.id, "ssmcp.computer.status", {}),
  browser: await call(c.id, "ssmcp.browser.status", {})
};
```

这条原生调用路径已通过当前公开 broker 验证。该 broker 中 `computer`、`browser`、`image` 仍为 `undefined`；便捷命名空间和 `image()` 渲染通道需升级服务端后使用。本轮升级范围仅为开发机独立客户端。所有本任务创建的浏览器已经关闭。[原生调用与服务端边界](reports/native-control-live-20261116.json)

## 适用边界

| 环境或行为 | 本轮结论 |
| --- | --- |
| Linux Hyprland | 前一候选完成真实桌面操作；最终候选完成焦点夹具与全部目标测试，运行实例状态检查通过 |
| Linux X11 | 已实现适配；本轮没有实机全流程结果 |
| 其他 Wayland 会话 | 依赖会话协议与已安装辅助程序，以逐项状态诊断为准 |
| macOS / Windows 桌面 | 当前适配器明确报告不可用 |
| Chromium 浏览器 | 实际验证系统 Chromium；其他支持 CDP 的 Chromium 系浏览器未在本轮逐一实测 |
| Firefox | 原生协议未接入 |

平台支持范围来自 [接口说明](native-control.md)，实测范围来自 [桌面](reports/native-control-desktop-20261116.json)、[最终无界面](reports/native-control-headless-20261116.json) 和 [运行实例](reports/native-control-live-20261116.json) 报告。

聚焦成功只说明观察时刻目标处于前台；后续物理输入前仍应确认目标窗口。一次手工输入未到达测试页时，观察到桌面焦点独立变化，随后停止了物理输入；该尝试未计为通过，也未据此认定接口参数存在缺陷。屏外截图裁剪会被明确拒绝；有界面页面处于屏外时可能无法继续绘制并导致截图超时，本轮成功的后台截图使用 `headless:true`。[运行中观察](reports/native-control-live-20261116.json) · [操作约束](native-control.md)

## 复现与证据文件

在具备系统 Chromium、OpenSSH 和项目依赖的开发环境，可从对应源码执行下列命令。输出写入新的复验目录，保留本轮原始记录：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets --quiet
cargo test --locked --lib code_mode::tests -- --include-ignored
cargo build --locked --bins
python3 scripts/e2e.py --report target/native-control-evidence/recheck-protocol.json
python3 scripts/e2e_native_control.py --evidence-dir target/native-control-evidence/recheck --report target/native-control-evidence/recheck/headless.json
```

命令依据 [最终执行记录](reports/native-control-final-gates-20261116.json)。物理桌面测试需单独使用 `--desktop`，它会打开临时测试页面并实际输入；本轮最终门禁未重复该操作。[测试入口](native-control.md#验证)

公开 JSON 保留断言、环境版本、辅助程序可用性、来源摘要和构建摘要；未包含截图、私人窗口内容、认证凭据或固定客户端连接 ID。原始运行记录的路径和 SHA-256 随各报告保存，便于核对公开副本与测试输入。

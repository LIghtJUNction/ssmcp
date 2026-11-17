# ssmcp 后台控制、浏览器 setup 与客户端能力验收

日期：2026-11-18

对应 PR：[LIghtJUNction/ssmcp #19](https://github.com/LIghtJUNction/ssmcp/pull/19)

代码基线：已合并 PR #18 的 TTY observer / 0.7.4 与 PR #21 的客户端内存优化，保留两轮已发布到主分支的改动。

最终验收与部署源码：`397a14a5ef18b6444226298284e66ab784af9443`；对应主分支基线为 `a9fbd53c4d52c8e178cbb32defc297defe135a3f`。初轮 `e1e58155` 的证据同时保留，最终复验文件使用 `memory-merged-` 前缀。

## 交付结论

本轮把电脑输入目标与用户当前桌面分开，并打通了后台浏览器的真实输入、等待、截图和关闭流程。浏览器环境可通过 setup 检查、安装和保存；客户端独立提供能力清单、资源和工具目录，服务端保留通用转发接口。

“达到 CUA 水平”在此落实为可验证的操作行为：真实输入到正确目标、返回实际截图、保持用户鼠标与键盘焦点、不隐式切到用户桌面、连接关闭后清理自有进程。这些结果覆盖本次 Linux 开发机与所列浏览器路径；其他平台与复杂网页行为仍需各自的兼容验证。

## 行为变化

| 范围 | 已实现的行为 |
| --- | --- |
| 浏览器启动 | `browser.launch` 默认 headless；显式 `headless:false` 可用于有意启动的可见窗口或独立桌面。 |
| 标签页 | `browser.open` 默认后台创建；只有显式 `background:false` 才要求前台。 |
| 后台输入 | 通过目标页面的 CDP 焦点模拟发送可信事件，不移动系统指针；关闭连接时明确关闭模拟。DOM 内部焦点与系统焦点分别记录。 |
| 后台截图 | 自有 Chromium 启动时启用 Playwright 同款 `CDPScreenshotNewSurface`；截图保留 JPEG、尺寸映射及大小上限。 |
| 等待与定位 | 新增客户端本地 `browser.wait_for`；快照补充隐式角色和可访问名称。复杂 iframe、Shadow DOM 定位继续使用 Playwright 原生工具。 |
| 独立桌面 | Linux 按需创建带私有授权的 Xvfb；应用、截图、键鼠与输入释放操作使用该显示环境。 |
| 主桌面输入 | 未启动独立桌面时，修改输入必须明确指定 `desktop:"host"`；独立桌面退出后不会自动回退。 |
| 生命周期 | 关闭自有应用进程树与显示服务器后，清理目录、授权和属于本次显示服务的 socket。通过持有文件描述符固定 inode，保留替代文件。 |
| 浏览器 setup | 检查既有浏览器和扩展，验证 CLI，固定安装 Playwright MCP 0.0.85，保存后台偏好，保留其他 TOML 配置。 |
| 客户端能力 | 提供版本、wire 版本、注册表代际、工具与提供者摘要；资源按需分页读取，服务器不另建文档缓存。 |

## 最终验收

所有交互测试都使用本次测试拥有的页面、应用和显示环境。没有向用户正在使用的桌面发送验收用鼠标或键盘输入。

| 验收层 | 结果 | 主要证据 |
| --- | --- | --- |
| 合并后的 Rust 单元测试 | 672 项通过，0 失败；8 项按标记跳过 | `target/cua-validation/memory-merged-rust.log` |
| 客户端资源、上下文集成 | 2 项通过 | 同上；覆盖版本摘要、分页、最新内容、完整长文档与根目录权限 |
| JavaScript 能力、CUA facade、工具搜索 | 23 项通过 | `scripts/test_client_capabilities.mjs` 等三组 Node 测试 |
| 独立桌面完整链路 | 33 项通过 | `memory-merged-private-desktop.json`，含传输认证与实际 GUI 输入、截图、清理 |
| 自有可见浏览器的后台标签 | 37 项通过 | `memory-merged-browser-background-owned.json` |
| Headless 浏览器完整链路 | 57 项通过 | `memory-merged-browser-headless.json` |
| Setup 与真实客户端配置加载 | 9 项通过 | `memory-merged-setup.json` |
| 保留 TTY observer 的回归 | 7 组通过，recording 模式 | `memory-merged-tty.json` |
| 新客户端连接现有服务端 | 通过：完整组合配置、已有账号、旧 relay 原语 | 使用保存账号及现有 `call` / `ListTools`，没有部署新 relay |
| 优化构建 | 优化构建通过（opt-level=z、fat LTO），release CLI 启动与浏览器 setup 检查通过 | `memory-merged-release.log` |

后台浏览器的验收不仅检查“命令返回成功”。它检查点击事件的 `isTrusted`、滚轮事件计数和实际页面位移；图片中的标记由可信点击从红色变为绿色，证明截图来自更新后的目标页面。浏览器原前台标签的窗口标题、另一个测试窗口的键盘焦点、指针坐标均保持不变。自有浏览器关闭后继续检查这些状态。

独立桌面验收在两个独立 X display 之间比较输入：在目标显示中输入文字、点击按钮并截图，另一显示的焦点和指针保持原值。停止目标显示后，应用进程、X11 socket 和临时状态都消失；后续输入明确失败，不回退到主桌面。

Setup 验收包含真实 `--list-tools --shell /bin/sh` 启动：在空 PATH 和临时 `[ui] + [client] + [browser]` 配置下，客户端能正常加载配置。另验证重复 setup 字节不变、`--check` 只读、无效参数不落盘及前后台偏好持久化。

## 修复过程中的有效发现

1. 后台标签的填表成功不代表真实点击成功。目标级焦点模拟解决了可信点击与滚轮输入，并保留了物理输入隔离。
2. Xvfb 被进程守护层终止后可能遗留文件系统 socket。清理逻辑现在等待自有进程退出，再核对原文件身份，防止删除后来替换的 socket。
3. 初版 setup 写入 `[browser]` 后，另一个 UI 配置解析器拒绝该节。已统一接入浏览器字段校验，并增加真实启动回归。
4. Chromium 的传统截图路径可能等待后台页面无法呈现的帧。精确版本源码和官方 Playwright 启动配置指出了 `CDPScreenshotNewSurface`；先做定向探针，再通过实际产品 `browser.launch` 路径完成 37 项验收。
5. 安装前核对发现默认客户端已是并行任务发布的 0.7.4 TTY 构建。先合并已发布 main 并复测；随后又发现默认安装被未合并的内存优化分支更新，最终采用独立命名部署，保留该优化。

早期因测试 scope 的 1.5 GiB 内存上限而终止的编译不计为通过。后续使用单个 Cargo 任务及更充足的任务级限额完成构建；没有取消其他开发任务。这些构建限额不是 ssmcp 运行时内存基准。

## 已运行外部浏览器的明确边界

Linux/Xvfb 测试中的 Chromium 157.0.8103.38 和 Google Chrome 157.0.8103.78，在没有对应启动配置时，后台可信输入、等待、滚动与焦点保持可以通过，后台截图仍会超时。测试同时覆盖新后台标签与曾经显示过的标签，没有把后一种场景假定为成功。

对同一外部进程调用官方 Playwright `connectOverCDP` 也复现该截图问题；连接到既有进程不会改变它的启动参数。失败记录分别保存在 `browser-background.json` 与 `browser-existing-background.json`，没有被修复后的自有浏览器报告覆盖。

产品不会为了获得一张图片偷偷激活标签页。需要可靠视觉反馈时，可以使用本轮已经验证的自有浏览器或独立桌面。显式选择前台仍是调用方可用的操作。此限制不被泛化为所有操作系统或所有 Chrome 版本都失败，也没有据此承诺升级某个版本就一定修复。

## 扩展复用与 setup

开发机已安装并验证固定版本的 Playwright MCP CLI：

```text
~/.config/ssmcp/browser/playwright-0.0.85/node_modules/@playwright/mcp/cli.js
```

常用入口：

```sh
ssmcp setup browser --check --json
ssmcp setup browser
ssmcp setup browser --install
ssmcp setup browser --playwright-mcp /absolute/path/to/cli.js
```

Playwright MCP Bridge 需要 0.5.0 或更新版本。发现扩展 manifest、验证 CLI 与完成浏览器配对是不同状态。首次连接仍需要用户在扩展原生界面批准；本轮未代替用户批准，也没有读取扩展 token 存储。ChatGPT 扩展被纳入公开安装信息检查；未发现可供 ssmcp 直接调用的公开通用控制接口，因此没有声称已接管它的私有能力。

开发机现已运行独立会话 **`ssmcp-cua-20261118`**。安装时发现默认客户端正被并行的内存优化任务使用，因此采用独立部署。随后 PR #21 合入主分支，本轮将其完整合并并复验，现在线上的独立会话同时包含后台控制、TTY 与内存优化；默认安装继续保留。

- 构建：`~/.local/share/ssmcp/builds/cua-397a14a5ef18/ssmcp`
- 配置：`~/.config/ssmcp/profiles/cua-20261118.toml`
- 用户服务：`ssmcp-cua-20261118.service`
- 版本：0.7.4 开发构建，包含本轮 CUA 改动、已发布 TTY 功能与主分支内存优化。
- SHA256：`2f097558335a468ed847abf9148fe0f5686fcaa70282a69311374b078f0f8734`

已对优化产物执行实际 CLI 启动检查，并通过在线新会话读取能力清单和独立桌面状态。新会话报告 wire 5，浏览器、电脑及客户端资源原语均可发现；独立桌面处于未启动状态，验证按需分配。核对时原有 7 个客户端仍在线，本轮只更新自己创建的独立会话。

这是当前用户管理器中的临时服务，停止或用户管理器重启后不会自动恢复；构建与配置文件会保留。停止本轮会话：

```sh
systemctl --user stop ssmcp-cua-20261118.service
```

检查本轮专用环境：

```sh
SSMCP_CLIENT_CONFIG="$HOME/.config/ssmcp/profiles/cua-20261118.toml" \
  "$HOME/.local/share/ssmcp/builds/cua-397a14a5ef18/ssmcp" setup browser --check --json
```

用户管理器重启后，可用同一配置重新连接；以下命令需要保持运行：

```sh
SSMCP_CLIENT_CONFIG="$HOME/.config/ssmcp/profiles/cua-20261118.toml" \
  "$HOME/.local/share/ssmcp/builds/cua-397a14a5ef18/ssmcp" \
  --name ssmcp-cua-20261118 --no-browser --no-setup --watchdog
```

## 服务端与客户端的边界

客户端拥有以下原子入口：

```text
ssmcp.client.capabilities
ssmcp.resources.list
ssmcp.resources.templates
ssmcp.resources.read
```

更新后的服务端新增通用资源路径：

| 路径，位于 `ssmcp://clients/{id}` 下 | 作用 |
| --- | --- |
| `/capabilities` | 客户端能力摘要 |
| `/resources` | 分页资源目录 |
| `/resource-templates` | 客户端资源模板 |
| `/resource?uri=...` | 按客户端 URI 读取资源 |

未知的新资源类型和扩展字段沿通用通道传输；远程读取完成后重新核对授权。wire v4/v5 保持兼容。现有服务端可以通过旧 `call(clientId, name, args)` 访问新客户端原子能力；新的 `native(clientId)`、`capabilities(clientId)` 和 `resources.forClient(clientId)` 便捷接口需要一次服务端更新。

本次上线的是新客户端；现有 ssmcpd 继续提供连接。新增服务端便捷接口尚未部署，客户端原子工具已通过现有通道实测可用。本轮未测量服务端 RSS、磁盘占用或吞吐量；已验证的是职责解耦、按需获取客户端信息，以及与现有服务端的兼容性。

## 复现与证据位置

```sh
cargo test --offline --locked --lib --test client_resources --test context_runtime -- --test-threads=2
node --test scripts/test_client_capabilities.mjs scripts/test_tools_search.mjs scripts/test_cua_facade.mjs
python3 scripts/e2e_private_desktop.py --bin-dir target/debug --report target/cua-validation/private-desktop.json
```

浏览器脚本的 `--owned-browser` 模式通过真实产品启动路径验收。完整命令及环境选项见脚本 `--help`；其对应报告、截图及构建日志保存在工作树的 `target/cua-validation/`。可复核的 JSON 证据摘要与自有浏览器截图另归档于 `docs/reports/cua-client-capabilities-20261118/`；摘要仅整理本机路径与临时会话标识，不改变检查结果。测试报告中的耗时只描述当次执行，不是性能基准。

使用说明：[后台浏览器与独立桌面](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/browser-desktop.md)。

同步报告目录：[Google Drive](https://drive.google.com/drive/folders/1Q50arainsVtSuGXOmm66e5lJNZ7QhGZx)。

## 一手参考资料

- [Playwright 扩展后台模式](https://github.com/microsoft/playwright/blob/main/packages/extension/README.md#working-in-the-background)
- [Playwright MCP 发布记录](https://github.com/microsoft/playwright-mcp/releases)
- [Playwright Chromium 启动配置](https://github.com/microsoft/playwright/blob/main/packages/playwright-core/src/server/chromium/chromiumSwitches.ts)
- [Chromium 157 的截图 feature 定义](https://chromium.googlesource.com/chromium/src/+/157.0.8103.38/content/common/features.cc)
- [Chromium 157 的截图实现](https://chromium.googlesource.com/chromium/src/+/157.0.8103.38/content/browser/devtools/protocol/page_handler.cc)
- [ChatGPT 浏览器扩展公开文档](https://learn.chatgpt.com/docs/chrome-extension.md)

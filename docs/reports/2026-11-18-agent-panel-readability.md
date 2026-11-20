# ssmcp：Agent 面板与图片清晰度验收

日期：2026-11-18（Asia/Taipei）

[在 GitHub 查看带截图和源码链接的在线版本](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/2026-11-18-agent-panel-readability.md)

本轮围绕两条实际反馈迭代：Agent 页把重复工具调用显示为多个条目，以及截图预览中的文字无法辨认。已完成调用方归组、稳定选择、成员消息隔离、原生图片保真与长图滚动；最终验收记录分别覆盖程序状态和实际显示效果。

## Agent 页表达的对象

工具调用是一次事件，成员是能够接收消息的身份。原来的列表按事件增加行，侧栏却按调用方统计，同一个调用方因此看起来像多个 Agent。现在列表、键盘导航、鼠标选择和侧栏共用稳定的调用方分组，同时继续按完整成员 ID 投递消息。[分组实现](../../src/client/activity.rs) · [Agent 页实现](../../src/client/terminal/conversation.rs)

| 对象 | 显示方式 | 选择后的作用 |
| --- | --- | --- |
| 已登记成员 | 名称、角色、状态；同名记录显示各自的 ID 后缀 | 将输入定向到完整 `agent_id` |
| 观测到的调用方 | 按来源、名称、模型归组；更新调用数和运行状态 | 查看代表调用的审计详情，保留原消息目标与草稿 |
| Next caller | 独立的收件选项 | 将要求排给下一次合格响应，不计为在线成员 |

新调用更新已有行，选中项按稳定身份保留。鼠标点击对应上一帧实际显示的对象，避免列表异步重排时选中邻近条目。调用方的名称和模型是显示信息，不授予接收定向消息的资格。所选对象的消息状态进入侧栏，窄屏列表仍显示模型和同名成员的 ID 后缀。[使用说明](../client-ui.md#agent-面板与成员身份)

[重复调用只占一个调用方条目](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/agent-panel-readability-20261118/agent-panel-callers.png`）

[同名成员保持独立，查看调用方时保留原草稿和收件人](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/agent-panel-readability-20261118/agent-panel-wide.png`）

## 图片为什么模糊，以及本轮的处理

用户提供的截图显示的是字符块缩略图。字符块用少量终端单元近似图片颜色，缩略过程中已丢失小字细节；放大这些色块无法恢复文字。原生图片路径则把 PNG 像素交给支持 Kitty 图像协议的终端绘制。两条路径现在明确标注，字符回退显示 `Low-resolution thumbnail · text may be unreadable`。[路径选择与限制](../terminal-images.md)

原生缓存先尝试保存原始像素。单张编码仍受 4 MiB + 64 KiB 限制，超限后只尝试一次最长边 1024 的缩小版本；Activity 中全部已编码图片仍受 12 MiB 总预算限制。PNG 按扫描行写入有界缓冲区，噪声图的压缩回归已覆盖宽图与最大面积回退，未通过提高预算掩盖失败。[PNG 编码与回退](../../src/client/terminal/images/kitty.rs)

长图按 Activity 的可用宽度显示，保留比例和可滚动高度。在跟随输出时收到的新长图从标题和图顶开始显示，随后暂停自动跟随；后续工具消息不会把阅读位置拉到底部。Ctrl+End 返回最新输出并恢复自动跟随。窗口调整大小时若像素尺寸尚未返回，图片保留待显示标记，尺寸恢复后再定位到图顶。等待期间文字继续跟随，用户主动执行 Ctrl+End 的选择优先。[布局与状态](../../src/client/terminal/events.rs)

实际终端测试还发现单图错误会影响整个会话。测试中的 2280×2400 图片编码为 PNG 后仅 238,935 字节，符合 ssmcp 的编码预算，但解码为 RGBA 需要 21,888,000 字节，超过本次 ztty 0.2.1 的 16 MiB 单图上限。终端先回复 `E2BIG`，随后仍能接受小图；ssmcp 原先却把这条图片错误当成整个原生能力失效。[编码大小证据](agent-panel-readability-20261118/native-limit-encoding.json) · [实际终端回复](agent-panel-readability-20261118/native-limit-terminal-replies.json)

现在图片错误只释放对应图片的原生缓存与显示位置，保留该事件的字符预览，其他图片继续显示。二维码也按各自缓存条目处理拒绝，必要时显示完整链接。能力查询失败和超时仍保留字符回退；本轮没有提高终端内存上限，因此超限大图本身仍可能显示低清缩略图。[错误范围](../../src/client/terminal/images/kitty.rs) · [输入处理](../../src/client/terminal.rs)

新版终端验收在独立 Xvfb 中运行 ztty 0.2.1，并读取真实 X11 窗口像素。600×2400 长图的顶部、中段和底部与源图逐像素一致；2280×1728 宽图缩到 Activity 宽度后，顶部与滚动后的裁切仍逐像素一致。超限图被拒绝后，同一客户端继续显示新的 128×64 小图，像素也与源图一致；重载后不存在旧图片覆盖，协议回复不会进入输入草稿。[原生终端完整记录](agent-panel-readability-20261118/native-images-result.json)

[新终端直接显示长图中的文字和单像素细节](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/agent-panel-readability-20261118/native-tall-top.png`）

[超过终端限制的图片被拒绝后，小图仍保持原生显示](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/agent-panel-readability-20261118/native-after-rejection.png`）

旧版 ztty 0.1.2 的独立对照使用同一张 600×2400 合成图，保留原始 MCP 图片文件，显示路径为字符缩略图。验收时已安装的新版位于 `~/.local/bin/ztty`，现有五个用户 ztty 进程仍运行 `/usr/bin/ztty` 0.1.2；安装新终端不会自动替换已经打开的进程。[旧版对照](agent-panel-readability-20261118/legacy-terminal-result.json) · [低清缩略图截图](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/agent-panel-readability-20261118/legacy-thumbnail.png`）

## 验收证据

开发机在候选 `01a012fa5559a6a9328e0ac0518be76bee3af151` 上完成了下列检查。[结构化记录](agent-panel-readability-20261118/validation.json)

| 检查 | 结果 | 范围 |
| --- | --- | --- |
| 终端定向测试 | 133 通过，0 失败 | 列表选择、图片、滚动、输入、终端状态等 |
| 调用方分组测试 | 7 通过，0 失败 | 稳定键、代表调用、断线历史、未知身份 |
| 审计视图测试 | 4 通过，0 失败 | 精确调用、状态更新、Unicode 与保留上限 |
| 构建 | 通过 | `ssmcp`、`ssmcpd`、`vt_snapshot` |
| Clippy | 通过 | `--lib -- -D warnings` |
| 格式 | 通过 | `cargo fmt --check` |
| Agent 页真实 PTY | 40 项检查通过 | 独立 SSH/OAuth、导航、消息定向与一次交付 |
| 原生终端图像流程 | 73 项检查通过 | 实际窗口像素、滚动、宽度缩放、拒绝恢复、重载 |
| 旧终端单图对照 | 通过 | 保留原始 MCP 图片，明确使用低清缩略图 |

两组端到端检查分别包含复用测试框架的 SSH/OAuth 前置阶段，完整项目逐项列于 [Agent 页记录](agent-panel-readability-20261118/agent-panel-result.json)和[原生图像记录](agent-panel-readability-20261118/native-images-result.json)。Agent 页直接验证了三次、四次重复调用仍为一行；两个同名成员可分别选中；新活动和窄屏切换保留 Unicode 草稿、光标、编辑状态与收件人；其他成员和未绑定调用方无法领取定向消息，指定成员只收到一次。

`validation.json` 记录候选提交与二进制 SHA-256；PTY 记录保留实际使用的二进制哈希，可与候选记录逐项核对。定向测试、构建与静态检查均在该候选上执行。

Agent 页 PNG 来源于生产程序的真实 PTY cell 快照，经项目 `render_ui_snapshot.py` 使用 DejaVu Sans Mono、16px 导出；它们证明文字布局和导航状态，原生图片像素由独立的 Xvfb 实际终端测试核对。所有公开图片均来自合成测试内容，不包含用户上传的原图。

## 复现命令

在独立工作树中使用已固定的 Rust 工具链：

```sh
cargo test --locked --lib -j 4 client::terminal:: -- --test-threads=2
cargo test --locked --lib -j 4 client::render::activity::caller_tests::
cargo test --locked --lib -j 4 client::audit_view::
cargo build --locked --bin ssmcp --bin ssmcpd --example vt_snapshot -j 4
cargo clippy --locked --lib -j 4 -- -D warnings
cargo fmt --check

python3 scripts/e2e_agent_panel.py \
  --bin-dir target/debug --out out/agent-panel \
  --font /usr/share/fonts/TTF/DejaVuSansMono.ttf

python3 scripts/e2e_native_images.py \
  --bin-dir target/debug --ztty /path/to/ztty \
  --out out/native-images
```

字体路径以测试机器的实际安装位置为准。图像测试需要 Xvfb、python-Xlib、Pillow 和支持 Kitty 图像协议的终端。常规验证在开发机本地完成，提交使用 `[skip ci]`。

## 交付与运行状态

主体改动已通过 [PR #33](https://github.com/LIghtJUNction/ssmcp/pull/33) 合入主线。后续生产修复包括[噪声 PNG 预算](https://github.com/LIghtJUNction/ssmcp/commit/1f36c53211eb9b214a2bdf7301314aac2035dcf9)、[窗口尺寸恢复](https://github.com/LIghtJUNction/ssmcp/commit/c402f06b472ed4016d2baea3245198b6b782a9de)和[逐图错误隔离](https://github.com/LIghtJUNction/ssmcp/commit/79c874dfe6e3bfd23565b3666039dba48baa9c55)。被测生产版本为 `01a012f`，最终原生验收脚本为 `d61df0b`；脚本更新不改变已构建的生产二进制。

本轮候选程序位于开发机 `out/agent-panel-readability-20261118/target/debug/`，原始验证日志位于同一输出目录。代码合并、候选构建与用户当前运行的进程是三个独立状态；本轮验收使用隔离进程，未替换现有用户会话。

原生预览依赖终端实际响应能力查询和提供正确像素尺寸。普通字符终端、tmux/screen 回退路径以及超出图片预算的场景仍可能显示缩略图。图片原始 MCP 块的 400 KiB 限制、解码尺寸上限和输出内存限制与原生缓存预算分别执行；这次图片显示改动不扩大这些输入限制。[完整边界](../terminal-images.md#limits-and-image-ownership)

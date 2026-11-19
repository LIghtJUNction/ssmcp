# Listener 历史分支整合验收

本轮把已验收但尚未进入主线的 listener 完整分支，与 v0.10.13 主线及左右滚动修复整合到同一候选。冻结产品源码为 [70aebd0](https://github.com/LIghtJUNction/ssmcp/commit/70aebd09d56cfa21cd0229da6c0e3c3cdaae2200)。该候选通过 56 项定向 Rust 测试、118 项强制发送检查、73 项双程序传输检查、121 项用户等待检查和 44 项真实浏览器 local 检查。各套件分别计数，检查数不等于独立功能数。原始断言、二进制摘要和门禁记录见本目录 JSON。

## 来源与整合边界

| 角色 | 不可变提交 |
| --- | --- |
| 主线起点 | `4e3132d60600dca5053af81cdefa6962c3058ac4` |
| 完整历史 listener 分支 | `57d3065ed2d65f579371e3cde170ebc6186f0ed0` |
| listener 核心实现 | `1001a0dba75795ca723a24ab4311546500ae4c43` |
| 左右滚动修复 | `4efc04486bb210f5b3861d43eba7d766aa888d69` |
| 冻结产品源码 | `70aebd09d56cfa21cd0229da6c0e3c3cdaae2200` |
| 本轮 E2E 收尾夹具 | `9989076d5ede9fd4959fc2b5c269c36909e6ab76` |

整合采用 merge，保留来源提交及祖先关系。历史分支同时包含统一账户授权、流量配额、OAuth 持久化、线程广播与来源、独立 frontend、队列回执和输入区改进；本报告验证的是它们与当前 listener、local、user-wait 和终端滚动交界处的具体风险。历史要求已明确统一为 `user@relay` 与 `--allow` 授权，因此保留该分支对 invite/join 和旧 SSH 入口的迁移处理。

合并时保留了主线的 local 启动、退出清理、账户状态与授权撤销唤醒。具体处理包括：账户传输继续携带明确的 relay 地址和可选凭据；local 不进入远端登录流程；撤销访问后唤醒等待者；Local 状态不显示远端流量配额；OAuth 撤销测试检查新的 Result 返回值。滚动冲突采用当前动态输入区 Geometry，并保留“命令面板左半区滚内部输出、右半区滚会话”的边界条件。

本轮只推送源码与证据。正式发布、安装、服务重启以及后续 zuse、ztuio、zsys、zplugin 升级均不包含在此冻结候选的验收结论内。

## 验证方法与结果

编译使用已有 Cargo target，`CARGO_BUILD_JOBS=1`，继续经过开发机的 `rustc-serial` 锁。完成编译后把 CLI、daemon、frontend、测试执行文件和 VT 解码器复制到独立目录，所有本轮 E2E 均使用这组冻结文件，没有调用 Cargo 隐式重编译。

| 门禁 | 结果 | 证据 |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 通过 | [validation.json](validation.json) |
| 库测试编译及三个产品二进制、VT 解码器构建 | 通过 | [validation.json](validation.json) |
| `cargo clippy --locked --all-targets --jobs 1 -- -D warnings` | exit 0 | [validation.json](validation.json) |
| 定向 Rust 测试 | 56 项，全部通过且名称无重复 | [validation.json](validation.json) |
| 真实 CLI/PTY 强制发送 | 118 项通过 | [force-send.json](force-send.json) |
| 真实 HTTP/Unix 双程序 listener | 73 项通过 | [transports.json](transports.json) |
| 真实 CLI/PTY 用户等待 | 121 项通过 | [user-wait.json](user-wait.json) |
| Linux local 与真实 Chromium OAuth | 44 项通过 | [local.json](local.json) |

严格 Clippy 命令成功退出；Cargo 自身另外报告 `qrcodegen` 为未使用依赖。该 manifest 警告没有被写成“零警告”，本轮也没有顺带修改依赖。

56 项 Rust 测试覆盖 listener 接口与线程隔离 12 项、runtime CallScope 5 项、客户端协议 7 项、HTTP 等待 5 项、native 等待 7 项、local 5 项、账户状态 10 项、滚动分区 1 项、终端 listener 回执与草稿保护 4 项。它们来自本次冻结执行文件；历史报告的 89 项结果不混入本次计数。

强制发送检查覆盖显式投递、accepted/failed/unknown 回执、草稿修订保护、普通队列保留和断线后的不自动重放。双程序检查使用独立真实进程，通过 OAuth/MCP 与私有 Unix socket 绑定和处理消息，验证作用域归属、单条在途限制、回执及退出资源清理。两套检查的自有进程均以 0 退出，详见对应 JSON。

用户等待检查覆盖鉴权、消息目标、超时、取消和连接替换。实际 PTY 输入唤醒已认证 HTTP 请求；重连不继承旧连接的等待账本，未提交草稿保留，最终恢复自有终端的 termios。测试使用 `notify:false` 和合成输入。

local 检查使用独立前台 CLI、私有 Unix socket、loopback HTTP、OAuth/PKCE 与真实 Chromium 157.0.8103.38（Playwright 1.65.1）。浏览器确认隔离 Cookie，随后真实文件、进程和用户等待调用往返成功。运行中的 Bubblewrap 只读绑定了 session 私有的内置 runner，内容摘要与编译源码一致；退出后自有进程为 0，socket、私有目录和网关全部清理。没有使用远端账户或启动模型，详见 [local.json](local.json)。

### 验收中修复的夹具偏差

旧 local 和 user-wait 收尾按一次 Ctrl-C 就等待退出，与历史分支已明确的“有草稿先清空，空输入先提示，再按一次退出”不一致。user-wait 首轮已完成 118 条业务检查，停在这一收尾断言。夹具现在明确检查清草稿不改普通队列、首次空输入 Ctrl-C 保持进程存活，再完成正常退出；强制发送套件原有同等契约保持有效。

local 首轮还因新工作树无法解析裸 `playwright` 包名停在浏览器步骤。复跑显式选择开发机已有 Playwright 模块和 `/usr/bin/chromium`，未安装新依赖，也未修改冻结产品文件。两项修正后的完整复跑全部通过；失败原因和复跑结果记录在 [validation.json](validation.json)。

## 滚动行为与视觉核验

新增 Rust 回归在 120、49、48 列的真实渲染状态中验证：左半区仅滚命令输出，右半区滚整段会话，其他命令窗口、草稿和光标状态保留。ANSI 来自实际终端渲染器，再由同一冻结 VT 解码器和 `render_ui_snapshot.py` 转为 PNG。

协调代理已通过原生图片读取检查 [120 列左内窗](scroll-zones-left-120.png) 和 [48 列右整页](scroll-zones-page-48.png)：命令面板、后续消息和草稿清晰，未发现截断重叠；窄屏隐藏冗余提示。其余渲染图随证据保存：

| 列数 | 左半区滚内部输出 | 右半区滚会话 |
| --- | --- | --- |
| 120 | [查看](scroll-zones-left-120.png) | [查看](scroll-zones-page-120.png) |
| 49 | [查看](scroll-zones-left-49.png) | [查看](scroll-zones-page-49.png) |
| 48 | [查看](scroll-zones-left-48.png) | [查看](scroll-zones-page-48.png) |

视觉检查支持这些截图的布局结论；分区、状态保持和事件路由由 Rust 回归断言验证。

## 剩余范围

当前证据支持冻结候选的本地整合与上述明确路径。Linux 开发机的结果不代替 macOS/Windows 运行验收，也不代表生产 relay 已部署这些新功能。完整跨会话生命周期、内存优化以及后续 Project Z 依赖迁移仍应使用各自候选和验证记录。未触发或等待日常 CI。

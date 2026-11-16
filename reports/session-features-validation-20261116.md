# ssmcp 会话控制与长输出护卫验收报告

日期：2026-11-16（UTC）

## 结果

`--title`、`code_mode` 的 `reload(clientId)` 和长输出自动转储已实现。实现提交 `3e6e06a6d81f87f5acda70e5fe10c7dc2732163b` 在 Ubuntu x86_64、ARM64 两条 CI 路径均通过，包含真实 SSH/OAuth/PTY 验收、发布版构建与安装检查。

- [变更 PR #6](https://github.com/LIghtJUNction/ssmcp/pull/6)
- [双架构 CI 与完整日志](https://github.com/LIghtJUNction/ssmcp/actions/runs/43822387229)
- [标题提交](https://github.com/LIghtJUNction/ssmcp/commit/aa5316c6a9e9e9db43b556cbb3899b71fabda4e5)
- [重载与输出护卫实现提交](https://github.com/LIghtJUNction/ssmcp/commit/3e6e06a6d81f87f5acda70e5fe10c7dc2732163b)
- [完整使用说明](https://github.com/LIghtJUNction/ssmcp/blob/feat/session-title-reload-output-guard/docs/session-control.md)

本次完成代码和构建验收。最终检查时 ssmcp 客户端列表为空，开发机无法接入，因此本次没有安装或重启生产服务，未验证在线插件已经使用新版。部署需要同时更新服务端和客户端；旧客户端的第一次升级仍需普通重启。

## 需求与行为

| 需求 | 实际行为 | 主要证据 |
| --- | --- | --- |
| 让 agent 知道会话用途 | `--title` 支持中文和空格，最多 512 个 UTF-8 字节；连接信息、`list` 与 `clients` 快照显示标题 | 参数、握手、发现测试与真实 SSH 用例 |
| 更新后重载客户端 | `reload(id)` 先确认请求，再等待服务端回执，关闭旧资源并执行已安装入口；保持参数、目录、环境与允许列表 | 6 个客户端进程回归测试及 HTTP/OAuth 测试 |
| 大输出只返回元数据 | 超过 32 KiB 或 500 行时转储，返回路径、SHA-256、行数、大小、完成状态和过期时间 | 大于 1.1 MiB 的命令输出、600 行短输出、分片重建校验 |
| 自动清理 | 启动、创建转储及每 60 秒执行保留策略；活动文件持锁跳过 | 过期、容量、活动捕获、异常清理单元测试 |

标题是描述信息，工具调用仍使用唯一客户端 ID。重载后 ID 改变；再次调用公开 `list`，按标题、名称和目录核对新连接。安装入口可被原子替换或通过符号链接切换；启动显示名称与其他 PATH 程序冲突时，不会重载错误的程序。

PTY 在进入原有 1 MiB 环形缓冲之前捕获字节，因此大输出前缀不会因缓冲滚动而丢失。PTY 文件包含整个终端会话的合并流。其他超大原生结果保存为完整 MCP JSON；后端自身已经截断的内容无法恢复。

## 验证记录

以下结果在两种架构上均通过。数字来自 [x86_64 作业](https://github.com/LIghtJUNction/ssmcp/actions/runs/43822387229/job/133148229534) 与 [ARM64 作业](https://github.com/LIghtJUNction/ssmcp/actions/runs/43822387229/job/133148229341) 的日志。

| 检查 | 每种架构的结果 |
| --- | --- |
| Rust 格式与 Clippy | 通过；Clippy 使用 `-D warnings` |
| 默认 Rust 测试 | 232 通过，0 失败，15 默认忽略 |
| 显式 Node/bubblewrap 测试 | `code_mode` 9、`two_tools` 7、`workspace_bridge` 3、`context_helpers` 2，全部通过 |
| 安装服务约束下的运行时 | 1 项通过 |
| 注册持久化与服务重启 | 8 项通过 |
| 真实 SSH/OAuth/PTY | 143 项通过 |
| 本次会话功能专门验收 | 66 项通过 |
| 五种后端协议 | 240 项通过 |
| 发布版编译与暂存安装 | 通过 |

显式运行时分组包含部分默认测试，不能把所有分组数字相加当成独立用例总数。默认忽略的运行时测试已通过专门命令运行；需要真实外部模型的 3 个 smoke test 未运行。五种后端使用真实本机 PTY 与确定性协议 fixture，没有调用外部模型。

### 专门验收覆盖

`scripts/e2e_session_features.py` 建立隔离的真实 OpenSSH 服务、OAuth PKCE 授权、HTTP MCP 与客户端进程。它验证：

1. 中文标题与启动目录可通过已授权发现读取。
2. 实际命令产生超过 1.1 MiB 的输出，起始标记、确定性正文和结束标记均保留。
3. 文件字节数、逻辑行数和 SHA-256 与元数据一致。
4. 通过不超过 32 KiB 的分页重建整个文件，摘要一致。
5. 600 行短输出在总字节数不足 32 KiB 时仍触发转储。
6. 清理操作保留活动转储。
7. `reload` 得到确认，同一进程重新连接，ID 变化但标题、名字和目录保留。
8. 旧连接 OAuth token 返回 HTTP 401，新授权可操作新终端。
9. 重载前尚未过期的日志仍可通过新客户端读取。

新增脚本已加入 CI；收尾同时补入 `make test`。该入口通过 `make -n test` 核对，调用与 CI 已执行的命令一致。随客户端安装的操作技能也同步了标题、重载与分片读取规则。收尾文档提交不改变已通过验收的运行时代码。

## 默认策略与限制

| 项目 | 默认值 |
| --- | --- |
| 内联阈值 | 32 KiB 或 500 行 |
| 转储位置 | 客户端机器的 `/tmp/ssmcp/<uid>` |
| 保留期限 | 24 小时 |
| 已结束文件总量 | 512 MiB，淘汰最旧文件 |
| 单文件上限 | 256 MiB |
| 自动清理间隔 | 60 秒 |
| 分片读取上限 | 32 KiB |

活动捕获不受总量淘汰，所以运行中占用可能暂时高于 512 MiB。达到单文件上限或发生写入失败时保留已写入前缀并明确标记不完整；若文件创建失败，可能没有可读 artifact。全部客户端退出期间没有清理进程，下次启动继续清理。

`reload` 负责执行已安装更新，不负责下载、编译或安装。进行中的工具请求会导致 busy；后台命令、harness 和转发随重载停止。登录码授权绑定旧连接，因此需要重新登录；有效 GitHub 身份授权配合相同 `--allow` 可以发现新连接。

## 交付位置

- GitHub：PR #6、提交与使用说明见本报告开头。
- Google 云盘：[ssmcp-session-features-2026-11-16](https://drive.google.com/drive/folders/1XP7Em0aVz9X1S8dyept4nhp8ou0uFRJL)，包含本报告、机器可读验收记录和使用说明。

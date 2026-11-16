# 上下文视图与文本原语：验收记录

日期：2026-11-16（Asia/Taipei）。全部指定验收项通过。

本轮实现技能摘要视图、AGENTS.md 规则链、显式只读上下文根，以及原生 UTF-8 文件读写和精确编辑。完整使用方法见 [接口说明](context.md)。

## 验收对象

- 源码提交：[e481bef2854373fe83ac31bb6372983fedfcd98e](https://github.com/LIghtJUNction/ssmcp/commit/e481bef2854373fe83ac31bb6372983fedfcd98e)。
- 基线 main：`0c0dcee90b183e94d26492e062ee6ffa111c26de`，保留之前的 workspace bridge、portal 与部署修复。
- 开发机工作区：`/home/lightjunction/Documents/GITHUB/ssmcp-context-primitives`。
- 验收开始、每个 gate 前及结束时均核对 HEAD；开始和结束的工作区都干净，期间未修改源码。
- 运行开始时间：2026-11-16 06:07:42 UTC；总耗时 35.748 秒。
- 最终运行中每项 gate 只执行一次。本报告在运行完成后补入，未改变已验收的实现。

## 整体验收结果

| 检查 | 结果 | 命令耗时 |
| --- | --- | ---: |
| `cargo fmt --check` | 通过 | 0.315 s |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过，无警告 | 3.921 s |
| `cargo test --locked` | 197 passed / 13 ignored / 0 failed | 18.014 s |
| `code_mode::tests --include-ignored` | 9 passed | 1.869 s |
| `two_tools --include-ignored` | 3 passed | 0.816 s |
| `workspace_bridge --ignored` | 3 passed | 2.521 s |
| `context_helpers --ignored` | 2 passed | 0.214 s |
| `cargo build --locked --bins` | 通过，复制独立二进制后验收 | 0.114 s |
| `scripts/e2e.py` | 143 checks | 3.172 s |
| `scripts/e2e_agents.py` | 240 checks | 4.323 s |
| `scripts/test_registry_restart.py` | 8 checks | 0.164 s |
| `cargo package --locked --allow-dirty --offline --no-verify` | 打包通过 | 0.164 s |
| crate 成员检查 | 89 个文件；18 项必需成员全部存在 | — |

显式测试覆盖默认跳过的 13 项隔离测试；上述 17 项显式测试中，有 4 项同时属于默认测试，不能重复计为新增测试。两个真实 SSH 脚本合计 383 项检查，加上完整 daemon 重启脚本共 391 项检查。

### 后端目录与协议检查

| 模式 | 工具数 | 目录页数 | 检查数 |
| --- | ---: | ---: | ---: |
| Shell | 32 | 1 | 44 |
| Codex | 39 | 1 | 49 |
| Claude Code | 37 | 1 | 48 |
| Pi | 65 | 2 | 50 |
| OpenCode | 106 | 2 | 48 |

五个模式合计 239 项，另有 1 项共享检查。新增原语使 Pi 工具目录超过一页；测试已验证两页目录和完整工具集合。

## 新功能覆盖

技能索引保留名称、描述、路径和根目录来源，同名技能不会互相覆盖。7 项上下文原语测试覆盖元数据引号、多行字段、BOM、CRLF、UTF-8、缺失或不支持的元数据、稳定排序、分页游标、扫描预算，以及重叠扫描路径。完整技能正文通过显式读取按需获得。

AGENTS 视图从所选根到目标目录返回有序规则来源。测试覆盖 `AGENTS.override.md`、`AGENTS.md`、`agents.md` 的选择顺序，空文件回退、缺失目标、兄弟目录排除和内容上限。附加上下文根必须显式配置并选择；它们只开放读取上下文文档，不扩大普通文件原语的写入范围。

工作区测试共 11 项，其中 6 项覆盖新增文本原语。验证内容包括二进制接口兼容、UTF-8 分页、精确匹配、陈旧 SHA-256 拒绝、并发原生编辑只产生一个成功者、权限和换行保持、符号链接与非普通文件拒绝，以及有限锁等待。写入通过同目录临时文件和原子替换完成；锁只协调使用这些原语的写入者，不声称约束外部编辑器。

2 项真实隔离 JavaScript 测试通过正式 Host 回调调用 Context 和 Workspace，验证冻结的 helper 对象、参数映射、错误传播、根目录限制和文件内容。HTTP 动态资源测试共 7 项，包含新资源的参数编码、分页、授权撤销和客户端断开行为。真实 SSH 脚本还覆盖文本读写、摘要与正文分离、规则链及资源读取。

## 可重现命令

以下命令在具备项目 Linux 隔离依赖的开发环境运行；`code_mode` 显式测试需要 Node.js、bubblewrap 和可用的用户命名空间。二进制先构建、再复制到独立目录，避免其他工作区的后续构建改变验收对象。

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --lib code_mode::tests -- --include-ignored --test-threads=1
cargo test --locked --test two_tools -- --include-ignored --test-threads=1
cargo test --locked --test workspace_bridge -- --ignored --test-threads=1
cargo test --locked --test context_helpers -- --ignored --test-threads=1
cargo build --locked --bins
mkdir -p target/context-primitives/bin
cp "${CARGO_TARGET_DIR:-target}/debug/ssmcp" target/context-primitives/bin/
cp "${CARGO_TARGET_DIR:-target}/debug/ssmcpd" target/context-primitives/bin/
python3 scripts/e2e.py --bin-dir target/context-primitives/bin
python3 scripts/e2e_agents.py --bin-dir target/context-primitives/bin
python3 scripts/test_registry_restart.py --bin-dir target/context-primitives/bin
cargo package --locked --allow-dirty --offline --no-verify
```

## 二进制与打包证据

独立二进制保存在开发机工作区的 `target/context-primitives/bin/`。

| 文件 | SHA-256 |
| --- | --- |
| `ssmcp` | `3d59d97c50ba446a917238f40c5c7d109ab5bddf45af40b96f283c9276963c3f` |
| `ssmcpd` | `8a4196eeb6b272d73777cb37ac55a5ba36742af094b04e51366567c0a04cafbd` |
| 本次源码的 `ssmcp-0.4.1.crate` | `93ec9ce6e2879824473bff4b19a6f5b170c00b99bdecb14f5a5619d41749e111` |

crate 哈希对应上述源码提交打包时的内容，包含补写前的验收文档；后续仅更新本报告会改变归档哈希。打包使用 `--no-verify`，记录的是归档生成与成员完整性检查，编译和测试已在前面的 gate 单独完成。

原始结构化结果和逐项日志保存在：

```text
/home/lightjunction/Documents/GITHUB/ssmcp-context-primitives/target/context-primitives/final/
```

其中 `summary.json` 记录命令、耗时、状态和 HEAD；`binary-sha256.json` 记录独立二进制；`ssh-e2e.json`、`ssh-agents-e2e.json`、`registry-restart.json` 记录集成测试；`package-membership.json` 记录归档成员与哈希。

## 行为和部署边界

技能摘要由 frontmatter 的 `name` 和 `description` 生成，解析器支持的 YAML 子集在返回值与接口文档中公开。缺失或不支持的字段会产生诊断；不会从正文臆造描述，也不会调用模型生成摘要。

AGENTS 视图表示显式根目录内当前磁盘上的规则文件与顺序，不表示某个后端已经载入模型上下文的实际提示词。默认只读取工作区；访问用户级技能或规则需要 `--context-root PATH`。

本轮使用真实 OpenSSH、公钥认证、OAuth、HTTP MCP 和 Linux PTY；模型后端使用确定性原生协议 fixture，没有调用外部模型。SSH 集成测试发生在开发机测试环境，不构成当前 ChatGPT 沙箱与开发机跨主机传输的验收。

本轮完成源码、测试、文档和项目 skill 更新；没有安装或重启生产服务。验收后对在线 `code_mode` 的只读探测显示 `skills`、`agents`、`context` 和 `fs.readText` 尚未提供。使用新接口需要更新客户端 `ssmcp` 与服务端 `ssmcpd`，再重新连接。

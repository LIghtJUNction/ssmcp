# 实时项目上下文资源与 RTK 验收

验证日期：2026-11-18（Asia/Taipei）。变更入口：[PR #17](https://github.com/LIghtJUNction/ssmcp/pull/17)。本报告的实现与测试对应提交 [7390d16](https://github.com/LIghtJUNction/ssmcp/commit/7390d1690a3016835ed71f2c7ef960a5b5ba77c7)。

本轮共通过 **28 项 Rust 测试和 20 项 JavaScript 测试**。真实 RTK 调用、完整隔离的 code_mode 技能读取，以及服务端资源接口均有验证。此处记录源码和测试交付；运行中的服务端与客户端尚未由本任务升级。

## 交付接口

项目指令与技能新增标准 MCP 资源读取渠道，并保留 code_mode 原语。两个入口共用客户端的技能扫描和完整文档读取实现，每次读取重新检查授权并读取当前文件，避免维护两份上下文副本。

| 资源模板 | 返回内容 |
| --- | --- |
| `ssmcp://clients/{id}/context` | 工作区与显式授予的额外上下文根 |
| `ssmcp://clients/{id}/agents{?root,path}` | 适用于目标路径的有序指令链，包含各文件全文；没有文件时 files 为空 |
| `ssmcp://clients/{id}/agents/read{?root,path}` | 单个 AGENTS.md、AGENTS.override.md 或 agents.md 的原文 |
| `ssmcp://clients/{id}/skills{?root,scope,cursor}` | 技能摘要、诊断、来源与全文 readUri |
| `ssmcp://clients/{id}/skills/view{?root,scope,cursor}` | 默认项目叠加全局的 Markdown 技能视图 |
| `ssmcp://clients/{id}/skills/read{?root,path}` | 单个 SKILL.md 的完整 UTF-8 原文 |

有效技能视图按名称合并：工作区优先，其后按显式 --context-root 的配置顺序选择。同根内按扫描路径顺序、文件路径顺序确定优先级。胜出记录保留 root、path 和 shadowed 来源。删除项目覆盖后，再次读取会显露对应的全局技能；无效元数据保留诊断，不覆盖有效技能。

skills.list 默认保持单根索引，用 scope: "effective" 获取合并索引。skills.view 默认使用合并视图，显式 root 或 scope: "root" 可查看单根。全局目录必须由客户端启动配置授予；默认不会扩大到用户主目录。

默认技能视图从资源和 JavaScript 两个服务端入口显式请求 scope: "effective"。旧客户端拒绝该参数时返回错误，不自动退回单根结果。首次启用需要更新服务端和对应客户端。

code_mode 新增 rtk.status、rtk.run、rtk.start。RTK 参数数组逐项传给客户端 PATH 中的可执行程序，任务沿用 exec.read/write/wait/cancel。ssmcp 不拼接 Shell 命令，也不在 RTK 失败后自动重跑原命令；实际执行设备需要安装 RTK。

## 验证结果

所有下列测试均针对 7390d16。Rust 工具链为 rustc 1.100.0、cargo 1.100.0；Linux 独立构建使用 CARGO_BUILD_JOBS=2。

| 验证项 | 环境 | 通过 | 失败 | 忽略 |
| --- | --- | ---: | ---: | ---: |
| context 集成测试 | 独立 Linux 环境 | 12 | 0 | 0 |
| context_runtime 原生读取测试 | 独立 Linux 环境 | 1 | 0 | 0 |
| resources HTTP 集成测试 | 独立 Linux 环境 | 10 | 0 | 0 |
| resources::tests 单元测试 | 独立 Linux 环境 | 4 | 0 | 0 |
| 指定 context_helpers 隔离集成测试 | Linux 开发机 | 1 | 0 | 0 |
| CUA、执行 helper 与 RTK 的 Node worker 回归 | 独立 Linux 环境 | 20 | 0 | 0 |

过滤未选择的测试不计为通过：resources 单元命令另有 598 项被过滤；context_helpers 仅选择本轮相关的一个用例，另一个用例被过滤。未执行全仓库完整测试套件，也未以 CI 状态代替本地验收。

附加检查通过：变更 Rust 文件的 rustfmt、git diff --check、仓库内 ssmcp 技能的 frontmatter 校验。没有增加运行时依赖。

### 完整隔离验证

独立容器不允许 bubblewrap 初始化网络命名空间，因此最终 context_helpers 用例在支持该能力的开发机执行。测试二进制在独立环境按同一提交构建，副本经 strip 和 gzip 后通过原生文件通道传输；运行前校验压缩包、解压后二进制和 runner 的 SHA-256。

| 校验对象 | SHA-256 |
| --- | --- |
| 压缩包，2,313,391 字节 | `3c9a764c8ad52932fc7238c35b3baa867a7cc33970fbd4179712cf0ba75e189a` |
| 解压后测试二进制，6,820,128 字节 | `33c1bb00706f9bdfa22aee4dfa363059f3aeca9bf5366a2acfd16152eecc9cce` |
| src/code_mode.js，35,741 字节 | `dcc3e2ddfcea350bf1af107002a0caba21cb7a920b603eb92a45364e99dcadb5` |

运行使用现有 SSMCP_CODE_MODE_RUNNER 接口选择开发机中同提交的 runner，保留原有 bubblewrap 隔离。指定用例输出为 1 passed、0 failed、0 ignored，退出码 0，测试耗时 0.43 秒。仅编译和列出用例均未计入通过数量。

## 行为覆盖

- 同一连接内修改、添加或删除指令和技能后，后续资源读取使用当前内容；覆盖项目与全局技能的优先级、删除后显露全局版本、多全局根顺序以及无效元数据。
- 摘要附可直接读取全文的 readUri。中文、空格及 +、&、?、= 等字符正确编码；资源 URI 上限为 8192 字节，原生相对路径限制继续生效。
- 技能游标绑定扫描配置与元数据快照；扫描数据改变使旧游标失效。Markdown 视图包含续页链接，扫描达到预算时标注不完整。
- 原生 Runtime 测试确认超过普通 32 KiB 输出阈值的上下文仍完整保留在 structuredContent 中。超过 256 KiB 的单个文档明确失败，不把部分内容作为全文返回。
- 资源测试覆盖现有两个 MCP 协议代际、授权与连接状态检查、MIME 类型、分页、完整原文和默认 effective 范围。旧客户端拒绝 scope 时不会重试为单根。
- JavaScript 测试确认显式 root/scope 透传、调用者参数不被修改、无效参数保持错误，以及旧客户端错误不被隐藏。
- RTK 测试覆盖状态探测、字面参数、工作目录和环境透传、stdin、后台任务、取消、非零退出码、信号、输出制品以及观察阶段错误中的 job_id。
- 合并主分支已有的协作功能后，保留 collab 工具、JavaScript namespace 和协作资源。

## 真实 RTK 样本

开发机使用 RTK 1.2.0。在同一仓库状态下比较 git log -8，原始输出 1587 字节，RTK 输出 742 字节，减少约 53.2%。这是单次命令的字节数样本，不代表所有命令的压缩率，也不是 token 数测量。

真实 code_mode worker 调用还验证了 rtk --version，以及通过 RTK proxy 传递含空格、Shell 特殊字符和中文的字面参数，子进程退出码 7 被保留。proxy / bypass 是透传入口；该用例验证传参和退出码，未计作压缩测试。

## 读取边界与运行状态

“最新”指每次 resources/read 返回当次磁盘读取结果。宿主缓存和已经进入对话的旧内容，需要再次读取才能更新；当前接口不提供资源订阅通知。多个文件的读取不是跨文件事务快照。

单个完整文档和指令链总正文上限为 256 KiB；上下文结构化结果与资源正文上限为 1 MiB。受限上下文响应不再经过普通命令输出的 32 KiB 转存规则。技能扫描继续使用每根预算，分页、truncated 和 diagnostics 均保留。

本任务提交并验证源码，没有发布新版本二进制、重启线上服务或重载现有客户端。验收期间对正在使用的 MCP 服务读取确认：rtk 命名空间仍未加载，资源目录仍为旧模板。首次启用需要更新服务端和对应客户端；完成升级后，后续 AGENTS.md 或 SKILL.md 文件改动通过重新读取即可生效，无需为每次文件编辑重启。

服务自带的 ssmcp://skill 是随服务构建发布的使用指南。项目技能的实时全文通过 ssmcp://clients/{id}/skills/read 获取。

## 复现命令

在支持项目隔离要求的 Linux 环境执行：

```sh
node --test scripts/test_cua_facade.mjs scripts/test_execution_helpers.mjs scripts/test_rtk_helpers.mjs
cargo test --locked --test context --test context_runtime --test resources
cargo test --locked --lib resources::tests
cargo test --locked --test context_helpers skill_discovery_and_instruction_resolution_preserve_scope_and_provenance -- --ignored
```

接口说明：[上下文与资源](context.md)、[RTK 原语](rtk.md)、[工作区资源入口](workspaces.md)。RTK 行为参考 [v1.2.0 CLI 定义](https://github.com/rtk-ai/rtk/blob/v1.2.0/src/lib.rs) 与 [命令执行器](https://github.com/rtk-ai/rtk/blob/v1.2.0/src/core/runner.rs)。

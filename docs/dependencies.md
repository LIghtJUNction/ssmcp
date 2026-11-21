# 独立核心库与子模块

ssmcp 把终端绘制、原生扩展接口、动态库加载和桌面能力拆成四个独立 Rust 库。编译器将这些库连进客户端；它们本身不会启动模型、读取其他 Agent 的账号，或扫描机器上安装的 harness。外部 Codex、Claude Code、Pi、OpenCode 和 Playwright MCP 仍遵循[显式加载](optional-harnesses.md)。

| 源码目录 | 固定库版本 | 在 ssmcp 中的职责 | 独立仓库 |
|---|---|---|---|
| `crates/ztuio` | `ztuio = 0.1.0`，Rust 库名 `ztuio` | 终端布局、文本行和 ANSI 差分帧 | [ztuio](https://github.com/LIghtJUNction/ztuio) |
| `crates/zharness` | `zharness = 0.6.0` | 原生扩展接口与调用原语 | [zharness](https://github.com/LIghtJUNction/zharness) |
| `crates/zplugin` | `zplugin = 0.3.1` | 显式指定的本机动态库加载与 ABI 校验 | [zplugin](https://github.com/LIghtJUNction/zplugin) |
| `crates/zuse` | `zuse-computer = 0.3.0`，Rust 库名 `zuse` | 通过受管工作进程调用桌面输入、窗口与截图 API | [zuse](https://github.com/LIghtJUNction/zuse) |

应用的 Project Z 跨仓库依赖使用 `https://github.com/LIghtJUNction/<repo>.git` 的默认分支源码。根 manifest 直接声明 Git 默认分支来源，保留包别名和 feature。实际解析版本来自上游 manifest，提交由 Cargo.lock 记录。仓库内的 path 或 workspace 依赖继续保留，例如 zplugin 自己的 `zplugin-macros`。

四个 gitlink 保留独立源码和契约测试。发行前的完整 `cargo metadata --locked --format-version 1` 必须核对 Cargo 使用的 Git 提交和工作树内容与对应 gitlink 一致。同步的上游 manifest 已直接声明八项传递依赖的 Git 源，因此移除了相应根级覆盖。实际解析图中，SSMCP 和 zharness 必须指向同一个 zplugin 包标识。[zharness](https://github.com/LIghtJUNction/zharness/blob/cb63a9a0aa163214c6062664cd62fefb357be5a8/Cargo.toml) · [zplugin](https://github.com/LIghtJUNction/zplugin/blob/cf02c1d9db575b92b36297dfe1fde72de9ee51e4/Cargo.toml) · [zcall](https://github.com/LIghtJUNction/zcall/blob/317c52f101f112c6af671a629d8406305a168e4a/Cargo.toml) · [zuse](https://github.com/LIghtJUNction/zuse/blob/830fcefd2277cbba46536c67d206f29058ba7fb3/Cargo.toml)

`ssmcp --version --all` 可离线查看构建所用 Cargo.lock 中从 ssmcp 可达的依赖树。`--version` 仍只输出应用版本。树包含锁定的构建、开发与各目标依赖，实际链接的代码随目标和 features 而变。

## 每轮迭代与发行前同步

1. 按仓库辨别 Project Z 包。读取实际使用的默认分支 manifest，记录 URL、默认 ref、完整提交、树和 UTC 查询时间。
2. 同步 manifest、相关 Git 锁项和源码 gitlink。同一个仓库的包版本取自实际 manifest。仅执行 `cargo update --workspace` 不会刷新已有的 Git heads。按当前 Cargo 图更新相应 Git 包，再核对完整锁定元数据与 checkout。
3. 对受影响的消费者执行定向编译、契约或集成检查。记录已核对的版本、提交和结果。最终门禁使用协调者冻结的同一批源码和锁文件。

GitHub 二进制发行记录固定 Git 源码及 Cargo 解析图。开启 crates.io 发布时，脚本在独立目录以实际 Git 包版本导出 registry manifest 和锁文件，再验证 Cargo 规范化后的源码包及其 registry 图。两份图分别记录。同一版本字符串可以对应不同的 Git 源码与 registry 归档。0.10.25 的 registry 发布延期，具体策略见[发布指南](releases.md)。

### 2026-11-20 迁移前核对记录

2026-11-20 12:34:23–12:36:53 UTC 查询官方 registry，并核对对应仓库 tag。基线是 `fd0c58018605871ba04119e22c2ce089698c3f1c`。本次将 zharness 从 0.5.1 更新为 0.6.0，固定到 `db4206d3b74e37d57ff052948962f55514081d4b`；其发布归档与 v0.6.0 tag 指向同一份源码，带入 zcall 0.4.0 和 zguard 0.1.0。其余直接依赖已处于查询时的最新稳定版。逐项版本、摘要和源码证据见[核对记录](reports/project-z-dependencies-20261120.json)。

| 包 | 使用路径 | 更新后的解析版本 | 查询时最新稳定版与 tag |
| --- | --- | --- | --- |
| [z64](https://crates.io/crates/z64) | zuse | `0.1.2` | [0.1.2](https://github.com/LIghtJUNction/z64/tree/v0.1.2) |
| [zcall](https://crates.io/crates/zcall) | zharness / zmcpio | `0.3.0、0.4.0` | [0.4.0](https://github.com/LIghtJUNction/zcall/tree/v0.4.0) |
| [zclap](https://crates.io/crates/zclap) | zuse | `0.1.4` | [0.1.4](https://github.com/LIghtJUNction/zclap/tree/v0.1.4) |
| [zconv](https://crates.io/crates/zconv) | zcall | `0.4.0` | [0.4.0](https://github.com/LIghtJUNction/zconv/tree/v0.4.0) |
| [zguard](https://crates.io/crates/zguard) | zharness | `0.1.0` | [0.1.0](https://github.com/LIghtJUNction/zguard/tree/v0.1.0) |
| [zharness](https://crates.io/crates/zharness) | 直接 | `0.6.0` | [0.6.0](https://github.com/LIghtJUNction/zharness/tree/v0.6.0) |
| [zmcpio](https://crates.io/crates/zmcpio) | zuse | `0.4.0` | [0.4.0](https://github.com/LIghtJUNction/zmcpio/tree/v0.4.0) |
| [zplugin](https://crates.io/crates/zplugin) | 直接 | `0.3.1` | [0.3.1](https://github.com/LIghtJUNction/zplugin/tree/v0.3.1) |
| [zplugin-macros](https://crates.io/crates/zplugin-macros) | zplugin 的宏包 | `0.3.1` | [0.3.1](https://github.com/LIghtJUNction/zplugin/tree/v0.3.1) |
| [zsys](https://crates.io/crates/zsys) | zplugin / zuse | `0.1.1` | [0.1.1](https://github.com/LIghtJUNction/zsys/tree/v0.1.1) |
| [ztuio](https://crates.io/crates/ztuio) | 直接 | `0.1.0` | [0.1.0](https://github.com/LIghtJUNction/ztuio/tree/v0.1.0) |
| [zuse-computer](https://crates.io/crates/zuse-computer) | 直接 | `0.2.2` | [0.2.2](https://github.com/LIghtJUNction/zuse/tree/v0.2.2) |

表中两条 zcall 调用链来自迁移前的 registry 核对。当时发布的 zmcpio 0.4.0 要求 zcall `=0.3.0`。本轮实取的 zmcpio Git 源码仍为 0.4.0，直接要求 canonical Git 的 zcall `=0.4.0`。外部发行使用实际 registry 图，其兼容性仍待规范化打包确认。[zmcpio Git manifest](https://github.com/LIghtJUNction/zmcpio/blob/84fc4978a248c2a3e2dd110f4f26fa52dd8b6890/Cargo.toml) · [官方索引条目](https://index.crates.io/zm/cp/zmcpio)

当时的锁定解析、原生插件契约和 91 项发布规则测试属于上述历史源码，完整范围见核对记录。

## 获取与构建

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/ssmcp.git
cd ssmcp
cargo build --locked
```

已有 checkout 拉取父仓库后：

```sh
git submodule update --init --recursive
```

GitHub 自动生成的源码压缩包不展开 Git 子模块。源码与契约验证使用递归 Git checkout。发行 workflow 递归获取固定提交，并按已冻结的锁定图验证。

## 独立修改与验收

上游库的修改与 registry 发布各自由其仓库维护。应用同步时记录新默认分支提交，更新消费方的锁和源码 gitlink，并检查 `git submodule status --recursive`。四个子库从父 workspace 显式排除，`cargo test --workspace` 不涵盖各自的独立契约测试。发行既有的 ztuio 独立检查继续使用 `cargo test --manifest-path crates/ztuio/Cargo.toml --locked`。

SSMCP 的 crates.io workflow 只发布应用包。独立依赖版本必须在各自的仓库发布，并通过 registry 摘要与应用实际规范化打包的检查。发布任务会复用已验证的导出锁文件，比较 Cargo dry-run 生成的包与已验证包，发布返回后再核对实际生成包与 registry 摘要。Cargo 上传命令本身会重新打包，因而这些检查提供可重现性和事后核对。[Cargo 发布说明](https://doc.rust-lang.org/cargo/commands/cargo-publish.html)

## zuse 桌面调用

zuse 的 Cargo 依赖键为 `zuse`，发布包名为 `zuse-computer`，当前 Git 源以默认分支提交 `830fcefd2277cbba46536c67d206f29058ba7fb3` 解析到 `0.3.0`。Cargo.lock 与 `crates/zuse` gitlink 记录同一源码。相同版本字符串可出现在多个不同 Git 提交上。

Windows 公共窗口 ID 在 SSMCP 适配边界合并 HWND 与 PID，以保留聚焦身份检查。各操作的实际执行引擎由 `computer.status` 的 `engine_routes` 报告。

每次受支持的电脑操作启动一个普通操作系统子进程，使用固定环境直接调用 Rust Desktop API；它只执行这次桌面请求，不运行模型。父进程等到输入释放和实际退出才交还输入锁，并拥有和清理截图临时目录。CLI 与 Linux 守护进程都注册 worker 入口；嵌入使用方缺少入口注册时会在创建子进程前得到明确错误。

现有 MCP Schema、隔离桌面默认值和主机控制授权边界继续生效。Windows/macOS 输入与状态、明确请求光标的截图兼容路径、显示器拓扑继续使用既有适配器；本次接入未把上游 DSL 或 Android 能力扩展为新的 SSMCP MCP 接口。具体调用和权限见[computer-use 指南](computer-use.md)。

## 依赖更新的传播闭环

底层库更新由该库的维护者负责追踪传播：先枚举直接与间接反向依赖，联系各消费者的现有维护者，再同步版本约束、不可变源码 pin 和锁文件，完成对应契约与集成验证，并及时提交、推送。只有库发布或发送通知不算传播完成。每个消费者都应记录采用的提交、验证范围及结果；尚未完成时明确写出阻碍、下一步和接手人。

Project Z 工作区可使用 `python3 scripts/project_z.py impact --repositories <仓库或包名>` 查询影响面；新发现的下游应登记到项目关系记录中。SSMCP 的消费者验收仍由本项目负责，聚合克隆提供的最小构建不能替代应用、原生插件、PTY 和桌面链路验证。协同已有维护者即可，不需要为传播任务启动额外模型。

## 执行边界

ssmcp 继续使用自己的工具目录、运行时 hook、输出边界和会话历史。接入原生扩展不替换这些机制，也不创建第二套模型循环。

本机动态库是调用者明确选择的受信任代码，加载时可能运行初始化逻辑。ABI 校验检查接口能否正确交换数据，不是文件或网络沙箱；同步本机调用也不能承诺被强制取消。该限制来自本机动态链接本身，不能用超时或界面状态冒充副作用回滚。

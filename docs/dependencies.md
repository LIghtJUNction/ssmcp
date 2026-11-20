# 独立核心库与子模块

ssmcp 把终端绘制、原生扩展接口、动态库加载和桌面能力拆成四个独立 Rust 库。编译器将这些库连进客户端；它们本身不会启动模型、读取其他 Agent 的账号，或扫描机器上安装的 harness。外部 Codex、Claude Code、Pi、OpenCode 和 Playwright MCP 仍遵循[显式加载](optional-harnesses.md)。

| 源码目录 | 固定库版本 | 在 ssmcp 中的职责 | 独立仓库 |
|---|---|---|---|
| `crates/ztuio` | `ztuio = 0.1.0`，Rust 库名 `ztuio` | 终端布局、文本行和 ANSI 差分帧 | [ztuio](https://github.com/LIghtJUNction/ztuio) |
| `crates/zharness` | `zharness = 0.5.1` | 原生扩展接口与调用原语 | [zharness](https://github.com/LIghtJUNction/zharness) |
| `crates/zplugin` | `zplugin = 0.3.1` | 显式指定的本机动态库加载与 ABI 校验 | [zplugin](https://github.com/LIghtJUNction/zplugin) |
| `crates/zuse` | `zuse-computer = 0.2.2`，Rust 库名 `zuse` | 通过受管工作进程调用桌面输入、窗口与截图 API | [zuse](https://github.com/LIghtJUNction/zuse) |

四个库的 Git 提交均由父仓库的 gitlink 固定，`.gitmodules` 记录公开仓库地址。ztuio 固定到 `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e`，复用独立仓库合并后的公共 API、ANSI 帧优化与契约测试，父仓库不再保留另一份终端核心源码。新包名和 Rust 导入名均为 `ztuio`；其版本由独立仓库维护。精确版本写在 [Cargo.toml](../Cargo.toml)，依赖解析结果写在 [Cargo.lock](../Cargo.lock)，日常构建不会跟随任何子库的 `main` 自动更新。

zuse-computer 0.2.2 固定到 `94c837551a7a2fd8ec9f577a3e2f57309bd5ce5a`。Windows 公共窗口 ID 在 SSMCP 适配边界合并 HWND 与 PID，保留原有聚焦身份检查；各操作的实际执行引擎由 `computer.status` 的 `engine_routes` 报告。

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

GitHub 自动生成的源码压缩包不展开 Git 子模块。需要从源码编译时，用递归 Git checkout；普通用户可以直接使用正式 Release 中的可执行文件。发布 workflow 同样递归取回固定提交，不临时拉取各子库的最新代码。

## 独立修改与验收

更新任一库时，先在相应独立仓库提交并推送修改，再在 ssmcp 更新 gitlink 和精确依赖版本。提交父仓库前检查 `git submodule status --recursive`；不要只提交父仓库指针而遗漏子库提交。

ztuio、zharness、zplugin、zuse 均从父 workspace 显式排除，应用通过路径依赖使用固定源码。ztuio 虽然是单包仓库，也明确排除，以免 Cargo 自动把路径依赖收入父 workspace。根级 `[patch.crates-io]` 将 zharness 传递使用的 zplugin 统一到同一个子模块，防止本机构建出现两套相同 ABI 类型。zplugin 0.3.1 与 zuse-computer 0.2.1 都精确依赖已发布的 `zsys = 0.1.1`；zplugin-macros 与 zplugin 同为 0.3.1。升级后需核对实际解析图只有一套这些共享包。

父项目的 `cargo test --workspace` 不会执行已排除子库的独立契约测试。ztuio 使用 `cargo test --manifest-path crates/ztuio/Cargo.toml --locked` 验证；正式发布复用的 CI 配置也保留此显式检查。更新子库时，应在该子库执行自己的测试和发布检查，再对 ssmcp 做集成验证。发布到 crates.io 时使用精确 registry 版本；不把子库的正式依赖改成只在相邻目录才成立的路径。

版本按实际变更分别递增。ssmcp 的版本递增不自动递增四个库。首次发布或升级公共依赖时，先发布对应库并核对包摘要，再发布应用。zplugin 的宏包先于 zplugin，新的 zharness 版本应依赖已发布的精确 zplugin 版本；ztuio 同样从自己的仓库发布，SSMCP 工作流只发布应用包。

## zuse 桌面调用

zuse 源码固定到 `d930769005d307c42639e8abedc81ab039427e40`，Cargo 依赖键为 `zuse`，发布包名为 `zuse-computer`，精确版本为 0.2.1。SSMCP 用 gitlink 固定实际源码，不凭相同版本字符串把不同提交视为同一来源；本轮采用已发布的版本和聚合仓库提供的不可变提交，应用调用仍由 SSMCP 验收。

每次受支持的电脑操作启动一个普通操作系统子进程，使用固定环境直接调用 Rust Desktop API；它只执行这次桌面请求，不运行模型。父进程等到输入释放和实际退出才交还输入锁，并拥有和清理截图临时目录。CLI 与 Linux 守护进程都注册 worker 入口；嵌入使用方缺少入口注册时会在创建子进程前得到明确错误。

现有 MCP Schema、隔离桌面默认值和主机控制授权边界继续生效。Windows/macOS 输入与状态、明确请求光标的截图兼容路径、显示器拓扑继续使用既有适配器；本次接入未把上游 DSL 或 Android 能力扩展为新的 SSMCP MCP 接口。具体调用和权限见[computer-use 指南](computer-use.md)。

## 依赖更新的传播闭环

底层库更新由该库的维护者负责追踪传播：先枚举直接与间接反向依赖，联系各消费者的现有维护者，再同步版本约束、不可变源码 pin 和锁文件，完成对应契约与集成验证，并及时提交、推送。只有库发布或发送通知不算传播完成。每个消费者都应记录采用的提交、验证范围及结果；尚未完成时明确写出阻碍、下一步和接手人。

Project Z 工作区可使用 `python3 scripts/project_z.py impact --repositories <仓库或包名>` 查询影响面；新发现的下游应登记到项目关系记录中。SSMCP 的消费者验收仍由本项目负责，聚合克隆提供的最小构建不能替代应用、原生插件、PTY 和桌面链路验证。协同已有维护者即可，不需要为传播任务启动额外模型。

## 执行边界

ssmcp 继续使用自己的工具目录、运行时 hook、输出边界和会话历史。接入原生扩展不替换这些机制，也不创建第二套模型循环。

本机动态库是调用者明确选择的受信任代码，加载时可能运行初始化逻辑。ABI 校验检查接口能否正确交换数据，不是文件或网络沙箱；同步本机调用也不能承诺被强制取消。该限制来自本机动态链接本身，不能用超时或界面状态冒充副作用回滚。

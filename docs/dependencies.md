# 独立核心库与子模块

ssmcp 把终端绘制、原生扩展接口和动态库加载拆成三个独立 Rust 库。编译器将这些库连进客户端；它们本身不会启动模型、读取其他 Agent 的账号，或扫描机器上安装的 harness。外部 Codex、Claude Code、Pi、OpenCode 和 Playwright MCP 仍遵循[显式加载](optional-harnesses.md)。

| 源码目录 | 固定库版本 | 在 ssmcp 中的职责 | 独立仓库 |
|---|---|---|---|
| `crates/ztuio` | `ztuio = 0.1.0`，Rust 库名 `ztuio` | 终端布局、文本行和 ANSI 差分帧 | [ztuio](https://github.com/LIghtJUNction/ztuio) |
| `crates/zharness` | `zharness = 0.5.1` | 原生扩展接口与调用原语 | [zharness](https://github.com/LIghtJUNction/zharness) |
| `crates/zplugin` | `zplugin = 0.3.1` | 显式指定的本机动态库加载与 ABI 校验 | [zplugin](https://github.com/LIghtJUNction/zplugin) |

三个库的 Git 提交均由父仓库的 gitlink 固定，`.gitmodules` 记录公开仓库地址。ztuio 固定到 `9c632b30f7e2c2fd51a19b4bc5d90d529041ae7e`，复用独立仓库合并后的公共 API、ANSI 帧优化与契约测试，父仓库不再保留另一份终端核心源码。新包名和 Rust 导入名均为 `ztuio`；其版本由独立仓库维护。精确版本写在 [Cargo.toml](../Cargo.toml)，依赖解析结果写在 [Cargo.lock](../Cargo.lock)，日常构建不会跟随任何子库的 `main` 自动更新。

zharness 0.5.1 固定到 `e3580c01c7252da090d04e59b59762bc561777d1`，zplugin 0.3.1 固定到 `00d566eedcff7b2932e90b9dfeb44910c783d305`；宏包也使用 0.3.1。两者是已正式发布的配对版本，zharness 的精确 zplugin 依赖与应用一致。zplugin 通过 `zsys = 0.1.1` 的 `dylib` 特性复用系统动态库加载，显式关闭与默认保留映像的生命周期约定不变。应用继续启用 zplugin 的 `std, macros`，并保留 zcall 0.3.0、zconv 0.4.0；本次依赖更新不改变应用版本。来源、摘要与后续验证范围见[配对升级记录](reports/zharness-zplugin-pair-20261120/source-checkpoint.json)。

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

ztuio、zharness、zplugin 均从父 workspace 显式排除，应用通过路径依赖使用固定源码。ztuio 虽然是单包仓库，也明确排除，以免 Cargo 自动把路径依赖收入父 workspace。根级 `[patch.crates-io]` 将 zharness 传递使用的 zplugin 统一到同一个子模块，防止本机构建出现两套相同 ABI 类型。

父项目的 `cargo test --workspace` 不会执行已排除子库的独立契约测试。ztuio 使用 `cargo test --manifest-path crates/ztuio/Cargo.toml --locked` 验证；正式发布复用的 CI 配置也保留此显式检查。更新子库时，应在该子库执行自己的测试和发布检查，再对 ssmcp 做集成验证。发布到 crates.io 时使用精确 registry 版本；不把子库的正式依赖改成只在相邻目录才成立的路径。

版本按实际变更分别递增。ssmcp 的版本递增不自动递增三个库。首次发布或升级公共依赖时，先发布对应库并核对包摘要，再发布应用。zplugin 的宏包先于 zplugin，新的 zharness 版本应依赖已发布的精确 zplugin 版本；ztuio 同样从自己的仓库发布，SSMCP 工作流只发布应用包。

## 执行边界

ssmcp 继续使用自己的工具目录、运行时 hook、输出边界和会话历史。接入原生扩展不替换这些机制，也不创建第二套模型循环。

本机动态库是调用者明确选择的受信任代码，加载时可能运行初始化逻辑。ABI 校验检查接口能否正确交换数据，不是文件或网络沙箱；同步本机调用也不能承诺被强制取消。该限制来自本机动态链接本身，不能用超时或界面状态冒充副作用回滚。

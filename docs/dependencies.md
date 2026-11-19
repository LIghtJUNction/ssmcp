# 独立核心库与子模块

ssmcp 把终端绘制、原生扩展接口和动态库加载拆成三个独立 Rust 库。编译器将这些库连进客户端；它们本身不会启动模型、读取其他 Agent 的账号，或扫描机器上安装的 harness。外部 Codex、Claude Code、Pi、OpenCode 和 Playwright MCP 仍遵循[显式加载](optional-harnesses.md)。

| 源码目录 | 固定库版本 | 在 ssmcp 中的职责 | 独立仓库 |
|---|---|---|---|
| `crates/ztui` | `ztui-core = 0.1.1`，Rust 库名 `ztui` | 现有终端布局、文本行和差分帧 | [ztui](https://github.com/LIghtJUNction/ztui) |
| `crates/zharness` | `zharness = 0.5.0` | 原生扩展接口与调用原语 | [zharness](https://github.com/LIghtJUNction/zharness) |
| `crates/zplugin` | `zplugin = 0.3.0`（[源码候选](reports/zplugin-consumer-20261120/README.zh-CN.md)） | 经 zsys 0.1.1 加载显式指定的本机动态库，并校验 ABI | [zplugin](https://github.com/LIghtJUNction/zplugin) |

zharness 和 zplugin 的 Git 提交由父仓库的 gitlink 固定；`.gitmodules` 只记录这两个公开仓库地址。ztui-core 保留父仓库中的普通源码目录及主线已有的 0.1.1 版本；当前独立 ztui 仓库仍为 0.1.0，因此本轮不把它改成会降版的子模块。两份终端运行时源码、测试、示例和基准一致，后续版本对齐后再迁移其源码来源。精确版本写在 [Cargo.toml](../Cargo.toml)，依赖解析结果写在 [Cargo.lock](../Cargo.lock)，日常构建不会跟随任何子库的 `main` 自动更新。

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

更新 zharness 或 zplugin 时，先在相应子库提交并推送修改，再在 ssmcp 更新 gitlink 和精确依赖版本。提交父仓库前检查 `git submodule status --recursive`；不要只提交父仓库指针而遗漏子库提交。ztui-core 目前直接修改父仓库内的源码目录，版本继续独立维护。

ztui 保留为 ssmcp workspace 成员。zharness、zplugin 自带独立 workspace，父 workspace 显式排除其目录，并通过路径依赖使用它们。根级 `[patch.crates-io]` 将 zharness 传递使用的 zplugin 统一到同一个子模块，防止本机构建出现两套相同 ABI 类型。

父项目的 `cargo test --workspace` 不会替代两个独立 workspace 的测试。更新子库时，应在该子库执行自己的测试和发布检查，再对 ssmcp 做集成验证。发布到 crates.io 时使用精确 registry 版本；不把子库的正式依赖改成只在相邻目录才成立的路径。

版本按实际变更分别递增。ssmcp 的版本递增不自动递增三个库。首次发布或升级公共依赖时，先发布对应库并核对包摘要，再发布应用。zplugin 的宏包先于 zplugin，新的 zharness 版本应依赖已发布的精确 zplugin 版本；ztui-core 独立先于依赖它的 ssmcp。

## 执行边界

ssmcp 继续使用自己的工具目录、运行时 hook、输出边界和会话历史。接入原生扩展不替换这些机制，也不创建第二套模型循环。

本机动态库是调用者明确选择的受信任代码，加载时可能运行初始化逻辑。ABI 校验检查接口能否正确交换数据，不是文件或网络沙箱；同步本机调用也不能承诺被强制取消。该限制来自本机动态链接本身，不能用超时或界面状态冒充副作用回滚。

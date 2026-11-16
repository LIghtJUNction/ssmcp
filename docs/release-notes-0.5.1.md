# ssmcp 0.5.1 更新说明

0.5.1 固定版本发布的触发条件和源码包验证链，隔离发布测试使用的 Git 配置，并包含 macOS 原生进程清理的边界处理。本文记录版本变更；是否已经完成发布，以对应的 [Actions 运行][actions]、[GitHub Release][releases] 和 [crates.io 版本][crates] 为准。

## 版本 tag 启动发布，空参数只做预检

推送与 `Cargo.toml`、`Cargo.lock` 一致的 `vX.Y.Z` tag，会自动启动发布流程；该 commit 必须已经进入 `main`。手动填写已有 tag 可重试同一版本。在 `main` 手动运行 Release 并留空 tag，以及创建或更新 `release-check` 分支，都只运行检查、生成 Actions artifacts，不创建 tag 或发布版本。普通 `main` push 与 pull request 不触发日常 CI。[发布工作流][workflow] · [事件判定与回归测试][tests]

手动 `Bump version` 继续支持递增版本、提交、创建匹配 tag，再显式启动该 tag 的 Release；这项操作与空 tag 预检分开。安装方式、六个平台归档及失败重跑步骤见[发布指南][guide]。

## 源码包与预编译产物共同通过验证

`finalize` 等待六个平台的检查与构建成功，核对完整归档集，再运行 `cargo package --locked` 验证源码包。验证后的 `.crate` 保存为 `verified-crate` artifact；crates.io 与 GitHub Release 两个 job 共用这一验证结果，分别报告发布状态。`finalize` 使用只读仓库权限，本身不创建 tag。[发布工作流][workflow] · [包与附件校验][script]

发布页保留安装说明，并附加与包版本匹配的发布说明；源码包也包含这些说明。crates 发布继续比较包摘要，GitHub Release 继续核对附件内容，已有版本或附件不会因重跑而被静默替换。

## 发布测试不依赖开发机 Git 设置

临时测试仓库使用空模板，关闭 commit／tag 签名和 hooks，并隔离全局、系统及继承的 Git 环境配置。fixture 创建失败时会显示命令、工作目录、退出码和 Git 的 stdout／stderr，保留失败结果供定位；不会跳过测试或改变生产发布校验。[测试 fixture][tests]

## macOS 清理使用有界的原生扫描

macOS 通过 `libproc` 查找受管理会话中的进程组，并限制扫描容量、轮次和时间预算。发送终止信号前重新核对会话、进程组和创建时间，以缩小进程标识复用的竞争窗口；该平台仍存在非原子检查的边界。终端关停采用有界等待，输出尚未到 EOF 时将捕获结果标记为不完整，避免把未收齐的日志报告为完整。[进程清理实现][platform] · [原生终端关停][pty]

客户端仍提供 Linux、macOS、Windows 的 x86_64／ARM64 构建目标，网关 `ssmcpd` 与原生 rsync 端点仍限于 Linux。某个版本的跨平台验证通过与实际发布完成是独立状态，应分别查看运行结果和下载记录。

[workflow]: https://github.com/LIghtJUNction/ssmcp/blob/main/.github/workflows/release.yml
[script]: https://github.com/LIghtJUNction/ssmcp/blob/main/scripts/release.py
[tests]: https://github.com/LIghtJUNction/ssmcp/blob/main/scripts/test_release.py
[guide]: https://github.com/LIghtJUNction/ssmcp/blob/main/docs/releases.md
[platform]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/platform.rs
[pty]: https://github.com/LIghtJUNction/ssmcp/blob/main/src/pty_portable.rs
[actions]: https://github.com/LIghtJUNction/ssmcp/actions/workflows/release.yml
[releases]: https://github.com/LIghtJUNction/ssmcp/releases
[crates]: https://crates.io/crates/ssmcp

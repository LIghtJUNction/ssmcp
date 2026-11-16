# ssmcp 0.5.1 发布验收报告

日期：2026-11-16

## 发布结果

**ssmcp 0.5.1 已发布到 crates.io 和 GitHub Releases。** GitHub 的稳定版 Release 已公开并设为 latest，包含六个平台归档、两份安装器、六份单包校验文件、SHA256SUMS 和 release-manifest.json，共 16 个附件。标签推送触发的发布工作流在失败任务重跑后整体成功。[GitHub Release](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.5.1) · [crates.io](https://crates.io/crates/ssmcp/0.5.1) · [发布运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446)

| 对象 | 精确记录 |
| --- | --- |
| 发布标签 | `v0.5.1` |
| 发行源码 | `c75309a9acab27c684c0beaaf9d22fd991200760` |
| 标签触发的工作流 | `43838379446`，事件 `push`，最终结果 `success` |
| 发布后追加的草稿恢复修复 | `95f24966fdd7dfe16bbb79f8dfce93f0b9a36d52`，已进入远端 `main` |
| 完整校验记录 | [release-0.5.1-verification.json](release-0.5.1-verification.json) |

发布后的自动化修复没有移动标签，也没有替换已经上传到 crates.io 的包。发行源码与后续发布工具修复分别记录，便于重现这次构建。[发行提交](https://github.com/LIghtJUNction/ssmcp/commit/c75309a9acab27c684c0beaaf9d22fd991200760) · [自动化修复](https://github.com/LIghtJUNction/ssmcp/commit/95f24966fdd7dfe16bbb79f8dfce93f0b9a36d52)

## 本轮项目迭代

0.5 系列的统一客户端能力已包含在本次发行源码中：同一连接提供 Shell 终端、独立后台命令、文件和上下文操作，并发现本机已安装的 Codex、Claude Code、Pi、OpenCode 原生能力。命令提供启动、等待、增量读取、取消和事件查询；目录分页保留快照，避免扫描过程中能力目录变化导致漏项。[功能与迁移说明](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/docs/release-notes-0.5.0.md)

本轮还完成了 Windows ConPTY、macOS 终端和进程生命周期适配、跨平台文件与输出处理、客户端重载，以及发布包安装器。macOS 清理采用有界的原生进程扫描，并在发送信号前重新核对身份；该平台的检查仍不具有 Linux pidfd 的原子身份保证。[0.5.1 说明](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/docs/release-notes-0.5.1.md) · [原生客户端验证](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/tests/release_client.rs)

升级旧客户端时，需要移除原来的 `-t`、`--type`、`--backend` 和尾随 Agent 参数。先更新 Linux 网关，再使用新版客户端建立连接；各 CLI 的账户、模型与扩展继续使用其自身配置。[迁移步骤](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/docs/release-notes-0.5.0.md#升级时先修改启动命令)

## 预编译产物

| 系统 | 架构 | 下载 | 包含程序 |
| --- | --- | --- | --- |
| Linux GNU | x86_64 | [ssmcp-x86_64-unknown-linux-gnu.tar.gz](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/ssmcp-x86_64-unknown-linux-gnu.tar.gz) | `ssmcp`、`ssmcpd` |
| Linux GNU | ARM64 | [ssmcp-aarch64-unknown-linux-gnu.tar.gz](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/ssmcp-aarch64-unknown-linux-gnu.tar.gz) | `ssmcp`、`ssmcpd` |
| macOS | Intel x86_64 | [ssmcp-x86_64-apple-darwin.tar.gz](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/ssmcp-x86_64-apple-darwin.tar.gz) | `ssmcp` |
| macOS | Apple Silicon ARM64 | [ssmcp-aarch64-apple-darwin.tar.gz](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/ssmcp-aarch64-apple-darwin.tar.gz) | `ssmcp` |
| Windows | x86_64 | [ssmcp-x86_64-pc-windows-msvc.zip](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/ssmcp-x86_64-pc-windows-msvc.zip) | `ssmcp.exe` |
| Windows | ARM64 | [ssmcp-aarch64-pc-windows-msvc.zip](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/ssmcp-aarch64-pc-windows-msvc.zip) | `ssmcp.exe` |

Linux GNU 包的构建基线为 glibc 2.35。网关 `ssmcpd` 和原生 rsync 端点限于 Linux；macOS 与 Windows 发行客户端。客户端依赖系统 OpenSSH。每个平台都在对应架构的原生 runner 上执行检查和测试，并对生成的归档验证安装、更新及失败下载处理。[发布矩阵](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/.github/workflows/release.yml) · [CI](https://github.com/LIghtJUNction/ssmcp/blob/v0.5.1/.github/workflows/ci.yml)

## 自动发布规则

| 入口 | 行为 |
| --- | --- |
| 推送匹配版本的 `vX.Y.Z` 标签 | 自动检查、构建、打包并发布 |
| 在 main 手动运行 Release，填写已有标签 | 发布或重试该标签 |
| 在 main 手动运行 Release，标签留空 | 仅预检，生成 Actions artifacts |
| 创建或更新 `release-check` 分支 | 检查 main，仅预检 |
| 手动运行 Bump version | 更新 manifest/lock 版本、提交 main、创建标签并显式触发该标签的 Release |
| 普通 main push / pull request | 不触发日常 CI 或发布 |

发布先检查标签、manifest、lockfile 和源码归属，再运行六个平台的检查、构建与安装验证。只有这些任务成功，`finalize` 才核对完整归档集并运行 `cargo package --locked`；通过验证的 crate 保存为 `verified-crate`。之后 crates.io 与 GitHub Release 两个任务分别发布并报告结果。仓库已配置 `CARGO_REGISTRY_TOKEN`。[工作流](https://github.com/LIghtJUNction/ssmcp/blob/main/.github/workflows/release.yml) · [Bump](https://github.com/LIghtJUNction/ssmcp/blob/main/.github/workflows/bump.yml)

标签、版本或已有附件不一致会使发布失败。重试保留既有产物的字节一致性检查。失败时优先重跑失败任务，以便复用本次运行已经验证的产物。[发布脚本](https://github.com/LIghtJUNction/ssmcp/blob/95f24966fdd7dfe16bbb79f8dfce93f0b9a36d52/scripts/release.py)

## 验证结果

| 验证范围 | 结果 |
| --- | --- |
| 开发机格式、Clippy 和常规 Rust 测试 | 通过 |
| Node.js / bubblewrap 隔离、two_tools、workspace_bridge、context_helpers | 显式运行相应忽略测试，通过 |
| 真实 SSH、OAuth、HTTP、PTY、注册信息重启持久化、会话标题、重载与输出附件 | 端到端测试通过 |
| Codex / Claude / Pi / OpenCode 协议集成 | 隔离的本地协议夹具测试通过 |
| 六个平台的原生检查与发行包构建 | 全部通过 |
| 六个平台安装、更新、损坏包、错误版本与下载失败场景 | 全部通过 |
| 源码包 | 本地 `cargo publish --locked --dry-run` 和工作流 `cargo package --locked` 通过 |
| 草稿恢复修复后的脚本测试 | `test_*.py` 共 45 项通过 |
| 发布后的下载复核 | 16 个附件的大小与摘要、manifest、SHA256SUMS、单包 sidecar 均通过 |
| crates.io 下载包 | SHA-256 与 registry 记录一致；内嵌 VCS SHA 与发行提交一致 |
| 发布页 Linux 包实机运行 | `ssmcp --version` 与 `ssmcpd --version` 均返回 0.5.1，`--help` 正常 |

常规测试中需要显式提供官方 CLI 二进制的选择性测试仍保留为 opt-in；本次后端集成验证使用协议夹具，未发起模型任务。详细证据与各任务结果见 [校验记录](release-0.5.1-verification.json) 和 [标签发布运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/43838379446)。

crates.io 包 SHA-256：

```text
290b276de95d2bfd20792338445cd1e48c48dd2c356704277261c38bbeb04fb9
```

## 发布期间修复的问题

第一次 GitHub 发布任务已经创建草稿，但随后按 tag 查询草稿时收到 404。GitHub 将该接口定义为获取已发布 Release；认证的 Release 列表可以返回草稿。修复后的脚本分页查找并复用同 tag 的草稿，取得 Release ID 后按 ID 读取和核验附件。多个同 tag 记录会被拒绝，已有附件仍受大小及摘要保护。[GitHub 官方 API 定义](https://docs.github.com/en/rest/releases/releases#get-a-release-by-tag-name) · [修复提交](https://github.com/LIghtJUNction/ssmcp/commit/95f24966fdd7dfe16bbb79f8dfce93f0b9a36d52)

本次恢复使用标签运行已经生成的六份原始构建产物，补齐附件后公开同一个 Release。随后失败任务重跑成功，整体工作流通过；crates.io 已发布的包保持原摘要。原始标签与发行源码始终为 `c75309a`。

## 安装与下一次发版

从 crates.io 安装客户端：

```sh
cargo install ssmcp --version 0.5.1 --locked --bin ssmcp
```

也可以使用发布页的 [Unix 安装器](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/install.sh) 或 [Windows 安装器](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/install.ps1)。下载归档时，可对照同页的 [SHA256SUMS](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/SHA256SUMS) 与 [源码/附件清单](https://github.com/LIghtJUNction/ssmcp/releases/download/v0.5.1/release-manifest.json)。

下一次递增 patch 版本并启动发布：

```sh
gh workflow run bump.yml --repo LIghtJUNction/ssmcp --ref main -f level=patch
```

只做预检：

```sh
gh workflow run release.yml --repo LIghtJUNction/ssmcp --ref main
```

## 工作区与交付记录

主工作区已切回 main；在最后一次成功检查时，本地源码为发行提交 `c75309a`，工作树干净。原有未提交内容保留在 stash 与恢复快照中。发布工具的后续修复已提交到远端 main；开发机会话随后离线，报告最终提交后的本地同步未再次复核。

本报告与 JSON 校验记录提交到仓库，并另存至 [Google 云盘交付目录](https://drive.google.com/drive/folders/1D2xFBWr-PKi2XepthoqNwEwuwo8O1lCM)。

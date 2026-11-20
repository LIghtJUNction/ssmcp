# v0.10.14 跨平台发布检查与修复

## 实际失败

固定标签 v0.10.14 指向 0713001875c472f7c1cd0686330b76e3d919463c。[正式发布运行 44661834953](https://github.com/LIghtJUNction/ssmcp/actions/runs/44661834953) 的发布策略检查已确认 publish=true、deploy_server=false，但跨平台客户端检查没有通过。标签和原始失败记录保持不变；此修复供下一次候选发布使用。

| 平台与任务 | 严格 Clippy 实际失败 |
| --- | --- |
| macos-15，136041887639 | listener.rs 的 Owner::Local 没有生产构造路径 |
| macos-15-intel，136041887665 | 同上 |
| windows-2025，136041887655 | 上述问题；link_metrics.rs 的 Status::Ready 没有生产构造路径；zuse_backend.rs 的 DirBuilder 变量不需要 mut |
| windows-11-arm，136041887535 | 与 Windows x64 相同 |

失败命令为 cargo clippy --locked --lib --bin ssmcp --test release_client -- -D warnings。上述诊断来自实际任务日志，不是从 Linux 检查推断的平台结果。

## 最终工作流结果

GitHub API 读回确认运行已完成，结论为 failure；API 记录的更新时间为 2026-11-20 02:44:05 UTC。完整任务回执见 [formal-run.json](formal-run.json)。

六个平台的优化构建、打包和安装验证任务全部成功；Linux x86_64 与 ARM64 两项安装后服务运行检查也成功。六个检查任务失败：四个 Windows/macOS 客户端严格 Clippy，以及两个 Linux workspace 测试任务。两种 Linux 架构的配置测试错误相同。

finalize、publish_crates、publish_release 和 deploy_server 四个下游任务均被跳过。本轮留下可复核的 Actions 构建产物，但没有发布新的正式资产、crate 或生产服务。新候选将合入修复，并在固定下一个标签前运行跨平台正式预检。

## 修改与边界

Local 由 Linux 的内核对端凭据传输创建；其他平台的生产构建只使用 Remote。共用的所有者模型也服务于跨平台作用域单元测试，因此仅对非 Linux、非测试构建中的这个枚举项标注允许未构造。

Ready 由 Unix 数据报采样器和单元测试创建；便携传输明确报告 Unavailable。仅对非 Unix、非测试构建中的这个枚举项标注允许未构造，保留共用渲染和采样测试模型。

截图临时目录的 DirBuilder 在 Unix 配置权限时才需要可变访问。现在将可变绑定限定于 Unix 块，仍设置 0700；Windows 使用原来的默认目录构建行为。

这些修改没有关闭工作流的 -D warnings，没有给模块或整个枚举加豁免，也没有更改 Linux 的认证、显示分配、遥测或目录权限行为。

## 验证状态

该提交只完成三处源码的 rustfmt 检查和 git diff --check。下一候选构建仍须通过实际平台的严格 Clippy 和完整发布检查；此文档不声明 Windows 或 macOS 已修复通过。

同一正式运行还在 Linux ARM 的 tests/client_server_config.rs 暴露了四个过时接口预期：测试仍调用已移除的 invite/join，或期待旧账号/错误文案。该问题由独立的配置测试修复处理，不能通过忽略测试或放宽发布门禁解决。

# 日志与部署加固验收 — 2026-11-16

本轮在独立工作区完成客户端输出和服务端诊断改造；保留主工作区未提交内容，未启动新的模型或 AI 子任务。

## 本机验证

| 检查 | 结果 |
| --- | --- |
| cargo test --locked | 208 passed，13 ignored，0 failed（合并 context primitives 后重跑） |
| code_mode::tests --include-ignored | 9 passed，其中含默认忽略的系统隔离测试 |
| two_tools --include-ignored | 3 passed |
| workspace_bridge --ignored --test-threads=1 | 3 passed |
| scripts/test_registry_restart.py | 8 项真实进程/HTTP/重启检查通过 |
| client::（最终显示改动后重跑） | 27 passed |
| client_startup | 6 passed，含 SSH/native stderr/lifecycle JSONL 逐行解析 |
| cargo clippy --locked --all-targets -- -D warnings | 通过 |
| cargo fmt --all | 通过 |
| Caddyfile adapt | 成功 |
| systemd-analyze verify | ssmcpd 单元验证通过 |

合并主分支的 context primitives 后保留 --context-root 与全部日志参数，整套测试及 Clippy 已重跑。

附加测试与常规测试存在重叠，不把上述数量相加作为独立测试总数。未执行的原生 Agent 实装测试仍按其前置条件保留 ignored。

客户端回归覆盖颜色策略、无色输出、主题与 UTC 时间戳、未知配置拒绝、大输出折叠与尾部保留、原始 JSON 不变、错误/超时、CR 下载进度、多进程 stdout/stderr 分隔以及 diff 每个分片边界。服务器回归覆盖 RUST_LOG 分类、日志字段白名单/换行转义、唯一请求 ID、客户端登记跨重启恢复、0600 权限、到期时间不延长和损坏状态拒绝启动。

截图由真实 Rust 渲染器的固定事件生成。运行 terminal_preview_fixture 测试并设置 SSMCP_RENDER_PREVIEW 指向 ANSI 文件，再用 scripts/render_log_preview.py 生成 PNG。图片用于样式检查，不是生产执行耗时证据。

## 线上验收

生产升级单独写入 reports 中的部署回执。源码提交、本机测试、预演通过和生产已生效分别记录；本文件不替代实际部署回执。重启后 DCR 登记保留，当前 OAuth 授权及 SSH 会话需要重新建立。

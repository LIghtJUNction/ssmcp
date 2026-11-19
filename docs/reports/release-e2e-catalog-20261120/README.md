# 当前公开目录的正式发布 E2E 验收

SSMCP 0.10.14 的四条正式发布检查路径已经使用同一组冻结产品程序通过。严格目录预期从已退休的 invite 工具更新为当前三项 list、code_mode、collab；没有删掉目录相等性检查，也没有修改产品工具实现。

| 检查 | 通过项 | 使用路径 |
| --- | --- | --- |
| 基础 MCP | 125 | 真实账号 WebSocket、OAuth、HTTP 与 PTY |
| agent 夹具 | 475 | 合成 harness 协议、显式加载与隔离 |
| 统一执行 | 105 | 命令、审计、归属与连接隔离 |
| 后台服务 | 182 | 真实用户 systemd、重连、watchdog 与清理 |

这些程序共享部分初始化和协议检查，数量不能相加解释为互不重复的场景。

## 来源与结果

产品来源为 b49da12ac890f4d2b00c42d9dcf05b97e2054bac，脚本修复为 4aa018b84973044420547c03c241f237dcdd77e6。账号路由 fixture 来自全库通过的 fab35e11425cdd147434de6d2eaa20510bf6db1e 测试程序；产品 CLI、daemon 与可选 frontend 始终复用原冻结版本，结束后重新核对三者哈希与冻结清单完全一致。

[validation.json](validation.json) 记录源码、程序摘要、运行耗时及原始/公开报告摘要。逐项检查分别见 [基础 MCP](base.json)、[agent 夹具](agents.json)、[统一执行](unified.json) 和 [后台服务](daemon.json)。公开副本替换了开发机与临时夹具路径；原始报告和日志在受控工作树保留。

这些路径实际运行 CLI 与传输/服务管理逻辑。harness 使用合成协议夹具，不调用模型，也不向操作者的外部模型会话提交任务。后台服务使用自己的隔离会话和资源，清理结果见报告；本次没有操作现有用户客户端或部署生产服务器。

## 范围

本报告解决公开工具目录在正式 E2E 中的旧预期。账号 relay 的旧 invite/join 迁移由独立报告记录；六平台 Release、crate 上传与部署仍以正式工作流末态为准。默认库测试另见 [组合验收](../zuse-integration-20261120/library-gate.json)。

# ssmcp 0.7.0 服务端部署验收

服务端已更新到 **0.7.0**，网关与账户门户已重启。重启后重新通过 MCP 身份验证，原有 **6 个客户端全部恢复在线**；通过新的连接在 ssmcp 项目执行原生命令，返回 `SSMCP_RECONNECT_OK`，退出码为 **0**。

- 服务端部署完成：2026-11-17 22:46:49 UTC+08:00。
- 重连验收完成：2026-11-17 22:52:39 UTC+08:00。
- 源提交：[6d2802b](https://github.com/LIghtJUNction/ssmcp/commit/6d2802b8533314fb8a43a962d88d74165c026b0c)。
- 构建命令：`cargo build --locked --release --bins -j 2`，退出码 0。

## 运行版本

| 对象 | 实测结果 |
| --- | --- |
| 安装包 | `ssmcp-git 0.7.0.r171.g6d2802b-1` |
| 网关 `ssmcpd.service` | `active`，实际进程版本 `ssmcpd 0.7.0` |
| 门户 `ssmcp-portal.service` | `active`，实际进程版本 `ssmcpd 0.7.0` |
| 两个服务的可执行文件 | `/usr/bin/ssmcpd` |
| 自动重启次数 | 两个服务均为 0 |
| 进程与安装文件一致性 | 两个实际进程的 SHA-256 均与已验证制品一致 |

已移除旧版本的两份 `90-portal-console.conf` 启动覆盖配置，服务使用新安装包中的 systemd unit。

## 部署与重连验收

20 项隔离预演检查和 20 项正式部署检查全部通过。完整布尔结果见 [staging.json](staging.json) 和 [verification.json](verification.json)。

| 验收项 | 结果与证据 |
| --- | --- |
| 隔离预演 | 网关与门户使用独立端口、控制 socket、管理 socket、OAuth 登记、账户存储和审计目录；生产进程与配置保持原状态 |
| OAuth 持久化 | 预演中的 DCR 客户端登记在重启后仍可进入授权流程 |
| 健康检查 | 网关、门户及公网 `/healthz` 均返回 HTTP 200 和 `ok` |
| OAuth 发现 | 公网元数据 issuer 与配置的 HTTPS origin 一致，包含 refresh-token 能力 |
| 未授权访问 | 不带授权的 `/mcp` 请求返回 HTTP 401 |
| 门户入口 | 公网首页返回 HTTP 200 |
| 连接码校验 | 不带有效连接码的 `/setup` 请求返回 HTTP 410，符合其短期授权流程 |
| 配置保留 | TOML、环境配置及 Nginx 配置的摘要与部署前一致 |
| 私有文件权限 | 管理 socket 为 0600，审计目录为 0700 |
| 实际重连 | 已重新通过认证调用 `ssmcp.list`，6/6 个原有客户端恢复，客户端 ID 已更新 |
| 实际命令调用 | 经重连后的 `ssmcp-manager` 执行原生命令，输出 `SSMCP_RECONNECT_OK`，退出码 0 |

管理员设置与 7 天审计保留配置沿用现有 TOML。本次保留了对应配置，并以部署前后摘要一致作为验收条件。

## 制品身份

安装包 SHA-256：

```text
fffa331b698964d6853dbd0ff47fc0b5ece73981bdc8c52f2431653e6000ec30
```

实际运行的 ssmcpd SHA-256：

```text
1fdaf72fc647a29c7574534965ef75998cb8478450d5324d6c192898ae28bafb
```

code-mode.js SHA-256：

```text
260b06a3d82b848ac6f00c9c979560b681c1033704a1343064bfa45882846a58
```

## 回滚备份与服务端回执

私有检查点：

```text
/var/lib/ssmcp-rollouts/6d2802b8533314fb8a43a962d88d74165c026b0c-0.7.0-final
```

检查点保留已验证的新安装包、当前安装包的回滚归档 `rollback.pkg.tar.gz`、部署前配置与所有权信息、部署执行脚本、`stage-result.json`、`apply-result.json` 和独立的最终 `verification.json`。原来运行的 0.6.4 release 文件也保留，回滚可恢复对应启动覆盖配置。

临时预演 release、失败预演检查点和上传中转副本已清理，最终回滚备份保留在上述私有目录。

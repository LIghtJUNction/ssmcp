# ssmcp 执行通道检查与迭代状态

## 当前结论

本次直接调用 ssmcp 的结果是：`list` 正常返回在线客户端，`collab list` 正常返回；`code_mode` 在网关端启动编排器之前失败。对只包含 JavaScript 输出、完全不访问开发机的调用也返回同一错误：

```text
code_mode runner is missing; run make install-server or set SSMCP_CODE_MODE_RUNNER to its installed absolute path
```

因此，当前能发现开发机，但不能通过这一入口读取文件、执行命令或向终端发送进度。原客户端连接已离线，当前列表包含同仓库的 `light-2449230`。改变客户端 ID 不能修复网关进程的 runner 查找失败。

## 源码依据

核对版本：main `b4acca9b7e3e5f22b205f2c60bd0f5d33a18094d`。

[runner 查找实现](https://github.com/LIghtJUNction/ssmcp/blob/b4acca9b7e3e5f22b205f2c60bd0f5d33a18094d/src/code_mode.rs) 依次检查服务进程的 `SSMCP_CODE_MODE_RUNNER`、实际可执行文件两级父目录下的 `libexec/ssmcp/code-mode.js`，并仅在 debug 构建允许源码目录回退。当前精确错误意味着默认查找没有找到可用文件；实际是漏装、目录被清理还是权限/挂载不可见，尚未在网关主机核实。

[Makefile](https://github.com/LIghtJUNction/ssmcp/blob/b4acca9b7e3e5f22b205f2c60bd0f5d33a18094d/Makefile) 的 install 目标会安装 runner，权限为 0644。[部署脚本](https://github.com/LIghtJUNction/ssmcp/blob/b4acca9b7e3e5f22b205f2c60bd0f5d33a18094d/scripts/deploy_server.py) 同样将二进制与 runner 一起解包。源码已包含正确安装步骤，本次没有在未检查现场的情况下修改安装逻辑。

## 恢复操作

在运行网关 `ssmcpd` 的主机上，先确认实际进程路径，而不是依据客户端开发机的路径：

```bash
ssmcpd_pid="$(systemctl show ssmcpd.service -p MainPID --value)"
readlink -f "/proc/${ssmcpd_pid}/exe"
```

上面使用仓库提供的标准 unit 名称；如部署使用自定义 unit，应换成现场名称。

| 实际 daemon 路径 | 默认 runner 路径 |
| --- | --- |
| /usr/local/bin/ssmcpd | /usr/local/libexec/ssmcp/code-mode.js |
| /usr/bin/ssmcpd | /usr/libexec/ssmcp/code-mode.js |
| /opt/ssmcp/releases/COMMIT/usr/bin/ssmcpd | /opt/ssmcp/releases/COMMIT/usr/libexec/ssmcp/code-mode.js |

从与运行中 daemon 相同构建或安装包中，把 runner 补齐到对应位置，并保证服务用户可以遍历目录、读取文件。通常目录权限为 0755、文件为 0644。随后用 `print("runner-ok")` 验证 code_mode，再验证对目标客户端的一次只读调用。

每次执行都会重新查找 runner，因此补齐正确文件原则上无需先重启服务。若采用环境变量方案，`SSMCP_CODE_MODE_RUNNER` 必须配置到网关服务进程；在开发机客户端 shell 中 export 无效。

本次没有访问网关文件系统的独立 SSH 入口，且 ssmcp 执行入口本身被上述问题阻断；没有声称已经完成现场恢复，也没有重启或替换用户客户端。

## 已提交的迭代修正

PR：[conversation canvas, native task panel and scoped skills #39](https://github.com/LIghtJUNction/ssmcp/pull/39)。

最新测试准备状态修正：[3f4f6b12](https://github.com/LIghtJUNction/ssmcp/commit/3f4f6b12bc65db13860ebccaa42a9f8eaa43ecff)。

画布聚焦 Agents 时会恢复稳定的选中成员 ID；滚动列表测试随后手动设置索引 15，却保留原选中 ID，导致渲染重新选择首项。修正只在该测试中清除旧稳定 ID，保留所有滚动、可见行鼠标选择、标题不触发选择以及返回位置断言。生产选择逻辑没有因此修改。

## 验证边界

- 通道中断前，组合客户端测试实际结果为 **581 通过、1 失败**；失败项正是上面的测试准备状态问题。
- `3f4f6b12` 已通过 GitHub 插件推送，但执行通道中断后尚未复测，不能报告为 582 项通过。
- 之前 `49cd343` 阶段的 **41 项真实 PTY 检查通过**，只证明该阶段；不能作为新增 Tasks、pet、退出等全部当前组合功能的验收结果。
- 最新 context 测试、组合候选构建和扩展 PTY 验收仍待执行。
- PR 继续保留 draft；本次检查中 GitHub 的 mergeable 已变为 true，没有已证实的合并冲突。
- 新版本发布和当前运行客户端升级尚未因本次操作完成。

恢复执行通道后，先同步已推送的修正并检查开发机未提交内容，再构建候选、复测失败项及剩余必要验收。最后以实际测试和发布结果更新 PR 与交付报告。

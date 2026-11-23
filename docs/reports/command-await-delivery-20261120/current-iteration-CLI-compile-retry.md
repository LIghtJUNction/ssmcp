# SSMCP 当前迭代：CLI 已整合，修复首轮编译问题后重试

**更新于 2026-11-21 16:25:27 UTC。** 新架构、等待提示与结束决策功能已整合到开发分支；首轮编译的两处错误已修正并进入重试。**最新用户要求进一步移除中间兼容入口，源码正在继续收口。****目前没有本轮迁移的完整 Rust、产品验收或发布通过回执。** [阶段事实](cli-compile-retry-and-H2-public-facts.json)

## 本轮交付范围

- 桌面和浏览器主操作直接使用 **Zuse CLI**；SSMCP computer 工具保留版本查询、下载、安装、卸载。
- 等待用户输入时显示**红色提示、符号和剩余 mm:ss 倒计时**。
- 准备结束本轮前进入**明确的 Human 决策等待**，绑定具体请求与调用方；普通输入、超时和取消均不充当结束许可。
- **按最新要求移除兼容入口。** 先前 f971／b0a4 恢复的本地 `/computer` 与 on/off/toggle/status 开关只是中间实现，已被新范围覆盖；源码负责人正在移除。默认 `Super + Alt + C` 继续退役。本报告组没有操作正在直播的机器。

上述行为已有固定源码静态核对，运行结果仍按实际候选逐阶段记录。

## 实际编译及修复

负责人报告，首轮固定 [`c56c040`](https://github.com/LIghtJUNction/ssmcp/commit/c56c040af31f1fa0e65a82773ced2eaf63d06ad8) 的锁定依赖获取、格式检查均以 0 退出；库编译以 **101** 退出，未进入测试二进制或产品运行。

| 实际诊断 | 已提交修复 |
|---|---|
| `E0599`：harness 绑定仍调用已退役的 `cancel_host_input()` | [`371d3e0`](https://github.com/LIghtJUNction/ssmcp/commit/371d3e090bd939ed3501c58858e2421e9367f031) 删除旧接线，不恢复执行器 |
| `E0616`：sleep 测试访问私有 `wait_fixture` | [`1358f84`](https://github.com/LIghtJUNction/ssmcp/commit/1358f84dff866970926f8b1c5c54ec0ba64e28cc) 仅在 `cfg(test)` 下允许父模块访问 |

报告组已独立读取这两份固定修改。第二轮使用 `1358f84`、独立的 v2 证据目录，保留 v1 失败证据并复用共享构建缓存；该回报为 fetch／fmt 已通过、库编译运行中。它仍带有随后被要求移除的兼容层，不能作为最终范围的通过证据。

此前共享编译锁阻塞已解除，`Cargo.lock` 已真实生成并在 [`35ff6e4`](https://github.com/LIghtJUNction/ssmcp/commit/35ff6e4eb8b463aeb090230e9557fb08fb22b458) 提交。早先的 blocked-v1 回执仅作为历史阶段保留。

## 发布核验适配

已根据固定 `35ff6e4` 准备并独立静态复核[新的 H2 静态副本](publication-verifier-git-source-zuse-cli-unbound/README.md)。本次只适配实际配方变化：三个 gitlink、Build 的唯一主上传改为 step 8，以及退役的 Windows 浏览器引导检查。Linux 辅助客户端仍要求在 Release 中跳过，finalize 两项证明上传保持。

19 个 job、17 成功／2 跳过的预期合同，16 个公开文件和 8 份必要发布证明附件，以及源码、原始请求、run、artifact 和文件字节的对应关系均保留。六份配方已读取并核对文件身份；这不是新的发布执行结果。**新旧 H2 均未由本报告组 import 或执行，最终 S／P／run／dispatch 绑定仍为空。**

## 已完成的前序工作

五个 Node 脚本已在固定 `031fc6e` 上以 0 退出，且原验收协调方独立读取了回执；这些结果不转移给后续提交。清理回执记录 72 个闲置缓存子目录，路径 `du` 累计约 118.7 GiB，最后对应磁盘可用约 429.5 GiB；该累计值不是净释放量。12 个远端历史分支的缺席已由本报告组独立核实。详见[首批检查与清理记录](current-iteration-CLI-first-checks-cleanup.md)。

较早的 Python 发布保障脚本由负责人报告 106 项通过；该消息未给出完整源码绑定，故不计为最新候选的验收。安装器检查当时因缺少必填 target 参数以 2 退出，未进入产品验证；需在新 archive 就绪后按正确参数完成。

## 尚待完成

继续取得修复后的真实编译与必要产品验证回执，再固定最终候选并完成已授权的新版本发布。报告组于 2026-11-21 16:23:46 UTC 读取 GitHub latest，返回 [v0.10.20](https://github.com/LIghtJUNction/ssmcp/releases/tag/v0.10.20)，本轮尚无新发布回执。旧 `02aa4dc` 的 Gate20／N20 及旧 H2、权限、EOF 附录均保持历史归属。

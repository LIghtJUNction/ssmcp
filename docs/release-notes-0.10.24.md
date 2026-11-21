# SSMCP 0.10.24

## 终端导航与发布验收

0.10.24 更新终端仪表盘的端到端验收，使导航检查对应当前布局。底部一行持续显示连接状态和电脑控制开关，审计列表的导航提示显示在面板标题。验收检查按 Escape 后审计面板与选择仍保留、输入焦点仍未返回编辑框，以及后续翻页和输入行为。[仪表盘验收](../scripts/e2e_dashboard.py)

本版继承 [0.10.23 的锁与跨平台修正](release-notes-0.10.23.md)，以及 [0.10.22 的本地 socket、Agent、线程消息和计时功能](release-notes-0.10.22.md)。0.10.23 的正式 Release 在 Linux 仪表盘验收停止，包发布未执行；其标签继续保留，本版使用新的版本身份。[0.10.23 发布运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/44843331041)

## 客户端资源测量

已保留冻结 0.10.23 候选与本轮基线的 12 次配对测量。六对结果中，候选空闲 CPU 均较低，RSS/PSS 与 40 条短命令批次耗时均略高。每种负载只有三对，数据描述这一轮完整改动的取舍，不用于归因某个单独修改或声明统计显著性。源码、二进制摘要、原始数值、配对范围与未测范围见[完整报告](reports/client-resources-2026-11-21/performance-report.md)和[原始运行表](reports/client-resources-2026-11-21/raw-runs.csv)。该测量保留原始 0.10.23 身份，不将版本号更新记为新一次测量。

## 依赖与部署

本版保持已冻结的依赖提交和源码 gitlink，Cargo.lock 只更新 ssmcp 自身版本。0.10.21、0.10.22、0.10.23 标签和历史验证记录均保持原样。[锁文件](../Cargo.lock)

Release 工作流继续延后生产服务器部署，0.10.24 的 tag 与手动发布均输出 `deploy_server=false`。客户端安装、发布包和生产服务器切换分别记录结果；服务端新增接口随中继单独升级后启用。[发布流程](releases.md) · [部署策略](../.github/workflows/release.yml) · [策略测试](../scripts/test_release.py)

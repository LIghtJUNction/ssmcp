# SSMCP 0.10.23

## 电脑输入开关与发布验收

0.10.23 改善多个本机会话同时切换电脑输入开关时的锁等待，修正 macOS 测试夹具，并同步原生工具目录的完整验收清单。连接、线程消息、真实回复、计时和终端功能沿用本轮已整合的实现；使用方式见 [0.10.22 发布说明](release-notes-0.10.22.md)。

用户明确执行 `on`、`off` 或 `toggle` 时，获取状态文件锁的等待预算调整为 2 秒，给其他会话完成一次持久化更新留出时间。状态观察、输入授权检查、初始化和自动撤销仍使用 100 毫秒预算；读取遇到锁竞争时按关闭处理。等待超时会明确说明锁获取阶段，成功获取锁后只执行一次读改写和落盘。[电脑控制](computer-use.md) · [状态存储实现](../src/computer/host_control/storage.rs)

回归测试使用独立文件句柄实际持锁，检查观察与撤销超时后状态保持原值、显式切换继续等待，以及放锁后提交新状态。原有四线程共十六次切换的并发用例继续检查更新序列。Windows ARM 的平台结果以本版正式发布检查为准。[存储回归测试](../src/computer/host_control/storage.rs) · [并发切换测试](../src/computer/host_control/tests.rs)

账号端到端验收清单包含保留的 `ssmcp.ui.reply`，并继续精确核对工具名称集合、数量、重复项和参数 schema。这个接口负责接收方提交实际线程结果，与已退役的结束审批接口分别处理。[账号验收](../scripts/e2e.py) · [线程结果回传](client-accounts.md) · [正常回复与用户问题](finish-and-questions.md)

macOS 的历史、目标、用户问题、计时与更新测试先解析临时目录的真实路径，再建立私有测试目录，适配系统临时目录的路径映射。测试继续执行原有的私有路径、会话归属与授权检查。[历史目录检查](../src/client/interface/history.rs) · [目标测试](../src/client/terminal/conversation/goals/tests.rs) · [计时测试](../src/runtime/time/tests.rs)

启动更新的超时测试同时观察客户端请求与真实 HTTP 连接，并为整个场景设置 5 秒上限。若请求没有到达测试服务器，场景会及时失败；正常路径仍检查 200 毫秒请求时限、连接关闭和缓存回退状态。[更新测试](../src/client/update/startup/tests.rs)

## 依赖与部署

本版沿用 0.10.22 的内部依赖快照，Cargo.lock 只更新 ssmcp 自身版本，各依赖提交和源码 gitlink 保持原值。0.10.21、0.10.22 的标签与发布记录继续保留，0.10.23 使用独立的验证记录和制品。[依赖维护](dependencies.md) · [锁文件](../Cargo.lock)

Release 工作流对 0.10.23 延后生产服务器部署，tag 推送与手动发布均输出 `deploy_server=false`。客户端安装、发布包和生产服务器切换分别记录结果；需要新版中继支持的等待与线程结果接口，随服务器单独升级后启用。[发布流程](releases.md) · [部署策略](../.github/workflows/release.yml) · [策略测试](../scripts/test_release.py)

# SSMCP 0.10.24

## 启动选项和对话

`--input` 选择聊天输入，默认 `terminal`。`--agent` 选择聊天处理方，默认 `next-caller`，由下一次符合条件的工具调用领取消息。用 `--list-harnesses` 查看原生协议，或用 `--load-harness codex` 在启动时加载 Codex 协议。原生尾参放在 `--` 后。只记录了旧协议含义 `--agent` 的会话需要用新选项重建。[客户端输入](client-input.md) · [原生协议加载](agent-entry.md)

`--input none` 关闭所选连接的聊天框和人类问答。命令、已协商的时间工具、独立原生消息和重载仍可使用。聊天启用时，`ssmcp.ui.reply` 按已认证的线程输入回传进度或结果。普通最终回复继续按会话归属和幂等回执保存。旧的结束审批和代理代答入口已退役。[线程消息](thread-messaging.md) · [问题与回复](finish-and-questions.md)

## 终端和电脑控制

终端批量解码粘贴内容，保留按键顺序、输入限制和草稿。显式选择 Next caller 后，消息仍由下一次符合条件的调用领取。找不到的 `@文件` 保留正文中的原始引用并提示。[输入与引用](client-input.md) · [粘贴回归定义](../src/client/terminal/conversation/tests/paste_batch.rs)

显式切换电脑输入状态最多等待状态锁 2 秒。观察、初始化和自动撤销仍使用 100 毫秒预算。私有桌面分配器释放持有对象时会显式解锁。[状态锁](../src/computer/host_control/storage.rs) · [私有桌面分配锁](../src/computer/isolated/display.rs)

## 验证与发布状态

本次准备 0.10.24 源码版本。正式构建、运行验收、发布和安装结果待记录。0.10.23 Linux ARM 的 Dashboard 检查停在 Escape 后固定查找底栏快捷键文字，客户端当时仍在运行。0.10.24 需要复验关闭过滤编辑后光标保持隐藏、焦点留在审计面板的行为。早期粘贴对比只对应各自记录的源码版本。[0.10.23 正式运行](https://github.com/LIghtJUNction/ssmcp/actions/runs/44843331041) · [Dashboard 检查](../scripts/e2e_dashboard.py)

发布工作流延后本版的生产 relay 部署。锁定依赖沿用 0.10.23，版本提交只调整 ssmcp 自身的包版本。[发布流程](releases.md) · [锁文件](../Cargo.lock)

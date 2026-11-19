# Shared cohort 应用消费者检查点

本分支以已验收应用候选 `78442aa0849cc1a168db039ff7427ea076594409` 为基线，合入聚合者的 `70d847ee3b529f16292297b20b80b2d06a43a9d5`，保留应用端已有的账号启动、终端和测试修复。聚合记录中的包来源、锁图与最小 CLI 构建证据仍绑定它自己的产品提交 `829abbf333622c6f260d51003f5f3bc78659e6e5`，不能转述为本分支已通过应用验收。应用版本保持 0.10.14；本检查点没有发版或部署。

## Wayland 消费者修复

zuse 0.2.0 在键盘状态不支持时，先拒绝操作，再决定是否创建虚拟键盘或发送快捷键。消费者现在只将已核实的完整前置错误文本识别为可恢复错误。未知错误、连接或输入超时、附带恢复失败的错误仍保持清理未确认状态，不根据一个通用字符串前缀解除输入保护。

文本处理在逐字符调用前检查完整 Wayland 文本，拒绝除 Tab、LF、CR 之外的控制字符，避免先输入有效前缀再报错。逐字符之间的取消检查保留，其他平台原有文本范围及 Windows/macOS 输入路由不变。

上游审查来源为 zuse `557ea46ae70e9de8f93b0928b505132e8c94e9db` 的 `src/platform/wayland.rs`、`wayland_keyboard.rs` 和 `wayland_keymap.rs`。完整比较见 [上游版本差异](https://github.com/LIghtJUNction/zuse/compare/c0dbfda315f8cf1cd03ddb0a3db15f3dcab22d41...557ea46ae70e9de8f93b0928b505132e8c94e9db)。

## 回归范围与当前状态

新增 Linux 回归通过独立测试进程、私有 Unix IPC 模拟 compositor，并调用真实 worker 与 host 输入清理路径。它覆盖 Caps/Num Lock、非零布局组、按住按键导致的拒绝，以及恢复中立状态后同一 Computer 的后续有效调用；混合非法文本必须在尝试原生键盘连接前完整拒绝。测试进程的 Wayland display 指向自有目录中的不存在路径，IPC 不连接真实桌面。另有未知错误保持保守处理，以及 Unicode、其他后端控制字符范围保持的测试。

本检查点仅执行 standalone rustfmt 和 Git 差异检查，**尚未编译或执行这些新增回归**，也没有运行 Cargo、写共享构建 target 或改变既有冻结制品。测试源码中的模拟 IPC 成功不能替代真实 compositor 输入、keymap 恢复、取消、长文本、native/Wasm/harness 与 PTY 应用验收。上述执行验证由应用 owner 在 v0.10.14 发布门禁完成后另行排程。

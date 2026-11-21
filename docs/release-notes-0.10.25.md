# SSMCP 0.10.25

## 启动和聊天

`--input` 选择聊天输入，默认 `terminal`。`--agent` 选择聊天处理方，默认 `next-caller`，由下一次符合条件的工具调用领取。用 `--list-harnesses` 查看支持的原生协议名称，用 `--load-harness` 选择启动时加载的协议。原生尾参放在 `--` 后。只保存了旧协议选择 `--agent` 的会话需用新选项重建。[客户端输入](client-input.md) · [原生协议加载](agent-entry.md)

`--input none` 关闭本连接的聊天框、人类提示和相关工具。执行、已协商的时间观察、独立原生消息和重载仍按原有权限提供。启用聊天的连接可通过 `ssmcp.ui.reply` 回传已认证线程的进度和实际结果。旧结束审批和代理代答入口继续退役。[线程消息](thread-messaging.md) · [问题与回复](finish-and-questions.md)

## 引用、历史和粘贴

普通消息引用缺失的 `@文件` 时会显示警告，消息仍会发送并保留引用原文。`$skill` 按有效作用域解析，项目来源优先。使用技能前应通过返回的读取入口查看完整内容。原生消息和组发保留字面正文。[引用规则](client-input.md) · [原生消息](thread-messaging.md)

已归属会话的回复保留原历史归属，并沿用原有幂等回执。终端批量解码粘贴内容，保留按键顺序、输入限制和草稿。显式选择 Next caller 后使用 `next` 目标投递。[会话历史](conversation-history.md) · [粘贴回归定义](../src/client/terminal/conversation/tests/paste_batch.rs)

## 原生图片与 Unix 输入租约

同一次原生工具调用返回的多张图片分别显示在独立卡片中。预览按各自的事件 ID 关联，保留来源、原会话和调用身份。工具摘要继续独立计数。[图片事件](../src/client/terminal/events.rs#L1550) · [会话与调用回归定义](../src/client/terminal/events/source_tests.rs#L456)

在 Unix 上，主机输入租约的持有对象释放时会显式解锁。派生进程暂时保留旧文件描述符时，其他请求仍可取得已释放的输入锁。回归覆盖关闭旧描述符后新租约继续有效的情况。[输入租约](../src/computer/host_control/storage.rs#L54) · [文件描述符回归定义](../src/computer/host_control/storage.rs#L418)

## 发布状态

本轮准备 0.10.25 的源码版本。最终整合、构建与运行验证、发布和安装结果均待确认。发布工作流延后本版的生产 relay 部署。此版本提交仅更新 ssmcp 自身版本，继续沿用现有锁定依赖。[发布流程](releases.md)

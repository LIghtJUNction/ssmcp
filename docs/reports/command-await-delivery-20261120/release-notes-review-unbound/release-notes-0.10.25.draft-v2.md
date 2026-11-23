# SSMCP 0.10.25

> 发布说明草稿：已进入源码的功能仍待最终候选验收；文中待落地事项不作为已交付功能。

## 启动和聊天

`--input` 选择聊天输入，默认 `terminal`。`--agent` 选择聊天处理方，默认 `next-caller`，由下一次符合条件的工具调用领取。用 `--list-harnesses` 查看支持的原生协议名称，用 `--load-harness` 选择启动时加载的协议。原生尾参放在 `--` 后。只保存了旧协议选择 `--agent` 的会话需用新选项重建。[客户端输入](client-input.md) · [原生协议加载](agent-entry.md)

`--input none` 用于关闭本连接的聊天框、人类提示和相关工具。客户端必须收到中继对关闭人工聊天能力的真实确认（ACK），才可确认这项能力已协商；未获确认时不能自动回退为启用聊天。当前线上尚未取得该路径的通过回执。

执行、已协商的时间观察、独立原生消息和重载仍按原有权限提供。成员之间可继续互发原生消息；启用聊天的连接可通过 `ssmcp.ui.reply` 回传已认证线程的进度和实际结果。旧结束审批和代理代答入口继续退役。[线程消息](thread-messaging.md) · [问题与回复](finish-and-questions.md)

## 查看调用信息和返回 ChatGPT

在当前已保存的对话中，`/openai` 查看最近观察到的调用标识；`/openai-session`、`/openai-subject` 分别查看对应字段。面板保留来源、缺失状态和观察时间；未收到所需元数据时会显示“未观察到”，不会猜测标识。

`/chatgpt set <URL>` 保存用户提供的真实会话网址；`/chatgpt open` 或 `/chatgpt` 请求浏览器打开它，`/chatgpt copy` 复制网址，`/chatgpt clear` 只删除当前对话的网址。`/plugin-docs` 打开官方 Plugins 参考。匿名调用标识不用于拼接网址；浏览器收到打开请求不等于页面已加载或已发送“继续”。浏览器和剪贴板操作只在显式命令触发时执行。[命令说明](commands.md#查看-openai-调用标识和打开-chatgpt)

## 引用、历史和粘贴

普通消息引用缺失的 `@文件` 时会显示警告，消息仍会发送并保留引用原文。`$skill` 按有效作用域解析，项目来源优先。使用技能前应通过返回的读取入口查看完整内容。原生消息和组发保留字面正文。[引用规则](client-input.md) · [原生消息](thread-messaging.md)

已归属会话的回复保留原历史归属，并沿用原有幂等回执。终端批量解码粘贴内容，保留按键顺序、输入限制和草稿。显式选择 Next caller 后使用 `next` 目标投递。[会话历史](conversation-history.md) · [粘贴回归定义](../src/client/terminal/conversation/tests/paste_batch.rs)

## 原生图片与 Unix 输入租约

同一次原生工具调用返回的多张图片分别显示在独立卡片中。预览按各自的事件 ID 关联，保留来源、原会话和调用身份。工具摘要继续独立计数。[图片事件](../src/client/terminal/events.rs) · [会话与调用回归定义](../src/client/terminal/events/source_tests.rs)

在 Unix 上，主机输入租约的持有对象释放时会显式解锁。派生进程暂时保留旧文件描述符时，其他请求仍可取得已释放的输入锁。回归定义覆盖关闭旧描述符后新租约继续有效的情况。[输入租约](../src/computer/host_control/storage.rs) · [文件描述符回归定义](../src/computer/host_control/storage.rs)

## 内部 Git 依赖

`zuse` 已更新至 0.3.0，对应提交 `830fcefd2277cbba46536c67d206f29058ba7fb3`，锁文件与子模块记录同步。`zuse`、`ztuio`、`zharness`、`zplugin` 四项直接依赖保留 canonical Git 来源，移除 registry `version` 字段；这次实际解析的提交由 `Cargo.lock` 记录。

后续四个上游的 Git 政策刷新仍待绑定最终提交，不能视为已经完成。Git 来源声明不会让锁文件自动更新。[依赖规则](dependencies.md)

`ssmcp --version` 保持单行版本输出。`ssmcp --version --all` 从二进制内嵌的构建期 `Cargo.lock` 展示从 `ssmcp` 可达的依赖树，包含 build/dev 依赖和各锁定 target；它不等同于当前平台与 features 的实际链接图，也不能单凭输出证明主仓 commit 或完整锁摘要。该离线版本树逻辑已做源码审阅，尚无执行结果。

## Computer use On/Off：源码已提交，验证待完成

统一开关的源码已提交于 [2619686](https://github.com/LIghtJUNction/ssmcp/commit/26196862691774cab333e1f110b2fdb9c15e5b97)，目标是取消 foreground/background 的控制模式区分。约定范围包括 SSMCP Computer 的 host/isolated、CDP Browser、Omarchy，以及启用时的 Playwright 实际控制和变更。合同要求动作绑定开关代次（generation）：Off 阻止新控制并取消尚未发送的动作，On 不恢复旧请求或旧 ticket。

只读观察和自有资源清理保留，默认仍为 isolated；已经送达的动作不承诺回滚。应用内开关不修改主机权限：同一系统用户的程序既可能直接操作桌面，也可调用本机切换命令；任意 Shell、独立 zuse 和外部 Browser 插件不受它的绝对约束，不能宣称防绕过。源码已提交，但撤权和清理路径仍有待修问题，尚未获得绑定该提交的执行通过结论。

## 发布状态

本轮计划提供六个平台的 GitHub 发布包及安装器。crate 发布延期，待独立完成 registry 版本映射、校验和、包字节及发布预演（dry-run）验证；生产 relay 部署也继续延期，以保留现有连接。这是两项独立决定。

最终整合、构建与运行验收、发布和安装结果仍待确认。此前源码的验收记录及中间工作树检查不替代最终候选的验证。[发布流程](releases.md)

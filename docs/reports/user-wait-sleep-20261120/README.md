# 等待输入时的休眠画面：验收记录

SSMCP 0.10.14 的冻结组合候选 `b49da12ac890f4d2b00c42d9dcf05b97e2054bac` 已通过 6 项休眠状态回归，以及 127 项真实认证等待检查。鼠标点击只解除视觉遮罩：等待请求仍保持 pending，草稿、目标接收方和队列保持原样；真正发送消息后，原有投递协议才交付对应消息及回执。

这份记录只说明休眠界面、相关终端定向回归和实际等待链路。独立 ztuio 的 13 项契约与打包证据见 [ztuio 接入报告](../ztuio-integration-20261120/README.md)。最终全库测试、其他功能验收及正式发布由组合候选的总报告记录。

## 可见行为

终端收到有效的用户等待状态后，整帧文字和底栏一起变暗，中央出现低亮度 ASCII 休眠图案与两行提示。原内容继续可辨认。终端字符不提供 alpha 混合；这里通过背景和文字亮度形成半透明的视觉效果。短或窄的窗口使用更小的标识。

鼠标按下、拖动及抬起作为一次完整手势被吸收，不会穿透到消息、菜单或滚动区域。滚轮和移动不会唤醒。普通键盘输入可唤醒，并沿原输入路径处理首个字符、分片 Unicode 和粘贴。Copy 模式、草稿与浏览状态继续保留；原生 Kitty 图片在休眠时按自有 ID 隐藏，醒后由既有缓存恢复。

同一连接内，视觉唤醒按逻辑 episode 记录，最多保留 32 项。服务端有界等待超时并换用新的 wait_id 重试时，同一 episode 不会再次压暗；新 episode 或新连接可以再次休眠。实现没有改动鉴权、消息预约、提交、最终回执或取消协议。现有 1–25 秒（默认 20 秒）的异步等待、消息通知、授权心跳及 continue_wait 已承担“等待一段时间、检查、再等待”的流程，见 [用户等待协议](../../user-wait.md)。

## 源码与二进制绑定

| 项目 | 标识 |
| --- | --- |
| 休眠源码初稿 | `40edc1f11eb7cd2314f74a2f1559c053baa6e416` |
| 输入时序修复 | `a965fe3828838013d47bc7ffc6254f0cb402c572` |
| 被测组合产品 | `b49da12ac890f4d2b00c42d9dcf05b97e2054bac`，0.10.14 |
| E2E 投影与旧 Local 断言修复 | `6ccf782093d90a58cc01f8e2f22d49654b2f909a` |
| 历史图片测试夹具修复 | `9bfd4de8ee55bc66e2414f20f2e4bb6f4b5fbe9e` |

后三项验证阶段没有替换冻结的产品 ELF。完整程序哈希、测试程序哈希、脚本哈希和图像哈希在 [validation.json](validation.json)、[user-wait-e2e.json](user-wait-e2e.json) 中记录。

冻结 CLI SHA256：

```text
c80da3aa112d25d9228dfe7a860ed02ceb3aa04cd545eb904d1ebad448bf85f7
```

VT 解码工具沿用之前冻结的只读 JSONL 工具；已比较其源文件和 vt100 锁文件条目，在旧候选与 b49 之间完全相同。它只解析捕获的 ANSI，不运行或替代被测产品。证据见 [decoder-source-equivalence.json](decoder-source-equivalence.json)。

## 通过的检查

6 项休眠回归覆盖：

- connection 与 episode 隔离、超时空档和 32 项有界记忆。
- 分片鼠标按下、拖动、抬起，跨连接后的手势吸收，以及后续正常点击。
- 旧遮罩尚未刷新而等待刚结束时，点击仍不能穿透。
- 首个分片 Unicode 字符和多行 bracketed paste 不丢失。
- 120、48、16 和 1 列窗口的真实帧，原 ANSI 重置、亮色和反色不能破坏整屏压暗。
- Copy 模式从冻结到休眠的过渡；按下唤醒后的帧继续接收 mouseup，释放后恢复原鼠标模式和画面。

另有 7 项终端定向检查通过，涉及缩短/清空行的擦除、隐藏 PTY 模式隔离、同步帧与原生图片顺序、分段非阻塞输出、几何分区以及 Unicode 分片。旧 Local 文案断言被单独发现并修正，未混入这些通过项。

真实 E2E 运行私有账户路由、合成身份和原生 CLI PTY，使用 notify:false，不调用模型、不触发真实系统通知，也不访问用户正在使用的会话。127 项检查在 20.972 秒完成，包含：

- 三个已认证等待请求属于同一稳定 episode，宽屏与窄屏都显示遮罩。
- 分片 SGR 鼠标按下、拖动和抬起只唤醒界面；三个 future 仍 pending，wait_id 集合不变。
- 完整草稿、目标接收方、当前视图、焦点、编辑状态及指定接收方队列保持不变。
- 真正的键盘消息提交后，工具响应和最终回执对应同一 message_id 与 revision。
- 等待容量、精确取消、继续等待提示、超时、指定 agent 投递、重连隔离及终端恢复继续通过。

## 两类截图

以下均由真实 ANSI 经过固定 VT 解码器和 Pillow 渲染，不是设计稿。字体使用已安装的 Source Han Sans CN 补足中文；这只影响报告图片的字形，不改产品输出。

**生产 Frame 夹具。** 由 b49 冻结 libtest 的 6 项测试调用生产 compositor，导出休眠和唤醒帧；宽、窄布局及状态断言均通过。主代理已逐张视觉检查压暗、居中提示和恢复后的输入区。最初字体版本缺少一个测试用中文字形，随后保留相同 ANSI 重新渲染，并在 JSON 中保留两组哈希。

| 120 列 | 48 列 |
| --- | --- |
| [休眠](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/frame-sleep-120.png`） / [醒后](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/frame-awake-120.png`） | [休眠](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/frame-sleep-48.png`） / [醒后](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/frame-awake-48.png`） |

**真实认证 PTY。** 下列画面来自同一次通过 127 项检查的真实 CLI 会话。主代理逐张复核：两种宽度的整体压暗和中央提示完整；中文路径草稿与第二行缩进可见；点击醒后恢复正常颜色、同一草稿、1 waiting 队列及等待确认底栏。未见遮罩越界、输入区重叠或中文缺字。

| 真实会话阶段 | 图像 |
| --- | --- |
| 三个等待请求下的 120 列休眠 | [pty-sleep-120.png](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/pty-sleep-120.png`） |
| 同一等待 episode 缩窄到 48 列 | [pty-sleep-48.png](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/pty-sleep-48.png`） |
| 完整鼠标手势唤醒，等待仍 pending | [pty-mouse-awake-120.png](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/user-wait-sleep-20261120/pty-mouse-awake-120.png`） |

## 发现并修正的问题

源码复审识别了两个实际时序边界，均在 a965fe3 修复并有对应回归：输入屏障必须跟随最后组成的画面，不能因为一个较新的空等待快照就让点击穿透旧遮罩；Copy 模式中必须继续报告 mouseup，直到唤醒手势完整结束。

第一次 E2E 读取整个 /display 超过既有 8 KiB 投影上限。夹具已收窄为 /display/sleeping，之后用同一冻结 ELF 完整复跑通过。没有放宽接口限制。

Local 状态旧测试将完整 sidebar 的“一次性登录码”文字误用于 compact 聊天布局。account/status.rs 在旧 70aebd0 与 b49 之间没有变化；新断言检查实际的当前系统用户、MCP 授权入口、本地地址与无云端配额语义。

全库检查还发现三项图片测试沿用“图片默认展开”的旧预期。历史提交 [3e84726](https://github.com/LIghtJUNction/ssmcp/commit/3e847266d40bed8f20b21fe8152fcbd181c1a5f6) 已明确改为聊天缩略图并提供 Click/Enter 展开。夹具现在先断言默认折叠并显式打开，再继续原有图片拒绝、其他 placement 保留、重新上传与 Copy 模式测量过期清理检查。产品图片策略未因此更改；这些 test-only 改动由组合工作树统一重新编译并执行全库门禁。

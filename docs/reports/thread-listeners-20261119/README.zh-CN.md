# SSMCP 程序监听器验收记录

2026-11-19｜Linux｜代码提交 1001a0dba75795ca723a24ab4311546500ae4c43

本阶段已完成可合并的程序监听器实现及本机隔离验收。经过 OAuth 认证的 MCP 网络调用与 Linux 私有 Unix socket 操作同一个租约、消息游标和确认回执。89 项去重后的 Rust 测试通过；真实 CLI/PTY 的强制发送回归通过 118 项检查；两个独立接收程序的网络与 Unix 联合验收通过 73 项检查。代码已推送功能分支，并由 GitHub 连接器按完整提交 SHA 读回。没有安装、部署或重载当前服务，也没有触发或等待 CI。

## 1. 交付内容

用户在终端显式输入 /send <正文>，消息才进入已登记程序的监听通道。普通用户请求队列、user.wait 和协作消息收件箱仍各自保持原有语义。网络接收程序通过现有 MCP code_mode 调用 ssmcp.ui.listener；本地程序通过状态中公开的私有 socket 路径调用同一组 open、read、ack、close、status 操作。实现与调用示例：https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/docs/thread-listeners.md

- 真实账号和线程：网络身份来自 relay 已验证的稳定 provider/subject，以及通过参与者凭据验证的线程绑定。展示名、模型名、run_id 或工具 JSON 不能授予身份。原始绑定在请求进入时固定，异步等待和结果返回之后再次检查。

- 共享状态：同一连接只有一个租约和一个未完成读取。HTTP 长轮询与 Unix 读取返回相同正文、delivery_id 和递增 cursor；任一入口接受的 ACK 在另一入口重试时返回同一回执。

- 明确失败状态：已尝试投递但未确认的消息标为 unknown，不自动重放。accepted 表示接收程序确认收到，不表示模型完成，也不保证外部副作用恰好执行一次。

- 本机来源：Unix peer 由内核 UID/PID 和进程启动时间识别。仅同 UID 程序可以连接；显式持有网络租约 token 的同 UID 程序可以代读和确认。远程账号不能把本地租约 token 当作网络身份。

可信范围与 relay 检查源码：https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/src/runtime/call_scope.rs
https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/src/broker.rs
共享监听状态与 Unix 适配源码：https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/src/client/interface/listener.rs
https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/src/client/interface/listener/transport.rs

## 2. 有界资源与生命周期

租约为 30 秒，read 最多等待 5 秒，ACK 预算为 5 秒。每条连接保留 32 个最终回执与最近 128 个登记幂等键。登记幂等键按实际账号及线程隔离，本地按真实进程范围隔离；已关闭或过期的键不会静默恢复旧租约。

Linux socket 位于本实现拥有的 /tmp/ssmcp-<effective-uid>/listener-<pid>-<random>/receiver.sock。唯一目录权限 0700，socket 权限 0600；路径逐层拒绝符号链接，清理只作用于自有 inode、锁文件和唯一目录。每个连接一帧请求，4 字节大端长度加 UTF-8 JSON；请求最多 4 KiB，响应最多 256 KiB，同时最多 16 个处理任务，整体预算 6 秒。

连接 EOF、额外请求字节或服务端停止会取消实际处理任务并释放读取占用。专项测试在一个真实 read 等待期间中止服务，验证等待连接 EOF、reading 标记释放和 socket 清理。Unix 入口初始化失败时保留已有网络入口，不开放匿名网络端口。共用路径安全实现：https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/src/local_socket.rs

## 3. 实际验证结果

Rust 证据来自两个固定来源阶段。核心提交 8e5ee6417bf6deaede8e8a16659fb5a57656ab4f 已通过 83 项不同测试；最后的连接状态修正与测试 fixture 修正后，重跑 listener 17 项和 identity 5 项。两组测试名称取并集为 89 项，不把重复运行计为新增测试。源码格式、严格 all-target Clippy 和客户端、daemon、公共前端、VT 解码器构建均通过。

- 核心 83 项分组：listener 16；CallScope 5；归属 5；能力探测 1；探测重试 1；对话绑定 1；client steering 29；steering core 7；流量归属 8；来源传播 3；公共前端 7。

- 最后定向 22 项：listener 17 和 identity 5。新增状态快照回归在并发连接代际切换下调用真实 status 入口；已有 Unix 回归补充实际挂起读取期间的服务中止。

- 强制发送：118 项检查通过。覆盖完整 Unicode 正文、带空格文件引用的精确摘要、ACK 重试与冲突、修改后的草稿不被旧 ACK 覆盖、未知结果不重放、断线后新连接身份，以及普通队列保持不变。

- 双程序联合验收：73 项检查通过。两个独立 Python 进程分别使用真实 OAuth/MCP HTTP 和私有 Unix socket，验证真实长轮询唤醒、跨入口相同消息与回执、并发读排斥、幂等登记、OAuth 缺失拒绝、真实 collab 线程绑定、同账号未绑定调用拒绝、UI 接收者严格匹配。

- 退出与清理：两次成功运行的 6 个自有进程均正常 exit 0，验收后再次检查 /proc 均已不存在。两个独立接收程序正常退出，私有 socket、锁和唯一目录消失；真实 PTY 的 termios 恢复到启动前原值，退出备用屏幕。

118 与 73 是各自脚本的检查数量，包含重复的公共 OAuth 初始化检查；合计 191 不代表 191 个互不重叠场景。所有验收使用本机回环地址、合成 GitHub 上游、临时账号与自有 PTY，没有读取真实用户输入或调用模型。脚本：https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/scripts/e2e_force_send.py
https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/scripts/e2e_listener_transports.py

## 4. 发现的问题与修正依据

独立源码审阅发现 status 原本先锁定验证调用范围，再用第二次锁读取快照。两次锁之间若重连，旧范围可能看到新连接的状态元数据。现在在同一个状态锁内验证并序列化。该问题不曾允许读取正文或 token，但仍属于应修正的范围泄露。补丁与并发回归：https://github.com/LIghtJUNction/ssmcp/blob/1001a0dba75795ca723a24ab4311546500ae4c43/src/client/interface/listener/scope_tests.rs

最初真实连接被正确拒绝，因为旧测试 fixture 的固定 HTTPS issuer 与 CLI 选择的实际回环 HTTP 地址不同。修正只存在于 cfg(test)：先绑定真实端口，再统一 fixture 的 issuer、ACL origin 和 OAuth callback。生产 IdentityProviders::new 的 HTTPS 检查未放宽；5 项身份提供方测试通过。

真实强制发送此前完成 115 项检查后停在退出步骤。旧共享退出助手只按一次 Ctrl+C；当前产品的规则是有草稿先清空，空态提示，再按才退出。新夹具逐态验证清空不改普通队列、提示不退出，然后再发送最终 Ctrl+C，保留真实终端恢复与进程退出断言。双程序首轮还暴露测试 worker 对无 JSON 正文的 HTTP 401 先解码；现按真实 HTTP 状态判断拒绝，再解析成功响应。两项都没有改变生产授权、退出或超时语义。

## 5. 源码与可执行文件绑定

最终代码提交：1001a0dba75795ca723a24ab4311546500ae4c43。编译前记录的输入摘要为 f4ae2c95f1180fb9dd85b0aae6308188415d68a8a25738ec22427f8a2603aa31。编译结束后重新计算完全一致；此后仅修改两个 Python 验收脚本，所有 Rust、manifest、lockfile 与子模块输入保持不变。最终脚本使用冻结 ELF，没有依赖随后被其他任务复用的 target 路径。

```
ssmcp SHA-256:
06f0b3074953dedaaf75c0cfc0aaee0a246e582b81555be57384850d17d1429d
library test SHA-256:
1dadbb6e3f79a1531342ea5ce608b782e5528060f3e21ac3235c2cad2a7bb4b0
force-send script SHA-256:
d4a0cde9828ad4b5eb0814394c998c5356bcd3de8f8a7df9a663c88967d16aff
listener-transports script SHA-256:
0f389444c4058a634ff4cfd9bb55b7ec7144612b989266021c365c179a8b83c3
```

同目录 evidence.json 保存完整 ELF 摘要、全部 Rust 测试名称、公开检查文本和各阶段来源。私有 PTY 原始转储、临时账号资料、token、ready 文件和本机完整日志不在公开报告中。

## 6. 复现与集成边界

在已构建候选上设置 SSMCP_E2E_FIXTURE_BINARY 指向同一次构建的 library test ELF，分别运行以下脚本。BIN_DIR 应包含 ssmcp 和 ssmcpd，DECODER 指向同一候选的 vt_snapshot；输出目录应为专门的私有临时目录。脚本本身不运行 Cargo。

```
python3 scripts/e2e_force_send.py --bin-dir "$BIN_DIR" --decoder "$DECODER" --out "$PRIVATE_OUT/force"
python3 scripts/e2e_listener_transports.py --bin-dir "$BIN_DIR" --decoder "$DECODER" --out "$PRIVATE_OUT/transports"
```

集成时应合并完整 feat/thread-listener-transports-20261119 分支，保留前面的公共前端、可信 CallScope 和线程原型来源历史。最后一个修复提交不包含全部实现。公共原型中没有生产调用的旧占位入口已移除，有价值的全局广播、路由、容量与私有请求回归改由真实 Steering→Queue::command 入口验证。

本阶段没有新增独立 webhook 公网入口、线程持久记忆读取器或外部模型。普通 user.wait 由独立工作继续整合。有限验收证明上述场景和资源清理结果，不能据此宣称无限运行永不泄露。代码已可集成，运行中的生产服务未被替换。

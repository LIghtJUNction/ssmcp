# 使用 zbridge 接入图形界面

ssmcp 的 zbridge 适配器让图形前端读取当前运行状态、接收实时事件，并在实际写入租约下提交明确命令。它复用现有 attach 服务的本地认证、输入租约和内存日志，界面可以直接消费结构化数据。

连接有两种方式：在同一进程中使用 `Handle::frontend`，或从另一个进程使用 `RemoteFrontend::connect`。两者处理同一个版本 1 协议，也遵守相同的能力、代次和权限检查。[适配器实现](../src/session/attach/zbridge_adapter.rs)

## 连接与授权

`Frontend` 由真实 `ssmcp::session::attach::Handle` 发放。`handle.frontend(true)` 创建观察者；`handle.frontend(false)` 申请 attach 的独占写入租约。租约封装在对象内部，调用方提交的 JSON 中没有 owner、account 或 agent 身份参数。

`RemoteFrontend` 连接已经存在的 attach endpoint。它通过现有 IPC 读取受保护的本地 descriptor，再使用其凭证访问 loopback 服务。传入的目录必须指向当前用户实际拥有的服务目录；连接过程沿用现有目录、文件权限与 token 校验。[IPC 实现](../src/session/ipc.rs) · [远程对象实现](../src/session/attach/zbridge_adapter/remote.rs)

```rust
use ssmcp::session::attach::RemoteFrontend;
use std::path::PathBuf;

async fn connect_observer(directory: PathBuf) -> ssmcp::Result<RemoteFrontend> {
    RemoteFrontend::connect(directory, true).await
}
```

观察者可以读取能力、状态和事件。写入命令还需要实际写入租约；宿主在分发前、修改前和异步返回后检查当前权限。GUI 输入权限来自本地 attach 授权，worker 的远程成员身份仍由原有 relay 和协作绑定验证。

## 先读取能力，再绑定代次

每个连接从 `Operation::Capabilities {}` 开始。响应列出当前可用资源和当前租约获准的命令，并带有不透明的 `generation`。前端在后续请求中完整带回这个值，让状态读取、事件游标和命令保持在原后端上。

ssmcp 的 bridge 代次同时覆盖 listener 和实际 Runtime 的生命周期。解绑后端、重新绑定后端或重新创建 listener 后，旧代次请求失效。前端应重新读取能力，并把未确定结果的命令留在原请求记录中。

下面的函数读取已有连接的 runtime 状态。`RemoteFrontend::request` 已检查响应与原请求的关联；宿主返回的结构化错误继续传递给调用方。

```rust
use serde_json::json;
use ssmcp::session::attach::RemoteFrontend;
use zbridge::{BridgeError, Id, Operation, Request, ResponseBody, State, VERSION};

async fn read_runtime(frontend: &RemoteFrontend) -> Result<State, BridgeError> {
    let capabilities = frontend.request(Request {
        version: VERSION,
        id: Id::new("capabilities-1")?,
        operation: Operation::Capabilities {},
    }).await?;
    match &capabilities.body {
        ResponseBody::Capabilities(value)
            if value.resources.iter().any(|resource| resource.id.as_str() == "runtime") => {}
        ResponseBody::Error(error) => return Err(error.clone()),
        _ => return Err(BridgeError::invalid("runtime state is unavailable")),
    }
    let generation = capabilities.generation
        .ok_or_else(|| BridgeError::invalid("missing backend generation"))?;
    let response = frontend.request(Request {
        version: VERSION,
        id: Id::new("runtime-1")?,
        operation: Operation::State {
            generation,
            resource: Id::new("runtime")?,
            query: json!({}),
        },
    }).await?;
    match response.body {
        ResponseBody::State(state) => Ok(state),
        ResponseBody::Error(error) => Err(error),
        _ => Err(BridgeError::invalid("unexpected runtime response")),
    }
}
```

同进程前端使用 `handle.frontend(true)?` 获得 `Frontend`，然后调用同名的 `request(Request)`。嵌入式方法直接返回 `Response`；跨进程方法另有传输失败，返回 `Result<Response, BridgeError>`。两种对象都保持自己的租约，且接口不接受外部构造的调用者身份。

## 当前资源

| 资源 | 查询 | 内容 |
| --- | --- | --- |
| `runtime` | `{}` | 后端是否在线、当前原生能力摘要、实时事件游标 |
| `terminals` | `{}` | 当前 Runtime 的真实终端列表；后端在线时提供 |
| `workers` | `{"offset":0,"limit":16}` | 当前 Runtime 自己的 worker 池；配置了 worker 时提供 |

worker 状态来自 Runtime 持有的池，包含实际 `idle`、`working`、`offline`、`retiring`、`registered_workers` 和 `desired_total`。站位保留稳定的 `slot` 与 `epoch`；未登记的外部 worker 容量显示为 offline。

worker 分页按当前站位列表的偏移读取，`limit` 范围为 1–32。每项提供任务摘要、最后命令、进度和真实任务用时，最多保留最近三次任务用时；该投影省略站位日志，并返回 `next_offset`。调用方通过事件接口观察已有进程日志，避免每次刷新复制全部记录。

## 实时事件

`Operation::Events` 使用上一页的 `next_cursor` 继续读取。`cursor: None` 从本次调用的实时边界开始，`wait_ms` 支持 0–1,000 ms。一页最多 128 条和 128 KiB，`has_more` 表示当前保留窗口还有可读取内容。

ssmcp 直接分页读取现有 attach 内存日志，保留原事件来源。日志按 2 MiB 和 4,096 条上限淘汰；旧游标落在窗口之外时返回 `retention_gap: true`。前端遇到缺口后刷新当前状态，再继续显示随后收到的事件。

协议计数器使用十进制字符串，例如 `"9007199254740993"`。前端应通过 `zbridge::Counter` 保存游标，并使用完整 `generation` 区分后端。状态读取与事件读取都保持用户消息、listener 回执和 worker claim 的原生命周期。

## 明确写入命令

| 命令 | 参数 | 作用 |
| --- | --- | --- |
| `terminal.write` | `terminal_id`、`data` | 向指定终端写入显式 UTF-8 数据；数据最多 16 KiB |
| `terminal.resize` | `rows`、`cols`，各 1–1,000 | 调整当前 Runtime 的终端尺寸 |
| `workers.resize` | `pool_generation`、`total`，0–128 | 调整当前活动远程 worker 池的目标容量 |

界面只应在明确的用户操作后构造命令请求，并保留产生该操作时的后端代次。`workers.resize` 还要求 worker 状态返回的 `pool_generation`，防止界面中的旧选择操作到替换后的池。

worker 缩容沿用原生握手：先移除未登记的 offline 站位，再请求实际 worker 释放站位。正在工作的 claim 和结果保持有效，worker 完成并确认释放后才删除对应容量。回执中的 `resize_pending` 表示握手仍在继续；本地 GUI 操作保留原有 manager 绑定。

`ResponseBody::Command` 保留原生回执的执行语义。若请求已经开始执行，随后遇到 I/O 错误、传输中断、授权失效或后端变化，错误的 `effect` 会标为 `may_have_occurred`。界面应先查询当前状态或原任务，再由用户决定后续操作，避免自动重发命令。

## 释放连接与界面接入

`RemoteFrontend::close().await` 等待 detach 回执。对象丢弃时会在可用运行时中尽力释放租约；失去连接的租约沿用 attach 的 15 秒期限。嵌入式 `Frontend` 在释放时归还自己的租约，关闭宿主 Handle 会使其前端失效，后台会话的期望运行状态由原有会话管理器控制。

zgpui 可以把 `RemoteFrontend::request` 作为显式异步 transport callback：输入 `Request`，输出 `Result<Response, BridgeError>`。回包始终关联原请求及其代次；切换窗口不会重新标记旧任务的结果。JSON 消费者可组合共享的编码器和 `Response::validate_for`，使用同一套边界检查。

现有 attach IPC 的外层请求包含 `op: "bridge"`、`lease` 和嵌套的 `request`。lease 由已经认证的 attach open 操作发放，嵌套请求采用 zbridge 协议；普通前端使用公开对象即可完成封装。

适配器为同一宿主限制最多 16 个 bridge 在途请求，并在 256 KiB 的 IPC 帧中预留外层封装空间。当前实现提供结构化接口与消费边界；桌面界面、移动平台和产品交互由各自前端继续实现与验证。

## 验证入口

适配测试包含真实 lease 的观察与释放、等待期间权限撤销、事件淘汰与后端代次变化、真实 Runtime/worker 缩容以及实际私有 IPC 往返。测试源码位于 [zbridge_adapter/tests.rs](../src/session/attach/zbridge_adapter/tests.rs)。

```sh
cargo test --locked --lib session::attach::zbridge_adapter::tests
```

zbridge 核心的字段、计数器、响应关联和事件边界由独立库测试检查。Android / iOS 实机和完整 GUI 验收需要在对应前端项目执行。

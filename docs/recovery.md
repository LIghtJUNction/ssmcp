# 连接恢复：先找回控制通道，再核实任务

本页同时作为 `ssmcp://recovery` 提供给 Agent。它描述当前源码的恢复契约；运行中的客户端是否支持某个工具，必须先查询实时 schema。恢复通道成功不等于任务完成，重新出现的同名客户端也不自动继承原连接的权限或作业。

## 先判断发生了什么

| 已观察到的状态 | 允许的下一步 |
| --- | --- |
| 读取超时、临时网络错误、原连接暂不可见 | 保留原请求和任务标识，执行下面的有界只读重试 |
| 原账号授权明确失效、401/403、撤销或封禁 | 暂停自动恢复；不能用另一账号、匿名入口或 007 绕过 |
| 用户明确退出、停止会话或要求 worker 退役 | 执行该停止意图；不能自动重新启动 |
| 新连接出现，稳定设备和会话身份一致 | 重新读 schema，核对当前工作区、控制／池关联和真实状态，再绑定自己的有效成员身份 |
| 仅名字或工作目录相同 | 身份仍未确定；不能选取第一条结果继续操作 |
| 007 和目标均未发现 | 报告“恢复通道未发现／不可达”，保留检查点；单次发现失败不能证明机器已经关机 |

有界重试只观察现状，不重放原来的启动、提交、写入、部署或付款等操作。客户端报错信息、HTTP 状态、当前 schema 和回执是证据；名字、模型自述和旧文档不是授权证明。

重新绑定已授权的客户端后，用 `await skills.view(clientId,{})`（原生 `ssmcp.skills.view`）查看该客户端的技能概览一次。检查恢复前的 Dreaming 计时和待决申请是否仍可核验。连接或成员身份已变化时，旧批准不再适用。

## 一次恢复窗口

1. **冻结未知操作。** 记录原连接 ID、稳定设备／会话 ID（如果已验证）、目标服务器和账号、工作区、任务与 worker claim、代码提交、输出游标、未完成步骤，以及每个操作当前的 `completed`／`failed`／`unknown` 状态。保留自己的私有成员凭据，但不要把 token 写入公共报告。
2. **先观察原连接。** 按约 1、2、4、8 秒的间隔最多做四次只读观察，允许少量抖动，单次网络请求要有明确期限。每次检查原回执和重新发现；整个窗口到期后报告当前阻塞状态。不要在每个失败外面再套无限重试。
3. **优先查找已授权的 007。** 如果原连接仍不可达，使用同一个已经授权的 MCP 端点查找此前核实属于目标设备的 007。先串行调用 `ssmcp.007.status` 和 `ssmcp.007.list`，核对设备身份、工作区、保存定义、`definition_id` 和 `stop_requested`。007 每次最多处理一个管理请求，不能并行轰击 status/list/recover。
4. **恢复精确原定义。** 实时目录有 `ssmcp.007.recover`，且已保留的设备 ID、定义 ID 均匹配时，提交一次恢复请求。该工具重新检查当前账号凭据和安装密钥，再在会话管理锁内核对定义与停止标记。活动管理器返回 `already_active`；未活动且允许恢复的定义返回 `start_requested`。它保留原程序快照、参数与工作目录，不清除明确停止标记，不执行原任务。
5. **核对结果后继续。** `start_requested` 仅确认启动请求。重新发现 `connected` 会话，验证稳定地址与新的 `client_id`，读取实时 schema。旧连接的成员身份如果仍有效则继续使用；确实失效时才以自己的身份重新加入，保留旧身份和 claim 作为证据。控制窗口和 worker 池分别加入、绑定，不能交换它们的凭据。
6. **结果不明时结束本次恢复尝试，保留任务。** 管理请求超时后先再次观察定义和管理器；不要自动重发 recover/wake。007 缺席、旧版没有安全恢复工具、定义没有事先核实、机器不可达或权限失效时，说明阻塞原因、已观察的状态和下一次可继续的位置，不能报告工作完成。

上述间隔是 Agent 的默认观察策略，不是工具内部自动执行的调度器。正式客户端和 007 进程已有独立的带上限指数退避循环；进程仍在运行时，Agent 应先观察，避免再启动第二个客户端。007 收到明确授权失败后保留进程并暂停网络重试；仅在本机显式写入同账号的新凭据后才继续，时间流逝或换账号不能解除这个暂停。

## 精确恢复请求

以下值必须来自先前已核实的状态，而不是由别名推导。先发现当前 007 的工具定义，再使用它提供的字段：

```javascript
await tools.search(recoveryClientId, {
  name_prefix: "ssmcp.007.",
  include_schemas: true
});

// status 和 list 分开、串行调用；检查其内容后再决定是否恢复。
const status = await call(recoveryClientId, "ssmcp.007.status", {});
const inventory = await call(recoveryClientId, "ssmcp.007.list", {});
return {status, inventory};
```

核对通过后，在新的调用中执行一次：

```javascript
return await call(recoveryClientId, "ssmcp.007.recover", {
  name: savedSessionName,
  expected_device_id: previouslyVerifiedDeviceId,
  expected_definition_id: previouslyVerifiedDefinitionId
});
```

`wake` 是显式创建或启动操作，可能清除先前的停止意图；不能把它当作安全恢复工具的自动替代品。旧版 007 只有 wake 时，应报告能力缺口。手动打开、没有保存定义的终端也不能仅凭相同路径变成一个可恢复后台会话。

## 检查点和幂等性

SSMCP 的连接、成员、任务回执和输出各有保留期限。客户端重启不会恢复此前进程已经丢失的内存作业。Agent 应在自己已有的任务上下文，或用户授权的位置，保留一个无秘密的检查点；不要为了恢复而自行建立全机对话归档。

```json
{
  "schema": "ssmcp-agent-recovery-v1",
  "task_id": "ORIGINAL_TASK_ID",
  "server": "https://relay.example.com",
  "verified_account_subject": "ORIGINAL_SUBJECT",
  "workspace": "/authorized/project",
  "device_id": "PREVIOUSLY_VERIFIED_DEVICE_ID_OR_NULL",
  "session_id": "PREVIOUSLY_VERIFIED_SESSION_ID_OR_NULL",
  "last_client_id": "ORIGINAL_CONNECTION_ID",
  "saved_definition_id": "PREVIOUSLY_VERIFIED_DEFINITION_ID_OR_NULL",
  "worker_claim": null,
  "pending_steps": ["inspect original build result", "continue validation"],
  "operations": [
    {"request_id": "ORIGINAL_REQUEST_ID", "job_id": "ORIGINAL_JOB_ID", "state": "unknown", "replay": "forbidden"}
  ],
  "retry_window": {"attempts": 4, "state": "blocked"}
}
```

示例中的占位符应换成实际值，未知字段使用 JSON `null`。worker claim 应完整保存，不重写 generation、slot、epoch、attempt 或 task ID。请求的幂等键只在原服务承诺的作用域和保留期内有效；换连接、换进程或换池代次后，同一字符串不能自动证明可安全重试。

先查原命令回执、实际文件、Git 提交或相关外部状态。证据足够时才把 `unknown` 改为已确认结果；证据缺失时保留它。只有原任务要求全部得到验证，才能报告完成。

## 实现与验证边界

稳定地址与单次证明见 [device_address.rs](../src/wire/device_address.rs) 和 [routing.rs](../src/device/routing.rs)；客户端退避和授权暂停见 [recovery.rs](../src/client/recovery.rs) 与 [zero.rs](../src/client/zero.rs)；原定义核对及管理锁见 [session.rs](../src/session.rs) 和 [service.rs](../src/session/service.rs)。

验收分别记录网络断线、007 在线、007 不在线、明确停止、错误设备／定义、授权失效和同账号显式重新授权。真实一小时设备测试、外网 NAT、P2P 路径和运行中的生产服务版本分别记录，不能由单元测试推断通过。

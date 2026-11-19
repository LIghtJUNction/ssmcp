# 可定制客户端、上下文工程与工具 hook

ssmcp 的服务端负责认证、会话路由和公开 MCP 入口；开发机上的客户端持有工具实现、工作区边界和原生 harness 连接。你可以在同一服务端后面运行自己的客户端，改变上下文组织和本地调用前后的处理。服务端通过该连接的工具目录发现能力，不需要为每个客户端增加一套策略分支。[Runtime](../src/runtime.rs) · [注册表](../src/registry.rs) · [wire 定义](../src/wire.rs)

本页说明两条扩展路径：复用 Rust Runtime 并安装一个小型工具 hook；或者实现当前 wire 7 客户端，保留自己的 UI、harness 与上下文组织。两者都沿用连接授权；工具实际执行仍由客户端负责。

## 1. 从有来源的文件构建上下文

上下文工程在这里可以拆成三个明确动作：找到适用的来源，按预算读取需要的正文，把内容与来源一起交给你的 harness。ssmcp 提供前两步的确定性文件能力。最终提示词怎样排列、何时压缩以及模型使用哪种上下文窗口，由自定义 harness 决定。[Context 实现](../src/context.rs)

客户端启动目录是 workspace 根。额外上下文目录必须在启动时通过 --context-root 显式授予，最多 8 个，返回 context-1、context-2 等 root ID。额外根只用于技能和指令读取，不会扩大普通文件工具的读写范围。普通文件读取和上下文读取都继续执行各自的 Root 路径检查，拒绝路径穿越和符号链接。[上下文边界说明](context.md#显式加入全局技能和上层规则) · [Workspace](../src/workspace.rs)

~~~js
// clientId 来自当前 list 结果，先核对 hostname 和 cwd。
const roots = await context.roots(clientId);
const instructions = await agents.resolve(clientId, {
  root: "workspace",
  path: "src/lib.rs"
});
const skillsPage = await skills.list(clientId, {scope: "effective"});
return {roots, instructions, skillsPage};
~~~

### 优先级与来源

| 内容 | 当前解析规则 | 应保留的信息 |
| --- | --- | --- |
| 目标路径的指令 | 从所选根到目标目录依次列出。每层取第一个非空的 AGENTS.override.md、AGENTS.md、agents.md；更近的目录排在后面 | root、path、顺序、完整 content、sha256 和诊断 |
| 同名技能 | workspace 优先，再按显式 context-root 的配置顺序；同一根内按 paths 顺序，再按文件路径排序 | 胜出 root/path、shadowed 来源、metadata_status、snapshot |
| 技能正文 | 选定条目后显式 skills.read，按其 root.id 与 path 读取完整原文 | content、bytes、sha256、root/path |
| 普通文件 | 以 workspace 为根，UTF-8 文本可按字节分页，二进制使用 base64 读取 | path、offset、next_offset、eof；完整读取时的 sha256 |

默认技能目录为 .agents/skills、.codex/skills、.claude/skills 和 skills。skills.list 默认列单根；scope: effective 选择项目覆盖全局的有效视图，skills.view 默认使用有效视图。游标绑定扫描配置与快照；来源变化导致游标失效时，从第一页重新读取。无效元数据保留诊断，不把正文猜测成技能描述。[技能索引与指令规则](context.md) · [扫描与解析实现](../src/context.rs)

AGENTS 的顺序是来源解析结果。把更近目录的规则应用于什么范围、如何与用户本轮要求组合，仍由调用方决定。ssmcp 不声称这些文件已经被某个外部 harness 加载，也不会执行其中的命令。

### 已有预算

这些是传输和读取预算，不是模型 token 预算。自定义客户端可在它们之上设置更小的上下文预算，同时保留被省略来源与续读位置。

| 项目 | 当前上限或默认值 |
| --- | --- |
| 额外只读上下文根 | 最多 8 个 |
| 技能扫描 | 默认深度 4、最多深度 8；默认 2048 节点、最多 4096 节点 |
| 技能分页 | 默认 32 条，最多 64 条 |
| 技能元数据读取 | 每个文件 16 KiB；每根扫描总计 1 MiB；单个元数据字段最多 2048 字节 |
| 完整技能/指令文件 | 每个文档最多 256 KiB；agents.resolve 指令链合计最多 256 KiB |
| 上下文响应 | 最多 1 MiB |
| 一次用户要求 | 原文与引用展开合计最多 16 KiB |
| 显式引用 | 每条要求最多 8 个引用；普通文件最多 64 MiB，文本预览约 1 KiB |
| 原生图片读取 | PNG/JPEG/GIF/WebP；压缩文件最多 400 KiB |

超出完整文档或指令链预算会明确报错，不返回被冒充为完整规则的半段内容。扫描响应有 truncated、diagnostics 与 limits；调用方应据此决定是否缩小扫描范围或继续读取。[Context 预算](../src/context.rs) · [引用预算](../src/client/references.rs) · [图片边界](../src/workspace/image.rs)

### 使用输入中的文件与技能引用

用户输入 @path、带引号的 @ 文件路径或者 $skill-name 后，客户端在投递前生成带 source_sha256 的引用描述。文本带有限预览和续读动作；技能带有效来源和完整读取动作；图片带 ssmcp.fs.read_image 动作。接收方用投递客户端的 ID 执行这些动作。[输入说明](client-input.md) · [References 实现](../src/client/references.rs)

~~~js
const result = await call(clientId, "ssmcp.fs.read_image", {
  path: "assets/example.png"
});
image(result);
return result.structuredContent;
~~~

队列重试保留已经准备好的引用载荷。后续显式读取仍读取当时的文件：如果 harness 要把正文与较早的计划绑定，应比较可用的 SHA-256 或重新确认来源，而不是把预览当作不可变上传文件。图片引用准备仅检查文件头；实际终端预览另有有界解码。

## 2. 安装一个工具调用 hook

Rust 客户端可使用公开的 runtime::hooks::ToolCall 和 ToolCallHook，并通过 Runtime::set_tool_call_hook 安装或清空。默认值为 None，直接沿用原来的调用路径；没有安装 hook 时不额外复制参数或结果。[公开接口](../src/runtime/hooks.rs) · [调用接线](../src/runtime.rs)

~~~rust
use serde_json::Value;
use ssmcp::runtime::{
    hooks::{ToolCall, ToolCallHook},
    Runtime,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct LocalExtension {
    recent: Mutex<VecDeque<(u64, String, bool)>>,
}

impl ToolCallHook for LocalExtension {
    fn before_call(&self, call: &ToolCall) -> Result<(), String> {
        if call.native_name == "ssmcp.fs.write_text"
            && call.arguments["path"].as_str() == Some("protected.txt")
        {
            return Err("protected.txt is reserved by this client".into());
        }
        Ok(())
    }

    fn after_call(&self, call: &ToolCall, result: &Result<Value, String>) {
        let success = result.as_ref().is_ok_and(|value| value["isError"] != true);
        let mut recent = self.recent.lock().unwrap();
        recent.push_back((call.id, call.name.clone(), success));
        while recent.len() > 128 {
            recent.pop_front();
        }
    }
}

fn install_extension(runtime: &Runtime) {
    runtime.set_tool_call_hook(Some(Arc::new(LocalExtension::default())));
}

// 需要恢复默认行为时：
// runtime.set_tool_call_hook(None);
~~~

示例只拒绝一个确定的写入目标，并保存有界的调用元信息。它展示的是客户端扩展入口；真正的路径授权继续由 Workspace 执行。项目没有因此增加审核弹窗、审批队列或默认拒绝规则。

### 调用顺序与结果语义

Runtime 先验证 operation 和已注册工具，将旧式 terminal read/write/resize/status 操作归一化为对应工具名。安装了 hook 时，它为该调用保留一个 hook 快照，并提供本地调用 ID、公开工具名、native_name、provider、generation 和参数。[Runtime::perform](../src/runtime.rs)

before_call 返回 Ok 才继续执行；返回 Err 会产生明确的本地拒绝，after_call 也能观察该拒绝。before_call 完成后，Runtime 再次验证同一个 RegisteredTool 的 generation，避免检查过的提供方在执行前被替换。之后的原生参数检查、工作区权限、harness 审批和输出归档都保持原来的边界。

after_call 收到借用的最终 Result<Value, String>。大型工具结果已经通过输出保护，可能是带归档 ID 的 spilled envelope；原生 MCP 内容、isError 和普通错误仍保持各自形式。观察器不复制或改写这个结果。before_call 的 panic 会拒绝执行；after_call 的 panic 只产生本地 warning，不能把已经完成的写入改成失败响应。

回调是同步的、可并发的，并且不持有 Runtime 的 hook 锁。耗时的记录工作适合交给调用方自己的有界通道；这里没有内置网络回调或命令执行器。替换 hook 只影响后续调用，已开始的调用保留自己的快照。取消或丢弃尚未完成的调用不会保证收到 after_call，也不表示已经发生的副作用被撤销。[Hook 契约](../src/runtime/hooks.rs)

### 覆盖范围

hook 覆盖通过 Runtime::perform 分派的注册工具和归一化 terminal 操作。工具目录 tools/list 直接返回目录；连接层的认证、特殊 reload 握手、内部引用准备和补全读取不属于这个入口。外部 harness 自己执行的内部操作也不会自动变成 Runtime 调用。定制客户端应按需要在自己的连接层或 harness 适配层加入对应处理，而不是把一次允许回调视作全局授权。[客户端连接循环](../src/client.rs) · [Runtime](../src/runtime.rs) · [References](../src/client/references.rs)

这个扩展是受信任的客户端代码，没有沙箱效果。不要让未经校验的模型文本直接变成 hook 程序；回调收到的参数和结果也可能包含私有内容，应只保留上下文工程实际需要的信息。

## 3. 保持服务端不变，重写客户端

当前连接协议版本是 wire 7。它使用有界的逐行 JSON 帧，字段和枚举以 src/wire.rs 为准。服务端仍负责账号与会话 ACL；name、hostname、cwd、controller/model 等展示字段不建立身份。客户端应复用已经配置好的 SSH 或 HTTPS/WSS 认证与信任层，再在该连接上处理协议帧。[wire](../src/wire.rs) · [客户端连接](../src/client.rs) · [服务器地址与信任](client-server.md)

下面展示一个自定义 example.echo 工具的最小消息往返。每个 JSON 对象占一行，末尾附换行；方向标题不是协议内容。

客户端发送 hello：

~~~json
{"type":"hello","version":7,"mode":"shell","metadata":{"name":"my-client","cwd":"/work/project","hostname":"dev-host"}}
~~~

服务端返回 ready。login_code 是连接绑定的私密一次性信息，交给现有登录授权流程处理：

~~~json
{"type":"ready","url":"https://relay.example.com/mcp","login_code":"EXAMPLE_ONLY","login_expires_in":300,"client_id":"EXAMPLE_CLIENT_ID"}
~~~

服务端请求工具目录，客户端只声明实际实现的工具：

~~~json
{"type":"call","id":1,"operation":{"op":"list_tools"}}
{"type":"reply","id":1,"result":{"Ok":{"tools":[{"name":"example.echo","description":"Echo text through this client","inputSchema":{"type":"object","required":["text"],"additionalProperties":false,"properties":{"text":{"type":"string","maxLength":4096}}}}],"nextCursor":null}}}
~~~

服务端随后调用该工具，客户端返回原生 MCP 结果：

~~~json
{"type":"call","id":2,"operation":{"op":"call_tool","name":"example.echo","arguments":{"text":"hello"}}}
{"type":"reply","id":2,"result":{"Ok":{"content":[{"type":"text","text":"hello"}],"structuredContent":{"text":"hello"}}}}
~~~

无法执行的操作使用对应请求 ID 返回错误。例如客户端自身拒绝时：

~~~json
{"type":"reply","id":3,"result":{"Err":"client policy rejected this operation"}}
~~~

这里的 Ok/Err 是 wire 中 Result 的序列化形式，和 MCP 结果内部的 isError 是不同层次。复用 Runtime 时，可将收到的 Operation 交给 perform，再把返回值放入同 ID 的 Reply；其他语言客户端也可以按这些公开帧自行实现工具分派。

连接处理需要保持请求 ID 对应、限制在途调用并处理断开。当前每帧最多 4 MiB，最多 16 个在途请求；terminal 读取最多 32 KiB、写入最多 16 KiB、单次读取等待最多 5000 ms。客户端还应消费 wire 7 的 code_mode_finished 等生命周期帧，并只发送自己实现的 steering 或 reload 行为；不支持的能力要明确拒绝。工具目录与调用处理应来自同一套能力定义，防止目录宣称有工具而调用没有实现。[帧与 operation 校验](../src/wire.rs) · [注册表 generation 校验](../src/registry.rs)

## 4. 验证与后续范围

新增 Runtime 回归使用临时工作区、独立测试进程、临时 HOME/XDG 目录和空 PATH。它检查拒绝不写文件、允许后仍受 Root 约束、generation 变化不能执行、观察成功与错误、panic 处理、归档后结果以及清空 hook 后恢复原生行为。空 PATH 防止发现开发机上已经安装的模型 harness。[实际测试](../src/runtime/hooks.rs)

~~~sh
cargo test --lib runtime::hooks::tests::hooks_guard_real_workspace_calls_and_preserve_native_outcomes
~~~

该接口不引入新的 relay 策略引擎，也不修改外部 harness 的原生审批。中文显示及完整的显示语言设置属于[下一版本计划](next-version.md)，本次客户端扩展不重写 i18n。

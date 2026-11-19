# 显式加载原生插件

ssmcp 可以把一个明确选定的原生动态库接入现有工具目录。默认不扫描插件目录、不读取插件文件、不启动插件 worker；内置文件、exec、UI 等工具不依赖插件。加载使用 zplugin 的真实 native loader，调用使用 zharness 的 Extension v1 接口。[实现](../src/runtime/plugins.rs) · [依赖](../Cargo.toml)

## 使用流程

先通过当前客户端的工具目录确认三个管理接口存在：

| 工具 | 行为 |
| --- | --- |
| `ssmcp.plugins.list({})` | 读取已明确加载的插件和资源状态；不发现或启动插件 |
| `ssmcp.plugins.load({path, namespace})` | 加载可信动态库，校验其目录，再原子注册工具 |
| `ssmcp.plugins.unload({namespace})` | 撤销该插件的工具路由，请求 worker 停止 |

`path` 可以是显式绝对路径，也可以是工作区内相对路径。相对路径拒绝父目录跳转，规范化后的目标必须仍在工作区内；目标必须是普通文件。绝对路径表示明确选择那个位置的可信代码，不扩大普通文件工具的 Root 权限。解析后的路径必须能表示为 UTF-8；合法中文 Windows 路径受支持。[路径检查](../src/runtime/plugins.rs)

`namespace` 为 1–32 个 ASCII 字母、数字、下划线或短横线。同一命名空间加载同一个已就绪路径不会重新创建实例；更换库先卸载，等待 `worker_running:false`，再明确加载。

~~~js
// clientId 来自当前 list，先核对开发机及工作区。
await call(clientId, "ssmcp.plugins.load", {
  namespace: "local",
  path: "build/libexample.so"
});
return await tools.search(clientId, {
  query: "plugins.local.",
  include_schemas: true
});
~~~

Linux 通常使用 `.so`，macOS 使用 `.dylib`，Windows 使用 `.dll`。实际工具名和参数以加载成功后返回的目录为准；不要根据库文件名猜测操作。

## 插件协议

插件实现 `zharness::plugin::Extension`，通过 `ExtensionExport` 导出。共享 ABI 的 interface ID 为 `dev.project-z.zharness.extension`，版本为 1；宿主以 `ExtensionApi::create` 绑定。方法为：

~~~rust
fn call(
    &mut self,
    operation: &str,
    input: &[u8],
    output: &mut [u8],
) -> Result<u64, zplugin::Error>;
~~~

加载后，宿主先调用 `ssmcp.catalog.v1`，输入是 UTF-8 JSON `{"namespace":"local"}`。插件返回 UTF-8 JSON：

~~~json
{
  "schema_version": 1,
  "tools": [
    {
      "name": "echo",
      "description": "Echo an explicit input",
      "inputSchema": {
        "type": "object",
        "properties": {"text": {"type": "string"}}
      },
      "outputSchema": {"type": "object"},
      "annotations": {"readOnlyHint": true}
    }
  ]
}
~~~

宿主将这个名称注册为 `plugins.local.echo`。除公开名称加前缀外，原始 descriptor 完整保留，包括输入和输出 schema、annotations、扩展字段。普通调用仍向插件传入原始操作名 `echo`，输入为工具参数的 UTF-8 JSON 对象。重复名称、非法名称、保留操作名、缺少原始 `inputSchema` 或超限目录会使本次加载失败；其他已接受工具目录保持不变。[注册表](../src/registry.rs)

操作必须返回完整 MCP 结果对象，包含 `content` 数组，可带 `structuredContent`、布尔 `isError` 和扩展字段。业务错误 `isError:true` 保留原义；非法 UTF-8、无效 JSON、错误结果结构或越界返回长度作为调用错误返回，不重新执行插件。完整结果继续经过 ssmcp 已有输出保护，大结果可能转为带归档 ID 的 spilled envelope。[Runtime 调用与输出边界](../src/runtime.rs)

可编译的真实动态库示例是 [native_plugin.rs](../tests/fixtures/native_plugin.rs)。它是确定性测试夹具，包含正常返回和故意失败的操作，不是通用生产插件。

## 权限、线程与取消

原生插件是与客户端同权限的可信代码。加载会执行动态库的 native initializer；ABI 版本检查不能把它变成文件、网络或进程沙箱。工作区路径检查只约束库文件的选择，不约束插件内部行为。这里没有自动安装、模型调用或外部 harness 启动。

zplugin 的 library 和代理不能跨线程移动。每个明确加载的插件在自己的线程内创建、调用和销毁代理，异步 Runtime 只持有有界消息通道。调用原生代码时不持有 Runtime、TUI 或 Registry 锁。现有 Runtime hook 可以拒绝工具调用；通过后仍会复验注册 generation，之后才分派。插件不能借此改写已认证 recipient 或另建会话队列。[worker 与管理状态](../src/runtime/plugins.rs) · [调用 hook](../src/runtime/hooks.rs)

等待超时或调用方取消不等于原生代码停止。尚未开始的已取消队列请求会被跳过；执行中的同步 native 调用无法强制取消，也不会自动重试或声称副作用回滚。取消尚未完成的 load 会请求其 worker 停止；没有真正退出的 worker 继续占用名额。

卸载先撤销工具路由，再请求 worker 停止。当前使用 `UnloadPolicy::Pinned`：原生映像仍驻留到客户端进程退出，`unload` 不代表 `dlclose`。关闭 Runtime 最多等待 250 ms 观察 worker 停止，仍在执行时报告清理未完成，不无限阻塞其他工具关闭。

## 资源边界

| 项目 | 上限 |
| --- | ---: |
| 每个客户端进程活跃或仍在停止的 worker（跨 Runtime 共享） | 4 |
| 每个客户端进程的 native library load 尝试 | 16；包括失败，进程退出后重置 |
| 每个插件的工具数量 | 64 |
| 每个 worker 等待执行的请求 | 4 |
| 单次输入 JSON | 64 KiB |
| 目录或结果输出缓冲 | 256 KiB |
| 等待 worker 初始化或调用回复 | 10 秒 |

工作线程真正退出后才释放进程级名额；丢弃 Runtime 或取消等待不会释放仍在执行原生代码的名额。

16 次进程预算用于限制 pinned 映像的反复加载；重复取得已就绪实例不消耗新名额，线程未能创建也不消耗。预算用尽时明确报错，由用户选择重启客户端；不自动重启。列表返回实际已用数量。

这些预算限制宿主管理的缓冲、队列和加载次数，不是原生代码的总内存、CPU 或系统调用限制。插件自身可以分配内存或启动线程；需要不可信代码隔离时应另行选择明确的 Wasm 或进程隔离方案。

## 本地验收入口

普通单元测试检查默认空状态、完整 descriptor 保留、目录拒绝不损坏旧 handle、结果结构和取消 load 的停止请求。真实动态库契约测试显式标记 ignored，避免未构建动态库时被普通测试误判：

~~~sh
cargo build --locked --example native_plugin_fixture
cargo test --locked --lib runtime::plugins::tests::native_plugin_runtime_contract -- --ignored --exact
~~~

两条命令使用同一 target/profile。该测试缺少真实库时明确失败；测试内部不运行 Cargo，不调用模型，不访问系统剪贴板或浏览器。它在临时目录和隔离子进程中验证 Unicode 路径与参数、真实调用、失败目录保留、hook 拒绝、输出归档、业务错误、无效输出、卸载后的 stale handle 和关闭。[测试源码](../src/runtime/plugins/tests.rs)

本文描述实现契约和可复跑验收入口，不替代具体版本的测试报告或发布记录。

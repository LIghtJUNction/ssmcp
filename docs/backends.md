# 统一客户端与 Harness 注册

ssmcp 只有一种客户端连接形态：**Shell**。不再使用 -t 选择 Codex、Claude Code、Pi 或 OpenCode。客户端启动后自动探测 PATH 中可用的 Harness，并把它们的原生工具注册到同一个 code_mode 工具目录。

~~~sh
cd ~/projects/my-app
ssmcp --name workstation --allow octocat@github user@ssmcp.example
~~~

客户端终端始终保持普通 Shell / PTY。Harness 是后台能力提供者，不会改变连接类型，也不会创建第二种 MCP 入口。

## 自动发现与工具命名

启动时并行探测并初始化以下本机程序；某个 Harness 缺失、版本不兼容或启动失败，不影响 Shell 和其他 Harness：

| Harness | 本机接口 | 注册命名空间 |
| --- | --- | --- |
| Codex | 版本匹配的 Codex App Server JSON-RPC / 生成 schema | codex.*、ssmcp.codex.* |
| Claude Code | claude mcp serve 原生 MCP | cc.*、ssmcp.cc.* |
| Pi | Pi JSONL RPC | pi.*、ssmcp.pi.* |
| OpenCode | v2 HTTP API / OpenAPI | opencode.* |

保留各 Harness 原生的参数 schema、结果、图片/结构化内容和审批机制。Claude Code 的 MCP tools/list、Codex 生成的 App Server 方法目录、Pi RPC 命令目录和 OpenCode OpenAPI 操作会合并为一个目录；list({client_id}) 和 tools(client_id, cursor?) 可分页读取。ssmcp.harnesses.list 返回实际注册的 Harness 工具数量。

如果某个 Harness 不可用，启动日志会说明原因；不会用模拟工具填充目录。Harness 内部连接的 MCP server、插件和扩展能力，以该 Harness 对外实际暴露的接口为准。不能从协议获得的工具不会伪造为可调用工具。尤其是 Codex App Server 的生成方法目录不等于它内部每个 MCP server 的独立工具目录。

## 并行与后台命令

code_mode 支持 Promise.all，可并行调用同一客户端的多个 Harness 工具：

~~~js
const client = clients.find(c => c.name === "workstation");
if (!client) throw new Error("workstation is offline");

const [codexState, piState] = await Promise.all([
  call(client.id, "codex.account_read", {}),
  call(client.id, "pi.get_state", {})
]);
return { codexState, piState };
~~~

Shell 命令不通过共享 PTY 排队。ssmcp.exec.start 启动独立 argv 进程并立即返回 job ID；stdout/stderr 分开保存，支持并发、增量读取、等待、取消和超时：

~~~js
const client = clients.find(c => c.name === "workstation");
if (!client) throw new Error("workstation is offline");
const jobs = await Promise.all([
  call(client.id, "ssmcp.exec.start", {
    command: ["cargo", "test", "--locked"],
    cwd: client.cwd,
    timeout_ms: 600000
  }),
  call(client.id, "ssmcp.exec.start", {
    command: ["git", "status", "--short", "--branch"],
    cwd: client.cwd
  })
]);
return jobs;
~~~

后续使用 ssmcp.exec.status、ssmcp.exec.wait、ssmcp.exec.read、ssmcp.exec.cancel 和 ssmcp.exec.list 管理任务。每个任务最多保留 1 MiB stdout 和 1 MiB stderr；每次读取最多 32 KiB。客户端断开时会终止其拥有的后台任务及进程组。交互式终端仍使用 terminal.*，与独立任务互不抢占输入输出。

## 文件与技能审计

ssmcp.fs.read、ssmcp.fs.write 等工作区工具直接在客户端工作区执行。客户端本地终端会显示文件读取/目录访问记录；写入会生成 unified diff。ssmcp.fs.events 可读取审计事件。

ssmcp.skills.list 会发现项目和用户技能目录，ssmcp.skills.read 按名称和来源读取 SKILL.md，并将使用记录写入审计流。Harness 原生工具调用也会记录 Harness 名称、工具名称，以及可识别的文件路径或技能名称。不会记录文件读取内容作为审计元数据。

## 安全边界

- list 返回的客户端 ID 是路由身份；显示名称不是身份。
- 每次原生工具调用仍执行 SSH 客户端授权和 Harness 自己的权限/审批策略。
- ssmcp.exec.start 使用 argv 数组，不隐式经过 shell；需要管道或重定向时显式调用 shell。
- 进程任务有每客户端并发上限、输出上限和最长执行时间。
- 文件工具限制在客户端工作区根目录，拒绝父目录遍历和符号链接逃逸。
- MCP 调用方的模型身份不由 MCP 协议保证提供；未暴露时显示 Unknown，不根据客户端 Harness 或进程推断。

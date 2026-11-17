# 技能摘要、AGENTS.md 与文本文件原语

MCP 资源与 code_mode 共用客户端的技能索引和指令文件解析。每次读取都从当前文件生成结果，文件修改、添加或删除后重新读取即可生效。技能摘要提取 SKILL.md 的 name、description 和来源路径；全文入口保留原始 UTF-8 内容。两条渠道均不调用模型。

先通过 list 选择客户端，并核对 hostname 与 cwd。下面的 clientId 均为已发现的客户端 ID，示例代码放入 code_mode.code。

## 接口速查

| 接口 | 用途 |
| --- | --- |
| context.roots(id) | 列出工作区与启动时显式配置的只读上下文根 |
| skills.list(id, options) | 返回一页技能元数据、来源和诊断；scope: effective 合并项目与全局 |
| skills.view(id, options) | 默认生成项目叠加全局的 Markdown 摘要表；指定 root 可查看单根 |
| skills.read(id, {path}) | 按需读取一个 SKILL.md 的完整原文 |
| agents.list(id, {path}) | 列出从所选根到目标路径的指令文件 |
| agents.resolve(id, {path}) | 返回有序来源及各文件的完整内容 |
| agents.read(id, {path}) | 读取一个指定的指令文件 |
| fs.readText(id, options) | 直接读取 UTF-8 文本及字节分页信息 |
| fs.writeText(id, options) | 原子写入 UTF-8 文本，支持旧内容 SHA-256 前置条件 |
| fs.editText(id, options) | 精确替换一个字面匹配，保留其余内容 |

这些 helper 调用同一组原生能力：ssmcp.context.roots、ssmcp.skills.*、ssmcp.agents.*、ssmcp.fs.read_text/write_text/edit_text。Shell、Codex、Claude Code、Pi 和 OpenCode 客户端均提供这些能力。MCP 顶层工具仍为 list 与 code_mode。

## 生成技能摘要视图

~~~js
return await skills.view(clientId);
~~~

默认在工作区和显式配置的全局上下文根中的 .agents/skills、.codex/skills、.claude/skills 和 skills 目录发现 SKILL.md。skills.view 默认生成有效视图：同名项目技能优先，其后按 --context-root 的配置顺序选择。同一根内按 paths 顺序，再按文件路径顺序选择。每个胜出条目保留 root、path 和 shadowed 来源。元数据无效的条目保留诊断，并单独显示，不覆盖有效技能。

skills.list 默认只列工作区，并保留同名的不同路径。用 scope: effective 获取与默认视图相同的结构化索引：

~~~js
return await skills.list(clientId, {scope: "effective"});
~~~

结果包含来源、诊断和分页信息；view 返回可直接展示的 markdown。正文用 skills.read 按需读取，读取全局条目时传入该条目的 root.id 与 path。

~~~js
return await skills.list(clientId, {
  paths: ["skills", "plugins/team/skills"],
  max_depth: 4,
  limit: 16
});
~~~

返回 next_cursor 时，保留相同的扫描选项继续读取：

~~~js
return await skills.list(clientId, {
  paths: ["skills", "plugins/team/skills"],
  max_depth: 4,
  limit: 16,
  cursor: nextCursor
});
~~~

每条记录保留 root、path、directory、source、name、description 和 metadata_status。单根索引保留同名技能的每个路径，有效索引记录胜出路径和被覆盖来源。游标绑定扫描配置与元数据快照，内容变化导致游标失效时，应从第一页重新读取。

~~~js
return await skills.read(clientId, {
  path: "skills/ssmcp/SKILL.md"
});
~~~

完整读取返回 content、bytes、sha256、root 与 path。单文件超过允许大小时明确报错，避免把不完整的技能正文当作完整规则。

### 元数据解析范围

摘要器支持常用 YAML 标量写法：普通字符串、单引号、JSON 兼容的双引号转义，以及 | / > 多行块和可选的 + / - 结尾处理。它处理 UTF-8、BOM 和 CRLF；单个字段最多 2048 字节。

缺失元数据、重复键、损坏或不支持的 YAML 写法会出现在 diagnostics 和 metadata_status 中。此解析器标记为 yaml-frontmatter-subset，用于生成 name/description 索引；完整 Agent Skills 格式校验应使用相应规范校验器。技能正文不会被猜测成描述，也不会执行其中的脚本。

## 查看适用于目标路径的 AGENTS.md

~~~js
return await agents.resolve(clientId, {
  path: "src/lib.rs"
});
~~~

从所选根目录向目标文件的父目录逐层检查，返回有序的 files。每层采用以下顺序中的第一个非空文件：

1. AGENTS.override.md
2. AGENTS.md
3. agents.md

零字节文件会跳过并记录诊断，再检查同目录下一候选。祖先目录的规则保留在结果中，更靠近目标的规则排在后面；兄弟目录不进入该目标的结果。也支持尚未创建的文件路径，只读取已经存在的祖先目录。

只需要来源列表时使用 agents.list。读取单个文件可使用：

~~~js
return await agents.read(clientId, {path: "AGENTS.md"});
~~~

这是所选根内的当前磁盘视图。返回值保留原文和来源，不会执行文件中的命令，也不重建某个后端会话已经加载的提示词。AGENTS.override.md 和小写 agents.md 的优先顺序是 ssmcp 的明确约定。

## 显式加入全局技能和上层规则

默认上下文根为 workspace，对应客户端启动目录。项目外的目录由启动客户端的人显式添加：

~~~sh
cd /path/to/project
ssmcp --context-root "$HOME" --name development YOUR_SSH_TARGET
~~~

最多支持 8 个附加目录。目录必须存在，路径在连接前解析；相对路径以启动目录为基准。额外目录对应 context-1、context-2 等 ID：

~~~js
return await context.roots(clientId);
~~~

显式选择全局根，生成其中约定目录的技能摘要：

~~~js
return await skills.view(clientId, {root: "context-1"});
~~~

读取该根的规则：

~~~js
return await agents.read(clientId, {
  root: "context-1",
  path: "AGENTS.md"
});
~~~

如果传入的上下文根本身就是技能集合目录，例如 --context-root /opt/team-skills，使用 paths:["."] 从该目录发现技能。若要查看上层规则到项目某个文件的完整链，选择相应根，并传入该根下的项目相对路径。

额外根仅用于读取技能和指令文档。普通 fs 方法仍以工作区为根，不能通过 root 参数或 .. 写入这些目录。上下文读取也拒绝符号链接路径和路径穿越。skills.list 的默认单根索引保持项目范围；skills.view 默认叠加全部已授予的根，显式 root 或 scope: root 可切回单根。scope: effective 使用 workspace 作为项目根，不能与其他 root 值组合。返回结果始终带有 root 和 path 来源。

## 原生文本文件读写

### 直接读取文本

~~~js
return await fs.readText(clientId, {
  path: "README.md",
  offset: 0,
  max_bytes: 65536
});
~~~

返回 text、offset、next_offset、bytes、size、eof、truncated 和 sha256。偏移和上限按 UTF-8 字节计；读取不会截断一个字符。max_bytes 的范围是 4 到 262144，继续读取时使用返回的 next_offset。无效 UTF-8 或落在字符中间的起始偏移会报错。

只有从偏移 0 完整读到文件末尾，sha256 才是整文件摘要；分页结果的 sha256 为 null。二进制文件继续使用 fs.read / fs.write 的 base64 接口。

### 原子写入

~~~js
return await fs.writeText(clientId, {
  path: "notes/example.txt",
  text: "原生文本写入\n",
  create_parents: true
});
~~~

默认拒绝覆盖。修改已有文件时，先读取完整内容，再传入旧摘要：

~~~js
const before = await fs.readText(clientId, {path: "notes/example.txt"});
if (!before.eof || before.sha256 === null) {
  throw new Error("完整读取文件后再做带前置条件的写入");
}
return await fs.writeText(clientId, {
  path: "notes/example.txt",
  text: before.text + "追加一行\n",
  overwrite: true,
  expected_sha256: before.sha256
});
~~~

### 精确编辑

~~~js
const before = await fs.readText(clientId, {path: "config.toml"});
if (!before.eof || before.sha256 === null) throw new Error("需要完整文件摘要");
return await fs.editText(clientId, {
  path: "config.toml",
  old_text: 'level = "info"',
  new_text: 'level = "debug"',
  expected_sha256: before.sha256
});
~~~

编辑要求 old_text 非空，并且恰好出现一次。找不到、存在多个或重叠匹配、旧 SHA-256 不匹配、结果超过 256 KiB 时，都拒绝修改。

写入先创建临时文件，再原子提交。新文件权限为 0600，替换已有文件时保留普通权限位。原生写入通过有界目录锁串行化，最多等待 500 ms 后返回忙碌错误；旧摘要可以检测协作原生写入造成的变化。外部编辑器不受该目录锁约束。

## 动态 MCP 资源

支持标准资源协议的 MCP 宿主可直接调用 resources/list、resources/templates/list 与 resources/read，获取项目上下文，无需执行 code_mode。只有普通工具入口的宿主仍可在 code_mode 中使用同名 resources helper。

| URI | 内容 |
| --- | --- |
| ssmcp://clients/{id}/context | 显式授予的上下文根 |
| ssmcp://clients/{id}/agents{?root,path} | 按目录顺序排列的指令全文；没有文件时 files 为空 |
| ssmcp://clients/{id}/agents/read{?root,path} | 单个 AGENTS.md、AGENTS.override.md 或 agents.md 的原文 |
| ssmcp://clients/{id}/skills{?root,scope,cursor} | 技能摘要 JSON，条目附全文 readUri；scope=effective 合并项目与全局 |
| ssmcp://clients/{id}/skills/view{?root,scope,cursor} | Markdown 技能视图，默认项目叠加已配置的全局根 |
| ssmcp://clients/{id}/skills/read{?root,path} | 单个 SKILL.md 的完整原文 |

花括号表示资源模板变量。客户端 ID 来自 list 或 ssmcp://clients，正文链接直接使用摘要条目的 readUri；path 采用 URL 编码。

下面是直接发给 MCP 服务的标准资源请求：

~~~json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "resources/read",
  "params": {
    "uri": "ssmcp://clients/CLIENT_ID/skills/view"
  }
}
~~~

code_mode 的等价入口如下：

~~~js
return await resources.read(
  "ssmcp://clients/" + clientId + "/skills?scope=effective"
);
~~~

每次读取都会检查当前身份、端点和客户端授权，再从客户端读取当前文件。原始全文资源使用 text/markdown，摘要和指令链使用 application/json。单个文档在 256 KiB 内完整返回；超过上限明确失败。受限上下文结果不经过普通命令输出的 32 KiB 转存阈值。

技能分页使用返回的 nextUri，保留 root、scope 与扫描配置。扫描数据变化后旧游标失效，应从第一页重新读。Markdown 页附元数据与全文入口，存在下一页时附续页链接；扫描达到上限时提示不完整。

“最新”指每次 resources/read 返回当次磁盘读取结果。当前 JSON 传输不提供订阅通知；宿主已缓存或已放入对话的旧内容，需要再次读取才能更新。无需重启服务或重新连接客户端。

全局范围沿用启动时配置的 --context-root。例如以用户主目录作为全局根，可读取其中约定技能目录；自定义技能集合可在 code_mode 中用 paths 选择。资源入口采用默认发现路径。服务自带的 ssmcp://skill 是随服务版本发布的使用指南，项目自己的实时全文通过上述客户端资源获取。

## 读取与扫描上限

| 项目 | 默认值 / 上限 |
| --- | --- |
| 附加只读上下文根 | 最多 8 个 |
| 每次技能扫描路径 | 最多 16 个 |
| 技能扫描深度 | 默认 4，最多 8 |
| 每个根的扫描节点 | 默认 2048，最多 4096 |
| 技能摘要每页 | 默认 32，最多 64 |
| 每个技能的元数据读取 | 最多 16 KiB |
| 每个根的元数据读取总量 | 最多 1 MiB；有效视图最多 9 个已授予根 |
| 单个完整文档、指令链总内容 | 最多 256 KiB |
| 文本写入或编辑结果 | 最多 256 KiB |
| 上下文结构化结果 / MCP 资源正文 | 最多 1 MiB |
| 资源 URI | 最多 8192 字节，包含 URL 编码后的查询参数 |

达到扫描预算时检查 truncated 和 diagnostics；完整指令读取超过上限时明确失败。各客户端的实际 schema 与返回 limits 是调用时的依据。

## 参考与验证

技能索引采用 [Agent Skills 的渐进读取方式](https://agentskills.io/specification)：先看名称与描述，再按需读取完整文档。指令文件组织参考 [AGENTS.md](https://agents.md/) 的目录层级用法；本实现的根范围与文件优先顺序见上文。

本轮测试、原生协议覆盖和实际运行状态见 [验收记录](validation-context-primitives.md)。

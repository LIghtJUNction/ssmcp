# 技能摘要、AGENTS.md 与文本文件原语

code_mode 提供原生的技能索引、指令文件视图和 UTF-8 文件读写。它们由客户端直接读取文件并生成结构化结果；技能摘要提取 SKILL.md 的 name、description 和来源路径，不调用模型。

先通过 list 选择客户端，并核对 hostname 与 cwd。下面的 clientId 均为已发现的客户端 ID，示例代码放入 code_mode.code。

## 接口速查

| 接口 | 用途 |
| --- | --- |
| context.roots(id) | 列出工作区与启动时显式配置的只读上下文根 |
| skills.list(id, options) | 返回一页技能元数据、来源和诊断 |
| skills.view(id, options) | 生成相同元数据页的 Markdown 摘要表 |
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

默认在客户端工作区中的 .agents/skills、.codex/skills、.claude/skills 和 skills 目录发现 SKILL.md。结果包含可直接展示的 markdown、来源、诊断和分页信息。正文留给 skills.read 按需读取。

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

每条记录保留 path、directory、source、name、description 和 metadata_status。同名技能可以来自不同目录；结果保留每个路径，调用方按来源选择。游标绑定扫描配置与元数据快照，内容变化导致游标失效时，应从第一页重新读取。

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

额外根仅用于读取技能和指令文档。普通 fs 方法仍以工作区为根，不能通过 root 参数或 .. 写入这些目录。上下文读取也拒绝符号链接路径和路径穿越。额外根不会自动混入默认摘要，返回结果始终带有 root 和 path 来源。

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

| URI | 内容 |
| --- | --- |
| ssmcp://clients/{id}/context | 显式上下文根 |
| ssmcp://clients/{id}/skills{?root,cursor} | 技能摘要页 |
| ssmcp://clients/{id}/agents{?root,path} | 有序指令内容 |

花括号表示资源模板变量。实际读取时使用真实客户端 ID 和查询参数：

~~~js
return await resources.read(
  "ssmcp://clients/" + clientId + "/agents?path=src%2Flib.rs"
);
~~~

这些资源经过同一身份、端点及客户端授权检查。技能分页使用返回的 nextUri，保持所选 root。资源不自动订阅文件变化，重新读取会生成新的磁盘快照。

## 读取与扫描上限

| 项目 | 默认值 / 上限 |
| --- | --- |
| 附加只读上下文根 | 最多 8 个 |
| 每次技能扫描路径 | 最多 16 个 |
| 技能扫描深度 | 默认 4，最多 8 |
| 扫描节点 | 默认 2048，最多 4096 |
| 技能摘要每页 | 默认 32，最多 64 |
| 每个技能的元数据读取 | 最多 16 KiB |
| 单次扫描的元数据读取总量 | 最多 1 MiB |
| 单个完整文档、指令链总内容 | 最多 256 KiB |
| 文本写入或编辑结果 | 最多 256 KiB |

达到扫描预算时检查 truncated 和 diagnostics；完整指令读取超过上限时明确失败。各客户端的实际 schema 与返回 limits 是调用时的依据。

## 参考与验证

技能索引采用 [Agent Skills 的渐进读取方式](https://agentskills.io/specification)：先看名称与描述，再按需读取完整文档。指令文件组织参考 [AGENTS.md](https://agents.md/) 的目录层级用法；本实现的根范围与文件优先顺序见上文。

本轮测试、原生协议覆盖和实际运行状态见 [验收记录](validation-context-primitives.md)。

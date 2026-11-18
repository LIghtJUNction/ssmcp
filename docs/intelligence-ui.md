# Intelligence UI：在 ssmcp 对话框中直接展示结果

`ssmcp.ui.present` 将结构化数据变成客户端里的卡片、指标、表格、进度、步骤和条形图。Agent 提交数据，客户端根据主题和终端宽度排版。结果出现在同一活动流，用户可以继续输入、浏览或复制。

这层界面服务于两件事：用户更快看懂当前结果，Agent 更少重复组织和解释同一组数据。普通解释继续使用 `say`；有明确字段或数量关系时使用 `ui.present`；需要用户选择时调用 `ssmcp.ui.ask`，由用户通过 `/questions` 回答。

## 发送结构化展示

以下数值仅用于演示 API：

```javascript
const result = await ui.present(clientId, {
  title: "项目检查",
  subtitle: "开发机 · 当前工作区",
  kind: "progress",
  idempotency_key: "project-check-1",
  blocks: [
    {type: "card", title: "编译完成", body: "下一步安装并检查运行进程。", tone: "success"},
    {type: "metrics", items: [
      {label: "通过", value: "24", detail: "示例检查"},
      {label: "待处理", value: "2"}
    ]},
    {type: "table", columns: ["组件", "状态"], rows: [
      ["客户端", "准备安装"],
      ["服务器", "等待部署"]
    ]},
    {type: "progress", label: "发布步骤", value: 60, detail: "这是调用者提供的示例进度"},
    {type: "steps", items: [
      {label: "构建", status: "done"},
      {label: "安装", status: "running"},
      {label: "生产检查", status: "pending"}
    ]},
    {type: "bar_chart", title: "检查数量", unit: "项", items: [
      {label: "输入", value: 12},
      {label: "显示", value: 8},
      {label: "交接", value: 4}
    ]}
  ]
});
return result;
```

没有 `ui.present` helper 的旧服务器也能通过既有 wire7 转发新客户端的原生工具；这个入口不要求更新服务器的便捷 helper：

```javascript
return await call(clientId, "ssmcp.ui.present", {
  title: "检查结果",
  blocks: [{type: "text", text: "客户端支持该工具时即可显示。"}]
});
```

先通过当前客户端目录确认 schema。安装版本和当前运行进程分别决定磁盘与实际会话的能力。

## 七种内容块

| type | 必要字段 | 行为 |
| --- | --- | --- |
| `text` | `text` | 按实际字符宽度折行的正文 |
| `card` | `title, body` | 带语义色的内容卡片；tone 为 info / success / warning / error |
| `metrics` | `items[{label,value}]` | 指标按可用宽度排列；可加 detail |
| `table` | `columns, rows` | 宽窗口显示对齐表格，窄窗口逐字段显示 |
| `progress` | `label, value` | 0–100 的百分比条；可加 detail |
| `steps` | `items[{label,status}]` | pending / running / done / failed 同时显示状态文字与颜色 |
| `bar_chart` | `title, items[{label,value}]` | 非负有限数值按同组最大值绘制；可加 unit |

数值和状态由调用者提供，客户端不会根据动画或调用次数推测完成比例。条形图保留数值，关闭颜色后也能阅读。标题和正文支持 Unicode；终端过窄时优先呈现文字。

## 消息、复制和交互

`kind` 和 `idempotency_key` 沿用 `ssmcp.ui.message`；发送者来自调用上下文，`author`、`role` 等冒名字段会被拒绝。回执保留 `accepted`、`shown`、`message_id`、`duplicate`、`history_saved` 和 `terminal_active`，另加 `format: "intelligence_ui.v1"` 与 `block_count`。接收回执不代表用户已经阅读。

同一保留期内重试相同的键和内容不会重复显示或写入历史。去重身份包含完整结构化文档：即使两种布局生成相同文字，复用同一个键也会报错。消息和展示共用最多 64 条回执缓存；它不是第二份消息历史。

完整文字进入现有 `/copy` 缓存和同一份标准 JSONL 对话历史，沿用现有脱敏与持久化规则；使用 `ssmcp.ui.history` 读取。视觉表格的省略不改写完整正文。`/resume` 恢复历史文字，不重放工具；本版本不恢复历史的结构化布局。展示沿用当前活动流的浏览和折叠行为。

交互问题调用 `ssmcp.ui.ask` 创建，Agent 可继续工作；用户通过 `/questions` 回答，调用 `ssmcp.ui.questions` 读取结果。结构化展示本身不提交草稿、不回答问题，也不切换当前收件目标。

更新后的服务器将 `ui.present` 与 `say` 同样视为实际交互，可在该次 `code_mode` 响应中领取 Next caller 要求。精确 Agent 投递继续要求显式绑定。只有原生工具更新、服务器尚未更新时，显示仍可通过 `call` 工作；便捷 helper 和未绑定的追加要求交付取决于正在运行的服务器版本。调用方始终检查 `structuredContent.user_messages` 与图片内容块。

## 限额与验证

每次展示 1–16 块，完整文字最多 14 KiB，结构化文档最多 24 KiB，含请求元数据的 JSON 最多 25 KiB。指标组最多 8 项；表格最多 8 列、30 行，每行列数一致；步骤和条形图最多 24 项。未知字段与类型、终端控制字符、异常数值和超限内容会在显示前被拒绝。渲染复用现有文本脱敏与终端字符清洗，按 Unicode 显示宽度折行，并保留 NO_COLOR 语义。没有通过校验的展示不会改写当前消息。

实现位于 [presentation.rs](../src/client/presentation.rs)，与 [共享消息路径](../src/client/interface/messages.rs) 共用回执和去重，通过 [现有 renderer](../src/client/render.rs) 写入同一份 JSONL、活动流和复制缓存。真实终端验收使用 [e2e_presentations.py](../scripts/e2e_presentations.py)，检查结构化布局、原生调用、完整复制和 JSONL、去重与追加需求交付。脚本使用临时 HOME/XDG 和私有剪贴板 fixture；测试脚本存在不代表某一构建已经通过，需要保留对应二进制哈希与运行报告。

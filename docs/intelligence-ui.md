# Intelligence UI：在 ssmcp 对话框中直接展示结果

`ssmcp.ui.present` 将结构化数据变成客户端里的卡片、指标、表格、进度、步骤和条形图。Agent 提交数据，客户端根据主题和终端宽度排版。结果出现在同一活动流，用户可以继续输入、浏览或复制。

这层界面服务于两件事：用户更快看懂当前结果，Agent 更少重复组织和解释同一组数据。普通解释继续使用 `say`；有明确字段或数量关系时使用 `ui.present`；需要用户选择时使用 `ui.ask` 和 `/questions`。

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

没有 `ui.present` helper 的旧服务器也能转发新客户端的原生工具：

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

`kind`、`author` 和 `idempotency_key` 与 `ssmcp.ui.message` 相同。返回 `accepted`、`message_id`、`duplicate` 等字段，另有 `format: "intelligence_ui.v1"` 和 `block_count`。同一保留期内重试完全相同的键和内容不会重复显示；重用键修改内容会报错。

完整的文字表达进入 `/copy`、`ui.messages` 和本地持久历史；视觉表格的截断不改写原始正文。`/resume` 恢复历史文字，不重放工具。结构化显示仍可按事件浏览与折叠。

交互问题通过 `ui.ask(clientId,{question,options})` 创建，Agent 可继续工作。用户用 `/questions` 查看和回答，读取答案使用 `ui.questions`。Shift+↑ 始终用于取回最新待交付需求；普通草稿、图片与接收目标分别保留。

发送 `ui.present` 与发送 `say` 一样，可能在本次 `code_mode` 响应中收到用户追加要求；调用方继续检查 `structuredContent.user_messages` 和真实图片内容块。

## 限额与验证

每次展示 1–16 块，完整文字最多 14 KiB，结构化文档最多 24 KiB。指标组最多 8 项；表格最多 8 列、30 行，每行列数一致；步骤和条形图最多 24 项。未知类型、终端控制字符、异常数值和超限内容会在显示前被拒绝。没有通过校验的展示不会改写当前消息。

实现位于 [presentation.rs](../src/client/presentation.rs)，与 [消息模块](../src/client/messages.rs) 共用接收和保留逻辑。真实终端验收使用 [e2e_presentations.py](../scripts/e2e_presentations.py)，检查结构化布局、原生调用、复制与追加需求的交付。

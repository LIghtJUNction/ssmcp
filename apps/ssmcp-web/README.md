# SSMCP Web

SSMCP Web 是现有 CLI 核心的浏览器前端。页面通过同源 HTTP 网关连接真实 attach 会话，读取 zbridge v1 状态与事件，并在用户明确提交后调用原生命令。执行、输入队列、worker 调度和控制租约仍由 CLI 管理。

应用使用原生 ES modules、HTML 和 CSS，没有运行时第三方依赖、打包步骤、外部字体或演示数据。正文、日志和元数据都作为纯文本渲染。界面默认跟随系统明暗主题；主题切换只影响当前页面。

## 启动

需要 Node.js 20 或更新版本，以及已经运行的 SSMCP CLI HTTP 网关。网关启动方式和配对令牌以仓库的 [GUI bridge 文档](../../docs/gui-gateway.md) 及当前 CLI 帮助为准。

在仓库根目录运行下列命令。把 `网关端口` 换成 CLI 实际监听的端口：

```sh
node apps/ssmcp-web/dev-server.mjs --gateway http://127.0.0.1:网关端口
```

浏览器打开 `http://127.0.0.1:4173`，点击“连接本机网关”，输入 CLI 提供的 pairing token，然后从真实会话目录选择会话。默认连接模式为“只读观察”；需要发送输入或调整 worker 时，在连接前选择“控制会话”。控制连接是否获准由原生输入租约决定。

开发服务器仅监听 `127.0.0.1`。它把 `/api/gui/v1/` 请求转发到显式配置的本机网关，同源提供页面和 API。上游地址必须为 loopback HTTP(S) origin，不接受 URL 中的凭据、额外路径或外部主机。服务器不输出请求头、配对令牌或输入内容。

```sh
# 指定 Web 端口
node apps/ssmcp-web/dev-server.mjs --port 4174 --gateway http://127.0.0.1:网关端口

# 仅查看断开连接的静态界面；这个模式不会提供模拟 API
node apps/ssmcp-web/dev-server.mjs
```

也可由实际网关或同源 Web 服务器直接提供此目录中的 `index.html`、`styles.css`、`app.mjs` 和 `src/*.mjs`。API 前缀固定为 `/api/gui/v1`。页面不需要内联脚本或样式，可使用 `script-src 'self'; style-src 'self'; connect-src 'self'`；不能把令牌放进 URL 或静态配置文件。

## 已接线的能力

所有控件都依据当前 zbridge `capabilities` 和实际运行时状态显示。没有广告的命令不会被调用；缺失资源会显示原因。会话目录、线程、队列和 worker 均支持实际接口的有界分页。

| 页面 | 实际接口 | 行为 |
| --- | --- | --- |
| 会话导航 | `GET /sessions`；`POST /connections` | 读取名称、目录与连接状态，显式选择只读或控制连接。 |
| 对话与事件 | `state: conversation`；`POST /connections/{id}/events` | 展示真实历史及后续事件；来源使用经过认证的外层 envelope 或服务端历史 projection。 |
| 线程选择 | `state: threads` | 分页查看真实 agent、角色和状态；接收方离开当前页后要求重新核对，不静默换目标。 |
| 人类输入 | `command: chat.send`；`state: chat.queue` | 使用现有 CLI 输入队列。只有收到原始 `request_id`、目标和 `local_queue_admission` 回执后才确认入队。 |
| 终端 | `state: terminals`；`terminal.write`；`terminal.resize` | 选择真实终端，显式发送文本。换行选项清楚可见；浏览器窗口变化不会自动 resize 终端。 |
| Workers | `state: workers`；`workers.resize` | 读取注册、站点、任务摘要与原生进展，按当前 `pool_generation` 提交目标站数。 |
| 主对话中的 worker 分派 | 真实 `workers.slots` 任务快照及原生事件 | 按实际 `task_id` 去重。快照标记“最后观察”；任务从列表消失不会被推断为完成。 |
| 关闭前端 | `DELETE /connections/{id}` | 释放当前前端连接。接口回执必须明确 `session_stopped: false`。 |

终端输出是有界纯文本预览，不是完整终端屏幕模拟器。页面没有发明文件管理、任意工具执行、worker submit、任务取消或其它未广告的控制接口。CLI 后端新增能力后，应先扩展明确的命令接线和对应验证。

## 协议与结果语义

`src/protocol.mjs` 根据仓库的 [GUI bridge 协议说明](../../docs/gui-gateway.md) 和 [原生适配器](../../src/session/attach/zbridge_adapter.rs) 接线，验证版本、响应 ID、操作种类、资源或命令名称，以及完整的 opaque generation。成功的连接响应还验证嵌套 capabilities 的 ID 等于原连接请求 ID；连接后再执行一次独立、有关联 ID 的能力查询。

游标和事件序号始终是规范十进制字符串。比较通过 `BigInt` 完成，不经过 JavaScript `Number`。事件页检查顺序、范围、`has_more` 和缺口语义；新 generation 重新读取能力和状态，不沿用旧游标或旧命令权限。

原始 zbridge v1 `events.wait_ms` 的上限仍为 1 秒。Web 使用网关的聚合观察端点，默认每次观察 50 秒，HTTP 总等待上限 55 秒。暂停按钮只中止当前观察和退避等待，不传递命令取消信号。恢复观察继续使用上一次完整消费后的游标。

当事件页报告 `retention_gap` 时，客户端先刷新权威状态，再消费页面和推进游标。如果刷新失败，原游标和已收到的待消费页面一起保留，恢复后先处理这张页面，不重新请求来替换它。缺口只说明中间事件已不在保留窗口，不能用来证明某个任务成功。多次资源读取共享一个最多 55 秒的读取窗口；命令不使用这个取消信号。

命令只由点击或明确的 `Ctrl / ⌘ + Enter` 触发。网络失败、响应关联错误和原生 `unknown` 均保留原请求 ID，记录为未确定，不自动重发。有效的 `effect: none` 错误记录为拒绝。`confirmed` 的含义仅为“已取得并验证原生回执”，异步任务是否完成继续依赖实际状态；`chat.send` 的入队回执不等于模型已读。

配对令牌只在 `Gateway` 的私有内存字段中保存，用于 `Authorization: Bearer …`。它不会进入 URL、日志、localStorage、sessionStorage、事件元数据或命令记录。输入框在认证请求发出时清空。刷新页面会丢失当前页面的认证、草稿和操作记录；在刷新前可通过“操作记录”查看需要保留的原请求 ID。

## 界面与内存边界

对话输入框和终端输入框保持同一个 DOM 节点。事件刷新不会替换正在输入的元素；输入法组合期间禁止发送快捷键。发送时仍可继续编辑草稿，回执只清空正文、接收方、连接和 generation 都与提交时一致的草稿。

时间线最多保留 300 条或 768 KiB，每个本地分页最多渲染 40 条。终端预览最多 65,536 字符，命令账本最多 64 条，连接记录最多 32 条。未确定和发送中的操作不会为腾出空间而被静默淘汰；达到容量时拒绝新的记录，提示先核对已有请求。较早事件被本地容量释放时会显示提示。

追加事件只在用户原本接近底部时自动滚动。查看较早记录时保留当前锚点，并提供“回到最新”。窄屏使用会话抽屉和三面板切换，工作区无需横向滚动。断线读取采用 1、2、4、8、16、30 秒退避，上限 30 秒；立即返回的空事件页也会让出等待时间，避免热轮询。

## 测试

基础测试只使用 Node 内置 test runner，不需要下载依赖：

```sh
cd apps/ssmcp-web
node --test test/*.test.mjs
```

测试覆盖响应关联、u64 大游标、缺口同步失败、命令与观察的独立生命周期、暂停续读、未确定结果不重发、较新资源页覆盖延迟旧读取、原生回执关联、私有令牌、来源伪造、有界记录和 worker 快照去重。测试 fixture 来自真实协议结构，只在 `test/` 中使用。

另外提供可选浏览器 smoke，用现有 Playwright 与 Chromium 检查实际页面的外部 CSS/JS CSP、输入法组合期间的刷新与焦点、未确定命令草稿，以及 320 / 390 像素布局。它使用明确隔离的协议 fixture，不能代替真实网关验收：

```sh
# 不自动安装或下载浏览器；用环境中已存在的模块和可执行文件
PLAYWRIGHT_MODULE=/已有路径/playwright-core \
CHROMIUM_PATH=/已有路径/chromium \
node test/browser-smoke.mjs
```

在当前受限沙箱中，基础测试已通过；Chromium 在启动时被系统 socket 权限拒绝，尚未执行浏览器检查。真实 CLI 网关、原生控制租约及浏览器外观仍需在完整开发环境中验收。不要把协议 fixture 的结果当作真实任务已执行的证据。

## 文件职责

- `src/gateway.mjs`：同源 HTTP、认证内存、响应大小和请求超时。
- `src/protocol.mjs`：zbridge v1 与聚合观察的严格校验。
- `src/client.mjs`：真实连接、能力、资源、游标、观察退避与命令记录。
- `src/store.mjs`、`src/presentation.mjs`：有界记录、真实事件投影、来源和元数据。
- `app.mjs`、`index.html`、`styles.css`：稳定输入、可访问的控件及响应式显示。
- `dev-server.mjs`：可选的本机同源静态服务与固定上游转发。

更改 HTTP 合同时先修改 transport。更改原生资源或事件结构时以 [SSMCP adapter](../../src/session/attach/zbridge_adapter.rs)、[对话接线](../../src/session/attach/zbridge_adapter/chat.rs) 和 [历史投影](../../src/session/attach/thread_history.rs) 为依据，并补充对应边界测试。

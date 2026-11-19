# 原生历史会话元数据与关键词搜索

日期：2026-11-19（Asia/Taipei）  
实现与审阅：[PR #45](https://github.com/LIghtJUNction/ssmcp/pull/45)

## 结果

code_mode 现在可以直接列出历史会话、读取会话元数据、按关键词搜索，并根据命中位置回到完整原文。搜索在现有 JSONL 历史上运行，沿用工作区隔离、文件校验和脱敏边界。

| helper | 用途 |
|---|---|
| `history.list(clientId, options)` | 列出会话摘要与数量 |
| `history.metadata(clientId, {id})` | 返回标题、时间、消息数、截断计数及当前会话标记，不附带正文 |
| `history.read(clientId, {id, offset, limit})` | 分页读取原文 |
| `history.search(clientId, {query, ...options})` | 返回命中片段、会话 ID、消息 ID 与消息位置 |

例如：

```javascript
const found = await history.search(clientId, {
  query: "登录",
  role: "user",
  limit: 16
});
if (found.matches.length) {
  const hit = found.matches[0];
  const original = await history.read(clientId, {
    id: hit.conversation_id,
    offset: hit.message_index,
    limit: 1
  });
}
```

原生接口仍为 `ssmcp.ui.history`，新增 `metadata` 和 `search` 操作。现有 `ui.history` 与通用 `call` 保持兼容。

## 行为与边界

查询使用字面文本，支持中文，默认以 Unicode 小写形式进行大小写不敏感匹配。可以限定会话 ID、消息角色和大小写。每条消息最多返回一个命中片段；片段最多 384 个字符，并明确报告片段或原始消息是否已经截断。

扫描先检查文件元数据，再读取正文；最多读取 64 个有效候选文件、按已检查大小累计 16 MiB。超过预算会返回 `scan_truncated`。每页最多 64 条命中且不超过 64 KiB；继续读取使用 `next_offset`。手工导入更多文件也不会触发无限扫描。

历史文件读取运行在阻塞工作线程上，界面的更新锁不参与磁盘扫描。查询不会切换活动会话、改写历史或重放旧需求。

组合测试还暴露了已有文件锁的并发问题：fork 出来的子进程可以暂时继承同一打开文件描述，单纯关闭父进程的句柄不足以立即释放锁。本提交复用 [PR #40](https://github.com/LIghtJUNction/ssmcp/pull/40) 提出的显式事务解锁方式，并保留对应回归测试。

## 验证

`cargo test --lib history`：**56 passed，0 failed**。测试涵盖原有历史读写与保留边界、新增元数据、中文与字面搜索、Unicode 片段、分页预算、手工导入扫描上限、只读行为、跨工作区拒绝、code_mode helper 路由和继承句柄解锁。

`rustfmt` 与 `git diff --check` 通过。测试使用临时会话和合成数据，没有读取用户真实聊天正文，也没有替换运行中的客户端或服务端。

[测试结果](tests.json) · [测试运行器记录](tests.log)  
运行器记录省略了测试夹具产生的结构化代理日志，保留测试名称、结果和最终计数。

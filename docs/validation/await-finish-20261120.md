# 通用等待与人工结束批准：定向验证

2026-11-20 在源码 [`a6b1e29ddc2bac1e730efbfd063e46843731df92`](https://github.com/LIghtJUNction/ssmcp/commit/a6b1e29ddc2bac1e730efbfd063e46843731df92) 上运行了 106 项定向回归，全部通过，0 项忽略。本文及指向本文的文档修改在该源码之后提交，没有修改受测实现。

## 方法与结果

环境使用 Rust/Cargo 1.100.0、锁定的 Cargo.lock 和精确子模块，依赖已缓存；构建设置为单任务、独立 target，并通过共享 rustc 包装器避免与并行任务争用编译资源。首先执行 `cargo fmt -- --check` 和 `git diff --check`，两项均通过。随后执行：

```sh
cargo test --offline --locked --lib --message-format=json http::await_tool:: -- --nocapture --test-threads=1
```

该命令构建了当前 libtest，等待测试实际运行 3.77 秒。其余分组直接复用 Cargo JSON 回执里的同一个 libtest 可执行文件，分别传入下表筛选器及 `--nocapture --test-threads=1`；没有为每组重新构建，也没有将仅编译成功计作测试通过。

| 范围 | 精确筛选器 | 通过数 |
|---|---|---:|
| 通用等待 | `http::await_tool::` | 26 |
| 问题、人工批准及原生等待状态机 | `client::interface::questions::` | 35 |
| 公共结束接口回执边界 | `http::finish_request::` | 3 |
| 协作身份、收件与状态观察 | `collab::` | 23 |
| 外层用户输入交付 | `http::user_wait::tests::` | 5 |
| 原生最终消息 | `client::interface::messages::tests::` | 6 |
| JavaScript helper | `code_mode::tests::typed_wait_helper_preserves_targets_and_uses_server_wait_identifiers` | 1 |
| 公共工具目录 | `http::tests::public_catalog_and_arguments_include_direct_collaboration` | 1 |
| 资源契约 | `resources::tests::` | 4 |
| 执行等待与结果保留 | `backend::shell::tests::waits_keep_work_running_and_completed_jobs_remain_available` | 1 |
| 执行超时、取消及客户端隔离 | `backend::shell::tests::jobs_support_timeout_cancellation_and_client_isolation` | 1 |
| 合计 | 各组不重叠 | 106 |

## 这些测试证明的边界

26 项等待回归由 14 项解析、记录与聚合单测和 12 项集成测试组成。集成测试使用实际的内存 Broker、OAuth 身份、协作成员和输入队列，覆盖同账户不同成员的并行等待、跨身份恢复或取消拒绝、真实成员租约到期、未确认收件来源、超时恢复以及最终响应交付。[单测](../../src/http/await_tool/tests.rs) · [集成测试](../../src/http/await_tool/integration_tests.rs)

用户输入的交付竞争按实际实现路径验证：取消先于最终提交时不消费输入；最终交付降为超时或忙时保留同一 wait ID；待交付记录不会被容量或保留期限回收；外层响应被丢弃会释放保留；独占观察重新获得后必须再查终态。取消只停止观察，其他 agent 的对象及未确认消息仍保留。

人工批准回归要求 `Approved + Human + Yes` 同时成立，拒绝所有 agent Yes，并验证普通代理答复和代理 No 的真实来源仍保留。旧的、不一致的批准记录不能暴露或消费许可；同一请求只允许原范围的一次完成。公共接口另外验证恢复 ID、请求者线程和原生回执中的批准字段一致。[人工许可单测](../../src/client/interface/questions/human_finish_tests.rs) · [状态机与被动观察](../../src/client/interface/questions/workflow.rs) · [公共边界](../../src/http/finish_request.rs)

## 验证没有覆盖的事项

本轮没有启动外部模型、登录其他人的会话、重启生产 relay、部署客户端或执行正式发布。内存身份与本地执行夹具验证协议和生命周期，但不能替代整合版本的终端键鼠、宽窄布局和正式发布验收。旧 relay 不会自动获得公共 `await` 或 `waitFor`；`finish` 观察还需要支持 `op: "wait"` 的客户端。SSMCP 只能检查经过其接口的完成动作，无法阻止完全停止调用工具的宿主自行输出外部 final。

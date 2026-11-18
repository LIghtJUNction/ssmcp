# ssmcp：attach 输入与工具发现验收

日期：2026-11-18（Asia/Taipei）  
已验证源码：`f86bc76b8675387d7c767ea596924a4091e5803c`  
变更：[PR #28](https://github.com/LIghtJUNction/ssmcp/pull/28)

## 完成的行为

### attach 中的独立 Esc 正常生效

单独按 Esc 可以关闭 attach 的帮助界面，无需再按一个键。底层键盘读取保留绝对截止时间，持续输出或窗口变化造成的读取取消不会重新计时；真正收到后续字节时会重新建立等待窗口，以保留分段转义序列和 bracketed paste 的处理。attach 的空闲刷新也会释放第二层按键解析器中已超时的前缀。

实现：[attach 输入](../../src/session/attach/input.rs)与[会话循环](../../src/session/attach.rs)。两项新增 Rust 回归分别覆盖反复取消输入读取，以及分段前缀推进、同批输入完成旧前缀后开始新前缀。

### 工具发现支持注册名称前缀

`tools.search` 新增可选 `name_prefix`，先按真实注册名称筛选，再应用原有 `query`。例如：

```javascript
return await tools.search(clientId, {
  name_prefix: "ssmcp.exec.",
  include_schemas: true,
  limit: 20
});
```

前缀不区分大小写。省略或传空字符串时，结果形状和 v1 游标保持兼容；非空前缀使用绑定查询条件的 v2 游标。更换前缀后续用旧游标会在请求客户端之前被拒绝。完整原生 descriptor、逐页授权检查、目录变更检测和输出预算继续生效。

`exec.run` 是组合 `ssmcp.exec.start`、`wait`、`read` 的 JavaScript 便捷函数，目录中没有 `ssmcp.exec.run`。前缀过滤减少返回给模型的无关内容；首次底层目录扫描次数保持原有方式。参数由服务器端 JavaScript worker 解释，需要运行包含本次修改的服务器版本。

实现：[code_mode.js](../../src/code_mode.js)；用法：[工具发现](../backends.md)。

### attach 验收适配现有界面语义

真实 PTY 验收使用独立的聊天界面与原始输入模式。聊天界面验证帮助打开、独立 Esc 关闭以及分离后会话仍运行；原始模式验证 shell 输入、只读观察、写入独占、Ctrl-C、窗口尺寸、分离与重新接入。默认产品布局仍遵循当前界面的输入规则。

## 验证结果

所有下列命令退出码均为 0。 原始断言记录见 [session-service.json](attach-input-discovery-20261118/session-service.json)。

| 验证 | 结果 | 主要覆盖 |
| --- | --- | --- |
| JavaScript discovery/execution/capabilities | 31 项通过，0 失败 | 前缀、分页、旧游标兼容、原生描述、授权与执行 helper |
| Rust `session::attach` | 10 项通过，0 失败 | 读取取消、字节进展、UTF-8、粘贴、输入所有权与历史读取 |
| 隔离 SSH/OAuth/MCP/session 验收 | 61 条断言通过 | 真实认证后的前缀分页、实际 PTY、后台会话生命周期与清理 |
| `cargo fmt --all -- --check` | 通过 | Rust 格式 |
| `cargo clippy --locked --all-targets -- -D warnings` | 通过 | 全目标静态检查 |
| `cargo build --locked --bins --example vt_snapshot` | 通过 | 客户端、服务器与终端观测程序 |

端到端测试使用 Linux 开发机上的临时 session home、loopback SSH/OAuth fixture 和测试进程，结束后移除测试会话并停止测试管理器。该结果覆盖本次代码的本地运行；本轮未安装客户端或部署生产服务器。macOS、Windows 的原生运行验收未在本轮执行。

## 复查命令

在已验证源码上执行：

```sh
node --test scripts/test_tools_search.mjs scripts/test_execution_helpers.mjs scripts/test_client_capabilities.mjs
cargo fmt --all -- --check
cargo test --locked --lib session::attach
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --bins --example vt_snapshot
python3 scripts/e2e_session_service.py --bin-dir target/debug --report target/attach-input-discovery.json --revision f86bc76b8675387d7c767ea596924a4091e5803c
```

端到端脚本要求非 root Linux 用户，依赖与 [e2e.py](../../scripts/e2e.py) 相同。报告中的 61 条断言包含生命周期不同阶段的重复状态检查。

## 结论与证据核对

| 核对项 | 结论与证据 |
| --- | --- |
| 改动价值 | 独立 Esc 可完成界面动作，原生命名空间可直接筛选；都有回归与真实链路检查。 |
| 表述清晰 | 文档区分注册工具名与 JavaScript 便捷函数，示例使用可调用的真实目录。 |
| 实验强度 | 回归测试覆盖异步读取被反复取消；真实 PTY 覆盖无后续按键的 Esc。 |
| 覆盖范围 | 本轮为 Linux 定向验证；没有将本地验收写成生产部署或跨平台运行结果。 |
| 设计一致性 | 收到字节与读取取消采用不同计时规则；搜索保留原有授权、描述及分页约束。 |

## 提交

- [1f34ec1](https://github.com/LIghtJUNction/ssmcp/commit/1f34ec141ac71184de5c28a602652047c189e668)：工具发现名称前缀过滤与兼容性验证。
- [f86bc76](https://github.com/LIghtJUNction/ssmcp/commit/f86bc76b8675387d7c767ea596924a4091e5803c)：attach 输入修复与真实会话验收。

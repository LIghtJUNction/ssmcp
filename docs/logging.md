# 终端日志与服务端诊断

客户端默认使用 `pretty`：请求蓝色，成功绿色，响应元信息青色，工具及 stdout 标题紫色，警告与下载进度黄色，错误和超时红色。stderr 在成功调用中为黄色，失败调用中为红色。普通长命令按当前终端宽度换行；diff 保留行号与原有布局。

## 三种输出模式

```sh
ssmcp --log-format pretty my-server
ssmcp --log-format pretty-verbose my-server
ssmcp --log-format jsonl my-server 2>events.jsonl
ssmcp --no-color my-server
```

`--verbose` 选择 `pretty-verbose`，展开完整输出并附上原始事件 JSON。`--raw-events` 和原有的 `--raw` 选择 `jsonl`。JSONL 中每个事件独占一行；连接状态、SSH stderr、原生后端 stderr 和 shell PTY 字节输出也通过事件输出。PTY 事件包含原始字节的 base64；原生 JSON 事件保留输入 EventLog 时的原始序列化。启动参数解析或配置加载失败发生在日志配置生效之前，错误使用默认可读格式。

stdout 用于连接信息。重定向 stdout 或选择 JSONL 时，保留单行连接 JSON，兼容现有脚本；交互终端默认显示 MCP 地址和登录说明。一次性登录码仍是敏感凭据，保存 stdout 时请限制文件权限。OpenSSH 自身通过 `/dev/tty` 发出的主机密钥或私钥口令交互不属于事件日志。

默认每个缓冲 stdout/stderr 块显示最多 12 行，保留开头与末尾，省略处注明折叠数量和展开参数。实时流式输出持续显示；大对象和数组显示有界摘要。`--max-inline-output N` 调整缓冲输出的行数。这里的折叠是日志展示策略，没有引入交互式终端面板；工具结果和事件缓存仍按原有协议保留。

## 命令、文件与技能活动

后台命令开始时显示 `RUN`、任务 ID、命令参数和工作目录；结束时显示退出码、耗时及 `OK` / `ERR` / `WARN` 状态。每个 stdout/stderr 标题包含对应任务 ID，并行任务的片段可以追溯到来源。命令超过内联阈值时显示 `SAVE` 与转储位置，正文按需使用 `outputs.read` 读取。

原生文件读取显示 `READ` 路径，写入和编辑显示 `EDIT` 及有界 diff；技能访问显示 `SKIL` 名称。diff 使用上下文与增删颜色，长内容注明展示截断。终端摘要只用于观察，任务结果、文件内容与完整输出仍通过对应原生工具读取。

## 本地主题配置

配置文件依次选择 `--ui-config FILE`、`SSMCP_UI_CONFIG`、`$XDG_CONFIG_HOME/ssmcp/config.toml` 或 `~/.config/ssmcp/config.toml`。命令行覆盖对应配置项。默认路径缺失时使用内置设置；显式路径缺失、格式损坏、未知字段和非法取值会阻止启动。

```toml
[ui]
color = true
log_format = "pretty"
theme = "codex-dark"
timestamps = true
show_request_id = true
show_raw_json = false
max_inline_output = 12
```

支持 `minimal`、`codex-dark`、`nord`、`solarized` 四种主题。`--theme nord`、`--timestamps` 可直接覆盖。时间戳使用 UTC；终端主题不修改原生事件。`NO_COLOR`、`--no-color`、`TERM=dumb` 和重定向 stderr 都关闭 ANSI 颜色。

![终端日志渲染器示例](assets/terminal-logs.png)

上图由项目的真实渲染器运行固定测试事件生成，展示排版与颜色；它是可复现的渲染示例，不是生产命令耗时记录。

## 服务端日志

`ssmcpd` 使用单行 `key=value` 结构，分类包括 `auth`、`oauth`、`identity`、`session`、`mcp`、`proxy`、`timeout` 和 `persistence`。运行日志只写入允许的元信息字段，不记录 HTTP 请求体、查询字符串、OAuth token、client secret 或后端输出。请求 ID 由服务器生成，并在 `X-Request-ID` 响应头返回；身份授权日志包含 GitHub 稳定 subject ID，转发日志包含 client/session ID。

```sh
journalctl -u ssmcpd --since '10 minutes ago' -o cat
RUST_LOG='warn,ssmcpd::proxy=debug,ssmcpd::persistence=debug' ssmcpd serve
```

`RUST_LOG` 支持 `off/error/warn/info/debug/trace` 全局级别以及 `ssmcpd::分类=级别` 或 `ssmcp::分类=级别`，更具体的分类优先；默认 `info`。健康检查成功记录属于 debug，避免日常刷屏。终端交互时自动着色，systemd journal 中保持无 ANSI 的纯文本记录。

## 代理与部署边界

提供 [Caddy](../packaging/Caddyfile) 和 [Nginx](../packaging/nginx-ssmcp.conf.example) 示例：控制接口 30 秒，长调用 930 秒，长输出关闭代理缓冲。930 秒留在内部 910 秒原生转发期限之外，让应用有机会先返回明确结果。Nginx 504 响应提醒先检查已有任务状态，避免重复执行有副作用的命令。访问日志使用不含查询字符串的 `$uri`。

当前公网 MCP 使用 JSON-response Streamable HTTP。反代保留流式及 Upgrade 相关设置；这些设置不会自动增加新的 SSE 或 WebSocket MCP 端点。原生后端进度仍通过现有事件工具读取。

systemd 固定使用 `ssmcpd` 用户，持久状态位于 `/var/lib/ssmcpd`，缓存位于 `/var/cache/ssmcpd`，控制 socket 位于 `/run/ssmcpd`，配置仍位于 `/etc/ssmcpd/ssmcpd.toml`。配置只读，StateDirectory/CacheDirectory/RuntimeDirectory 提供所需写入空间。保留 bubblewrap 所需的 `/proc/sys/user` 例外，避免破坏代码模式隔离。

DCR 注册持久化已包含原子替换、0600 文件、0700 私有目录、单写者锁和损坏文件拒绝启动。授权码、access token 和 refresh token 仍在内存中；重启后保留客户端登记，但当前连接和授权需要重新建立。

参考：[Caddy reverse_proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy)、[Nginx proxy module](https://nginx.org/en/docs/http/ngx_http_proxy_module.html)、[systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)。

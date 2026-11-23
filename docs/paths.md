# 查询 SSMCP 使用的路径

`ssmcp paths` 输出当前客户端实际使用的配置、状态和工具目录路径。它在连接和配置加载之前返回，不登录、不联网、不读取配置或凭据内容，也不创建目录。尚不存在的目标也可以查询，适合安装脚本、备份脚本和排查路径设置。

~~~sh
ssmcp paths
ssmcp paths config history
ssmcp paths config history --json
ssmcp paths --list
ssmcp paths --help
~~~

## 名称与路径来源

无名称时按下表顺序输出全部十项。显式选择时保留给定顺序，重复名称只输出第一次；名称区分大小写。

| 名称 | 含义 | 实际路径来源，按优先级排列 |
| --- | --- | --- |
| `config` | 客户端配置文件 | `SSMCP_CLIENT_CONFIG`；否则 `XDG_CONFIG_HOME/ssmcp/config.toml`；否则 `HOME` 或 `USERPROFILE` 下的 `.config/ssmcp/config.toml` |
| `ui-config` | 界面配置文件 | `SSMCP_UI_CONFIG`；否则与 `config` 相同 |
| `auth` | 账号凭据目录，仅显示路径 | `SSMCP_AUTH_HOME`；Windows 默认 `LOCALAPPDATA/ssmcp/auth`；其他平台默认 `XDG_CONFIG_HOME/ssmcp/auth`，未设置时为 `HOME/.config/ssmcp/auth` |
| `trust` | 已信任服务器的公钥固定值文件 | 与 `config` 同目录的 `trusted-servers.toml` |
| `rc` | 工具仓库配置文件 | `XDG_CONFIG_HOME/ssmcp/ssmcprc.toml`；未设置或为空时使用 `HOME` 或 `USERPROFILE` 下的 `.config/ssmcp/ssmcprc.toml` |
| `tools` | 受管工具仓库目录 | `XDG_DATA_HOME/ssmcp/tools`；未设置或为空时使用 `HOME` 或 `USERPROFILE` 下的 `.local/share/ssmcp/tools` |
| `history` | 本地对话历史目录 | Windows 为 `LOCALAPPDATA/ssmcp/conversations`；其他平台为非空 `XDG_STATE_HOME/ssmcp/conversations`，未设置或为空时为 `HOME/.local/state/ssmcp/conversations` |
| `sessions` | 保存的后台会话目录 | `SSMCP_SESSION_HOME`；Linux 默认 `XDG_STATE_HOME/ssmcp/sessions`，未设置时为 `HOME/.local/state/ssmcp/sessions`；macOS 默认 `HOME/Library/Application Support/ssmcp/sessions`；Windows 默认 `LOCALAPPDATA/ssmcp/sessions` |
| `cwd` | 本次调用的工作目录 | 操作系统返回的当前工作目录 |
| `executable` | 本次调用的可执行文件 | 操作系统返回的当前程序路径 |

路径计算复用对应功能的现有规则。要求绝对路径的配置项若被设为相对路径，查询也会返回原有错误；`SSMCP_UI_CONFIG` 保持 UI 的规则，可以输出相对路径。查询不会展开变量值中的 `~`、`$HOME` 或其他 Shell 表达式。`rc` 和 `tools` 沿用工具仓库对绝对 `HOME`／`USERPROFILE` 及有效工作目录的要求，即使 XDG 路径已设置也是如此；只解析工作目录身份，不加载 `ssmcprc.toml`。默认后台会话路径仅支持 Linux、macOS 和 Windows。

单独查看工具的分层位置可用 `ssmcp tools paths`，其 JSON 返回 `rc`、`official`、`global`、`project` 和加载顺序。全局目录是 `~/.ssmcp/tool/`，项目目录是当前工作区的 `./.ssmcp/tools/`；该查询不读取 RC、不登记来源、不创建目录。实际加载与启停规则见[工具仓库管理](tool-repositories.md)。

## 枚举、组合与输出

默认格式每行是 `名称`、一个制表符、`路径`。`--plain` 每行只输出路径，适合选择单个名称后交给另一个命令；调用者仍应按所用 Shell 的规则引用结果。

~~~sh
# 显示配置文件、历史目录；保留这个顺序。
ssmcp paths config history --plain

# POSIX Shell：把配置路径作为一个参数传递，路径含空格也不会拆开。
config_path=$(ssmcp paths config --plain) && printf '%s\n' "$config_path"

# 只列出可选名称，不解析 HOME、XDG 或任何目标路径。
ssmcp paths --list --plain
~~~

`--json` 输出名称到路径的 JSON 对象，键的输出顺序与选择顺序一致。下面的路径只是格式示例，实际值由当前环境决定。

~~~json
{
  "history": "/home/example/.local/state/ssmcp/conversations",
  "config": "/home/example/.config/ssmcp/config.toml"
}
~~~

~~~sh
ssmcp paths history config --json
ssmcp paths --list --json
~~~

`--list --json` 输出名称到说明的对象，`--list --plain` 只输出名称。`--list` 不接受具体名称，`--json` 与 `--plain` 互斥，`--help`／`-h` 单独使用。未知名称、未知选项和冲突参数会在解析路径之前失败。

只有选中的路径需要可解析：例如账号目录设置错误不会阻止 `ssmcp paths cwd --plain`。所有选中项都解析成功后才输出，因此第二项失败时不会留下看似完整的第一项结果。路径须能表示为 UTF-8；普通文本输出拒绝路径中的控制字符，此时可用 `--json` 保留转义后的原值。

查询只提供位置，不承诺目标存在、配置合法或当前用户具有访问权限。要检查配置或工具仓库内容，使用相应的 `config`、`ui`、`tools` 等命令。

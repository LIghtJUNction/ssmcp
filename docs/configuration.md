# 配置、SSH 公钥与登录

ssmcp 直接调用系统 OpenSSH。已经能用公钥登录的服务器，可以复用现有密钥、`~/.ssh/config` 和本机 `ssh-agent`；在另一个项目目录启动客户端时，无须再次上传公钥。Linux、macOS 与 Windows 客户端连接同一个 Linux 网关。安装预编译程序或从 crates.io 安装见[版本与安装](releases.md)；本文从已有服务器开始，完整部署见 [Linux 部署](deployment.md)。

| 你要做什么 | 从这里开始 |
| --- | --- |
| 安装客户端、选择 Windows／macOS／Linux 下载 | [平台与安装](releases.md) |
| 执行命令、并行任务或继续等待已有工作 | [独立命令指南](commands.md) |
| 从旧版 `-t`／`--backend` 命令迁移 | [参数迁移](backends.md#从旧的后端选择参数迁移) |
| 复用现有 SSH 配置、用别名连接 | [SSH 配置](#ssh-config) |
| 首次安装或更换公钥 | [公钥管理](#public-keys) |
| 在 ChatGPT 或其他 MCP 客户端完成登录 | [OAuth 登录](#oauth-login) |
| 用 GitHub 身份控制指定客户端 | [`--allow` 与访问规则](#github-allow) |
| 了解令牌怎样自动续期 | [静默续期](#silent-refresh) |
| 判断重新打开后是否要再次授权 | [连接与授权生命周期](#session-lifetime) |
| 处理 `Permission denied`、过期登录码或 `invalid_grant` | [排查问题](#troubleshooting) |

## 先分清三个位置

| 位置 | 保存什么 | 负责什么 |
| --- | --- | --- |
| 本机电脑 | 项目、OpenSSH 私钥、harness 原生配置 | 在项目目录启动 `ssmcp`，运行 shell 与自动发现的原生工具，通过 SSH 连到服务器 |
| SSH 服务器 | 用户公钥、`ssmcpd`、HTTPS 入口 | OpenSSH 验证本机身份，`ssmcpd` 转发 MCP 调用 |
| MCP 客户端，例如 ChatGPT | MCP URL、OAuth 令牌 | 用户授权后调用 `list` 与 `code_mode` |

SSH 公钥确认本机能以哪个服务器账户接入。`--allow` 指定允许操作该连接的 GitHub 身份，浏览器中的 OAuth 流程再取得用户对当前 MCP 客户端的同意。省略 `--allow` 时继续使用同一 SSH 账户下的登录码授权。[客户端实现](../src/client.rs) · [授权实现](../src/oauth.rs)

<a id="ssh-config"></a>

## 1. 复用已有 SSH 配置

在本机 OpenSSH 用户配置中使用一个指向 ssmcp 账户的条目：Linux／macOS 通常为 `~/.ssh/config`，Windows 为 `%USERPROFILE%\.ssh\config`。如果已有条目可用，直接复用；下面的主机名、账户和密钥路径需要换成你的实际值。

```sshconfig
Host my-ssmcp
    HostName ssmcp.example.com
    User alice
    Port 22
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
```

`Host` 是本机别名，`HostName` 是服务器地址，`User` 是服务器上的 Linux 账户。GitHub 模式返回统一 MCP URL `https://ssmcp.example.com/mcp`；登录码模式返回服务端实际 SSH 用户路径，例如 `https://ssmcp.example.com/alice`。SSH 配置中的 `User` 与 `--allow` 中的 GitHub 用户名各有用途。

先查看 OpenSSH 解析出的配置。这一步不建立 SSH 连接；Linux／macOS 可用：

```sh
ssh -G my-ssmcp | awk '$1 ~ /^(hostname|user|port|identityfile|identitiesonly|identityagent|proxyjump|proxycommand)$/'
```

Windows PowerShell 可用：

```powershell
ssh -G my-ssmcp | Select-String '^(hostname|user|port|identityfile|identitiesonly|identityagent|proxyjump|proxycommand) '
```

在你的项目目录中启动客户端：

```sh
# 显式填写服务器账户，其他配置从 Host 条目读取。
ssmcp --name ssmcp-project --title "维护当前项目" alice@my-ssmcp

# User 已在 SSH 配置中设置时，只需填写 Host 别名。
ssmcp --name ssmcp-project --title "维护当前项目" my-ssmcp
```

没有设置 `User` 的裸主机目标，会采用 OpenSSH 自身的用户名规则；通常为本机用户名。想固定账户时，在配置中写明 `User`，或继续使用 `alice@my-ssmcp`。`cwd` 是启动 ssmcp 时的本机目录快照，后续通过 `list` 可以找到它。

### 独立配置文件、端口与密钥

```sh
# -F 使用另一份 OpenSSH 配置。
ssmcp -F ~/.ssh/work-config alice@my-ssmcp

# -p 显式选择端口；-i 提供要使用的私钥路径。
ssmcp -p 2222 -i ~/.ssh/id_ed25519_work alice@my-ssmcp
```

`-i` 接受私钥文件，上传服务器时使用对应的 `.pub` 公钥文件。OpenSSH 可以同时配置多个 `IdentityFile`，因此单独增加 `-i` 不会排除其他密钥；需要验证某一把新密钥时，使用下文的[独立验证配置](#rotate-key)。

OpenSSH 会处理 `HostName`、`User`、`Port`、`IdentityFile`、`IdentitiesOnly`、`IdentityAgent`、`Include`、`ProxyJump` 和 `ProxyCommand` 等配置。跳板机需允许相应的 SSH 转发。ssmcp 自身保持公钥认证、禁用连接复用和目标连接的转发；自定义 `RemoteCommand`、`SessionType` 等会改变 SSH 会话用途的设置应放在其他 Host 条目中。[SSH 命令构建](../src/client.rs) · [OpenSSH ssh_config](https://man.openbsd.org/ssh_config)

### 有口令的私钥与 ssh-agent

私钥有口令时，可以按 OpenSSH 的原生提示解锁，也可以把密钥加入已经运行的本机 agent：

```sh
ssh-add -l
ssh-add ~/.ssh/id_ed25519
```

如果终端提示无法连接 agent，按桌面环境的方式启用它；Unix 独立 shell 也可运行 `eval "$(ssh-agent -s)"` 后再执行 `ssh-add`。ssmcp 允许本机 agent 参与认证，并关闭向服务器转发 agent。[客户端实现](../src/client.rs)

首次连接出现主机指纹时，向服务器管理员核对。服务端可用 `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` 查看对应主机公钥指纹；确认一致后接受。以后遇到主机密钥变化，先核实服务器是否重建或轮换密钥。[OpenSSH ssh](https://man.openbsd.org/ssh)

<a id="public-keys"></a>
<a id="更新公钥"></a>

## 2. 添加或更新服务器上的公钥

**已经建立 SSH 连接，说明这次使用的公钥已经被服务器接受。** 更换项目目录、启动第二个客户端或重新登录 OAuth，都不要求重新登记同一把公钥。需要添加新电脑、新密钥或撤销旧密钥时，再更新服务端的授权文件。

仓库提供的服务器配置使用系统 OpenSSH 账户和授权策略。通常公钥位于目标用户的 `~/.ssh/authorized_keys`，ssmcp 没有另一套公钥数据库，也没有自动上传公钥的命令。如果管理员配置了自定义 `AuthorizedKeysFile` 或 `AuthorizedKeysCommand`，应更新相应的实际来源。[服务器配置](../packaging/sshd-ssmcp.conf) · [OpenSSH sshd_config](https://man.openbsd.org/sshd_config)

### 2.1 准备要登记的公钥

本机已有密钥时，复用其 `.pub` 文件。确认文件和指纹：

```sh
ssh-keygen -lf ~/.ssh/id_ed25519.pub
```

需要一把独立密钥时，选择尚未使用的文件名，再运行：

```sh
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_ssmcp -C "ssmcp workstation"
```

这会生成私钥 `id_ed25519_ssmcp` 和公钥 `id_ed25519_ssmcp.pub`。私钥留在本机；下文只传送 `.pub` 文件。已有同名文件时应换一个新名称，保留原密钥。

### 2.2 账户尚未受 ForceCommand 限制

目标账户仍允许正常 SSH 命令执行、且你已有该账户的登录方式时，可使用系统 `ssh-copy-id` 添加公钥：

```sh
ssh-copy-id -i ~/.ssh/id_ed25519.pub alice@ssmcp.example.com
```

自定义端口使用 `-p 2222`，已有可用的 Host 条目也可作为目标。这个步骤依赖服务器已允许的登录方式；ssmcp 本身只使用公钥认证。首次创建账户且没有可用登录方式时，由管理员通过管理账户或服务器控制台安装公钥。

### 2.3 账户已经运行 ssmcp

仓库中的 `Match Group ssmcp` 会把该组的 SSH 命令固定为 `ssmcpd attach`，连 `ssh-copy-id`、SCP/SFTP 请求也会进入桥接程序。此时使用**组外的管理账户或服务器控制台**更新公钥。[ForceCommand 配置](../packaging/sshd-ssmcp.conf)

下面使用 `admin` 管理服务器、为 `alice` 添加公钥。先在本机上传公钥到管理账户：

```sh
scp ~/.ssh/id_ed25519.pub admin@ssmcp.example.com:alice-new.pub
ssh admin@ssmcp.example.com
```

接下来在服务器的管理 shell 中运行。脚本读取目标账户的实际 home 和主组，保留已有密钥；同一公钥已存在时跳过添加，包括带有授权选项的现有条目。

```sh
sudo sh -s -- alice "$HOME/alice-new.pub" <<'SH'
set -eu
target_user=$1
public_key_file=$2
target_home=$(getent passwd "$target_user" | cut -d: -f6)
target_group=$(id -gn "$target_user")
case "$target_home" in
  ""|/|[!/]*) printf 'Expected an absolute user home directory.\n' >&2; exit 1 ;;
esac

ssh-keygen -lf "$public_key_file"
public_key=$(awk '
  NF {
    if (++count != 1 || $1 !~ /^(ssh-|ecdsa-|sk-)/ || NF < 2) exit 1
    print
  }
  END { if (count != 1) exit 1 }
' "$public_key_file")
key_blob=$(printf '%s\n' "$public_key" | awk '{print $2}')
authorized_keys="$target_home/.ssh/authorized_keys"

install -d -m 0700 -o "$target_user" -g "$target_group" "$target_home/.ssh"
touch "$authorized_keys"
chown "$target_user:$target_group" "$authorized_keys"
chmod 0600 "$authorized_keys"
if awk -v key="$key_blob" '
  /^[[:space:]]*#/ { next }
  { for (i = 1; i <= NF; i++) if ($i == key) found = 1 }
  END { exit !found }
' "$authorized_keys"; then
  printf 'Public key already registered.\n'
else
  printf '\n%s\n' "$public_key" >> "$authorized_keys"
  printf 'Public key added; existing keys preserved.\n'
fi
SH
```

脚本适用于常见的每用户 `~/.ssh/authorized_keys` 布局。它只接受一个公钥条目，并把文件权限设为 `0600`、目录设为 `0700`。服务器启用 SELinux 时，还应按发行版要求为该 `.ssh` 目录恢复正确标签。公钥文件更新会影响后续 SSH 认证，无须为此重启 `ssmcpd`。

<a id="rotate-key"></a>

### 2.4 更换密钥：添加、验证、再撤销

先按上面的步骤添加新公钥，并保留旧的可用连接。用单独的 SSH 配置验证新私钥，防止 agent 或其他 `IdentityFile` 意外使用旧密钥完成认证：

```sh
keycheck_config=$(mktemp)
chmod 0600 "$keycheck_config"
cat > "$keycheck_config" <<'SSHCONFIG'
Host new-key-check
    HostName ssmcp.example.com
    User alice
    Port 22
    IdentityFile ~/.ssh/id_ed25519_ssmcp
    IdentitiesOnly yes
    IdentityAgent none
SSHCONFIG

# 修改上面的主机、端口和新私钥路径后运行；旧版 0.3.0 也支持此写法。
ssmcp -F "$keycheck_config" --name new-key-check alice@new-key-check

# 验证连接成功，按 Ctrl-C 退出此临时客户端后清理配置。
rm -- "$keycheck_config"
```

看到新的连接摘要或重定向输出的连接 JSON，说明指定的新私钥通过 SSH 认证并完成客户端初始化。需要跳板机时，把对应 `ProxyJump` 及其 Host 配置加入这份独立文件。[客户端启动流程](../src/client.rs)

验证成功后，在服务器管理 shell 中备份 `authorized_keys`，核对旧公钥指纹与条目，再用 `sudoedit` 只删除旧公钥所在行；保留新公钥、其他设备的公钥和各行的授权选项。最后把常用 Host 条目的 `IdentityFile` 改为新私钥。删除旧公钥会阻止它建立新 SSH 连接；已经建立的连接继续存在，需要在对应本机终端按 `Ctrl-C` 结束，关联 OAuth 授权随之撤销。

<a id="oauth-login"></a>
<a id="登录流程"></a>

## 3. 完成 MCP 登录

<a id="authorization-modes"></a>

### 先选择授权方式

| 启动方式 | MCP URL | 谁能访问 |
| --- | --- | --- |
| 带 `--allow 用户名@github` | `/mcp` | 经 GitHub 验证、且被目标客户端允许的身份 |
| 省略 `--allow` | `/SSH用户名` | 使用该 SSH 账号登录码授权的 MCP 客户端；仅覆盖未设置 `--allow` 的连接 |

两种方式都通过原有 SSH 公钥连接服务器。本机 CLI 无须执行 GitHub 登录，也无需改变现有 SSH 配置。服务器要使用 GitHub 模式，需先配置 [GitHub OAuth App](deployment.md#github-oauth)。

<a id="github-allow"></a>

### GitHub 模式：本机声明允许的用户

先在项目目录检查自动发现的工具，再建立连接：

```sh
ssmcp --list-tools
ssmcp --name ssmcp-project --title "迭代 ssmcp" --allow octocat@github my-ssmcp

# 多个用户可以用逗号分隔，也可以重复指定 --allow。
ssmcp --allow alice@github,bob@github --allow carol@github my-ssmcp
```

将示例用户名换成获准访问的 GitHub 用户名。参数采用 `用户名@提供方` 格式，目前提供方只支持 `github`；最多 32 个不同用户，大小写不敏感，重复项会合并。GitHub 用户名使用 1–39 个 ASCII 字母、数字及中间的单个连字符。`--allow` 需要 SSH 连接，不能与仅检查本机工具的 `--list-tools` 同用。[参数校验](../src/client.rs) · [身份格式](../src/wire.rs)

每条连接都提供交互式 Shell 和独立命令任务。后台检查 `codex`、`claude`、`pi`、`opencode` 的原生接口，安全启动、版本、握手与目录检查通过后才注册工具；某个可选 CLI 失败会显示警告，Shell 和其他成功注册的工具继续可用。旧版启动参数按[迁移说明](backends.md#从旧的后端选择参数迁移)删除；需要的 CLI 放入客户端启动环境的 `PATH`。发现不安装软件或提交模型请求。Pi 通过 `SSMCP_PI_RPC_SOCKET` 连接已初始化的本机私有 Unix RPC 入口；OpenCode 通过 `SSMCP_OPENCODE_URL` 连接已有 loopback 服务。Pi socket 及直接父目录需由当前用户独占、无符号链接，握手还会检查 peer UID。OpenCode 地址只能为带显式端口的 `http://127.0.0.1:端口/`，原生密码由 `OPENCODE_PASSWORD` 提供；不接受代理、重定向或其他 URL 路径。入口未配置或安全检查未通过时提供者保持 unavailable。模型身份、模型选择和权限仍由各原生 CLI 的配置管理。[统一工具目录](backends.md#1-启动客户端)

交互式终端会显示连接地址与登录提示。重定向 stdout 或选择 `--log-format jsonl` 时，连接信息输出为一行 JSON，例如：

```json
{
  "url": "https://ssmcp.example.com/mcp",
  "auth": "github",
  "allow": ["octocat@github"],
  "client_id": "本次连接的唯一ID",
  "name": "ssmcp-project",
  "title": "迭代 ssmcp",
  "type": "shell",
  "cwd": "/home/alice/projects/ssmcp",
  "hostname": "workstation"
}
```

GitHub 模式只输出连接信息与允许列表，不生成登录码。实时活动显示在 stderr；保持进程运行，后端才会在线。不支持 GitHub 策略的旧服务器会使这次启动失败，本机后端不会按旧的登录码方式继续开放。[客户端握手](../src/client.rs)

### 浏览器：GitHub 登录后，确认当前 MCP 客户端

1. 在 ChatGPT 或其他支持 OAuth 的 MCP 客户端中添加终端返回的完整 `/mcp` URL，启动 OAuth 连接。
2. ssmcp 登录页显示 MCP 客户端名称、已登记的回调来源和资源 URL。核对后点击“使用 GitHub 登录”，在 GitHub 页面选择账号并完成授权。
3. 返回 ssmcp 后，核对已验证的 GitHub 身份、当前可访问会话数量和这次请求的 MCP 客户端，再点击“允许连接”或“取消”。GitHub 登录成功后仍需这一步明确同意。
4. 同意后浏览器返回 MCP 客户端。客户端保存 ssmcp 签发的 access token 和 refresh token；GitHub 的令牌只供服务端读取身份，随后释放。
5. 调用 `list`，确认目标客户端的 `title`、`name`、`cwd`；随后用其 `id` 读取原生工具 schema，并通过 `code_mode` 调用。新客户端的 `type` 都为 `shell`，已安装的 Agent 能力以目录为准。

确认页的同意针对当前 OAuth/MCP 客户端，允许它使用当前以及随后明确允许这个 GitHub 身份的连接。允许列表由本机启动参数管理，页面没有逐个后端的勾选配置。没有获准访问的在线客户端时也可完成登录，`list` 会返回空列表；以后出现匹配的在线客户端即可被发现。[授权页面与令牌](../src/oauth.rs) · [按连接检查访问](../src/broker.rs)

服务端在每条连接接入时，把允许列表中的 GitHub 用户名解析为稳定用户 ID；后续按 ID 判断权限。用户名在这条连接在线期间改名，已有的 ID 绑定仍有效。重新启动会再次解析参数中的用户名，因此改名后应更新 `--allow`；当前没有跨连接持久保存的用户名与 ID 绑定。[GitHub 身份解析](../src/identity.rs)

### 登录码模式：保留现有使用方法

```sh
ssmcp --name ssmcp-project my-ssmcp
```

省略 `--allow` 后，终端返回 `/SSH用户名` URL、`login_code` 和 `login_expires_in`。将完整 URL 添加到 MCP 客户端，打开 OAuth 页面，核对客户端名称与回调来源，再输入本次登录码。它从签发起最多有效 1800 秒，输出的剩余秒数根据实际握手耗时计算，成功使用一次即消耗。[登录码输出](../src/client.rs)

该授权绑定签发登录码的 SSH 连接，只能发现和调用同一 SSH 账号下未设置 `--allow` 的连接。即使使用同一个 SSH 账户，登录码授权也不能访问设置了 `--allow` 的客户端。签发连接退出会撤销对应授权。[授权隔离](../src/broker.rs)

远程 MCP 的工具列表始终只有 `list` 和 `code_mode`。本机 `--list-tools` 展示的是后端的原生工具；远程调用这些工具时使用 `code_mode` 中的 `call(clientId, toolName, arguments)`。[两工具接口](../README.md)

两种方式都要求 MCP 客户端支持 Authorization Code、S256 PKCE 和动态客户端注册（DCR）。注册时省略 `grant_types` 默认获得 `authorization_code` 与 `refresh_token`，响应返回实际登记类型；显式只请求 `["authorization_code"]` 的客户端只获得访问令牌。注册、资源绑定及回调细节见 [OAuth 兼容性](deployment.md#oauth-兼容性)。

<a id="silent-refresh"></a>

### 静默续期：访问令牌到期后怎样继续使用

**access token 到期后，有效的 refresh token 可以换取下一组令牌，整个过程无需打开登录页。** 服务端提供刷新接口；MCP 客户端负责在后台发起请求并保存结果。[刷新实现](../src/oauth.rs)

1. 首次登录成功后，客户端保存 `access_token`、`expires_in` 和 `refresh_token`。
2. 客户端在访问令牌到期前刷新，或收到 MCP 的 `401` 与 `WWW-Authenticate` 中的 `error="invalid_token"` 后刷新。
3. 客户端向发现的 token endpoint 发出 `grant_type=refresh_token` 请求，带上当前 refresh token、原来的完整 `resource` URL，以及登记时约定的客户端身份认证。
4. 成功响应包含新的 access token 和新的 refresh token。客户端保存整组新令牌，用新 refresh token 替换旧值，再重试需要授权的请求。同一授权的刷新应串行处理，避免多个请求重放同一个旧 refresh token。

这些都是 ssmcp 签发的令牌。默认访问令牌可用 8 小时；每次成功刷新会把 refresh token 的闲置期限向后延长 30 天，同一授权从首次登录起最多延续 90 天。客户端登记与密钥默认可用 365 天。实际到期时间还受客户端登记和原授权剩余寿命约束，客户端应读取每次令牌响应的 `expires_in`。[默认有效期与配置](deployment.md#oauth-lifetimes)

旧版容易让刷新令牌显得没有作用：省略 `grant_types` 的注册收不到 refresh token；即使已领取，同一授权也只持续 24 小时。0.3.2 延长了默认期限，并为省略该字段的新注册提供刷新授权。客户端仍需实现刷新请求和轮换令牌的保存；是否静默刷新应以所用 MCP 客户端的实际行为为准。

从旧版升级后，完成一次新的客户端注册与 OAuth 登录，让客户端取得新的授权；若应用一直保留旧登记，可移除原 MCP 连接，再添加目标 URL。切换到 GitHub 模式时使用 `/mcp`。升级过程和服务器重启的影响见[升级步骤](deployment.md#upgrade-040)。

### 登录码模式：码过期或已使用

登录码签发后 30 分钟内可成功授权一次。已经登录的 MCP 客户端用 OAuth 令牌继续访问，正常刷新无须再填登录码。重新授权需要一枚新码：可以在同一账户下另启动一个客户端取得新码，或确认当前后端可停止后，退出并重启当前 ssmcp 进程。

新授权会绑定提供新码的那条连接，后续要保持该连接运行。连接退出会清理它的后端，并撤销由它签发的授权；因此，正在执行任务时应先处理好任务再退出。[登录码与连接绑定](../src/broker.rs) · [OAuth 绑定](../src/oauth.rs)

<a id="登录恢复"></a>

### 恢复已失效的 OAuth 授权

单独的 access token 到期应先走上述静默刷新；刷新返回 `invalid_grant`、插件提示 `oauth_token_invalid_grant`，或客户端明确要求重新授权时，按这个顺序恢复：

1. 确认使用的方式。`/mcp` 的 GitHub 授权失效时，重新进行 GitHub 登录并同意当前 MCP 客户端；无须为此重启在线后端。
2. 登录码模式需要新码。当前项目有任务时，可另开一个同账户、未设置 `--allow` 的客户端取得新码；然后对原 `/SSH用户名` MCP 连接重新授权。新授权会绑定提供这枚新码的连接。
3. 如果服务器重启后 MCP 客户端持续使用旧登记、重新授权仍失败，移除该 MCP 连接并用相同 URL 重新添加，让它完成新的动态客户端注册。
4. 授权成功后调用 `list`，重新选择当前在线的 `client_id`。GitHub 模式检查目标连接的 `--allow`，登录码模式还需要保持签发连接在线。

`invalid_grant` 可能来自令牌过期、授权撤销、刷新令牌重放，或登录码模式的签发连接退出；错误本身不指出唯一原因。[刷新校验](../src/oauth.rs) · [重连与撤销测试](../tests/oauth_security.rs)

<a id="session-lifetime"></a>

## 4. 重新打开后，还要登录吗？

**GitHub 授权可以跨本机客户端重连继续使用；登录码授权仍绑定其签发连接。** 两者都要求 MCP 客户端保存有效令牌；0.4.1 的安装服务持久化客户端登记；访问令牌、刷新令牌及尚未完成的授权仍在内存中，服务端重启后需要重新授权。[授权实现](../src/oauth.rs) · [连接与授权检查](../src/broker.rs)

| 操作或状态 | GitHub 模式 | 登录码模式 |
| --- | --- | --- |
| 关闭后重新打开 MCP 应用，保存的令牌有效 | 可继续使用已有授权 | 签发连接仍在线时可继续使用 |
| access token 到期，refresh token 有效 | 支持刷新的客户端可静默续期 | 同左，并要求签发连接仍在线 |
| 本机 ssmcp 退出或重连 | 旧客户端消失；新连接明确允许同一身份时可被原授权发现 | 签发连接退出会撤销授权，需要新码登录 |
| 当前无可访问的在线客户端 | `list` 为空；授权自身可保持有效 | 签发连接必须在线 |
| 新增第二个项目客户端 | 每个客户端按自己的 `--allow` 核对稳定身份 ID | 只包括同一 SSH 账户下未设置 `--allow` 的连接 |
| refresh token 闲置到期或授权达到总期限 | 重新 GitHub 登录并同意 MCP 客户端 | 取得新登录码并重新授权 |
| 服务端重启 `ssmcpd` | 重新连接并登录；启用持久化后保留有效登记 | 同左 |

| 凭据 | 用在哪里 | 有效期 |
| --- | --- | --- |
| SSH 私钥与服务端公钥登记 | OpenSSH 认证 | 由你和服务器管理员管理 |
| `login_code` | 登录码模式的浏览器表单 | 签发后 30 分钟；成功使用一次即消耗 |
| 授权表单事务 | 浏览器中正在进行的登录 | 30 分钟；不会延长登录码自己的期限 |
| OAuth authorization code | 浏览器回调后由 MCP 客户端兑换 | 60 秒；与客户端及 PKCE 绑定 |
| access token | MCP 请求的 `Authorization: Bearer` 请求头 | 默认最长 8 小时 |
| refresh token | MCP 客户端自动续期 | 默认闲置期限 30 天，成功刷新后重新计时；同一授权最长 90 天 |
| OAuth 客户端登记及密钥 | 客户端自动注册与身份认证 | 默认最长 365 天；启用登记持久化后跨重启保留 |

表中为 0.4.0 默认期限；管理员可以调整访问令牌、刷新闲置期、授权总期限和登记期限。令牌寿命还受登记到期时间约束。GitHub 授权绑定已验证的身份与 MCP 客户端，每次调用仍要匹配目标连接的允许列表；登录码授权另外绑定签发连接。服务重启和授权撤销会使两种方式的令牌提前失效，刷新不会推迟首次授权起算的总期限。[服务端有效期设置](deployment.md#oauth-lifetimes) · [生命周期校验](../src/oauth.rs)

<a id="troubleshooting"></a>

## 5. 排查问题

| 现象 | 检查与处理 |
| --- | --- |
| `destination must be user@host` 或 `expected user@host` | 当前运行的客户端仍要求显式用户名。使用 `ssmcp alice@my-ssmcp`，或更新到 0.3.1 及更新版本 |
| `Permission denied (publickey)` | 用 `ssh -G my-ssmcp` 确认主机、账户、端口和密钥；核对服务端实际授权文件、文件所有者与权限。加密私钥需能解锁，本机 agent 可用 `ssh-add -l` 检查 |
| `Too many authentication failures` | 为专用 Host 指定 `IdentityFile` 与 `IdentitiesOnly yes`，减少 agent 提供的无关密钥；验证新密钥时使用[独立配置](#rotate-key) |
| `ssh-copy-id`、SCP/SFTP 挂起或输出协议错误 | 目标账户已被 `ForceCommand` 固定为桥接。改用组外管理账户或控制台安装公钥 |
| 主机密钥不匹配 | 向管理员核对新指纹与变更原因，再更新对应 known_hosts 条目 |
| 需要连接 JSON，终端却显示可读的连接提示 | 重定向 stdout，或使用 `--log-format jsonl`；交互式终端默认显示连接摘要 |
| 无法建立连接，或 Shell 启动失败 | 先运行 `ssmcp --list-tools` 检查本机初始化；核对 `--shell` 的绝对路径和执行权限，再检查 OpenSSH 认证与服务端日志 |
| `list` 没有某个原生 Agent 的工具 | 确认该 CLI 在客户端启动环境的 `PATH`，查看发现阶段警告与 `ssmcp.harnesses.list`；修复其版本或配置后重载客户端。可选 CLI 失败不会停用 Shell |
| `--allow` 被拒绝、服务器未确认 GitHub 策略 | 确认客户端与服务器均支持 `--allow` 身份策略，服务器已配置 GitHub OAuth。该错误会停止启动，不会退回登录码授权 |
| GitHub 身份解析失败 | 检查 `--allow` 用户名、服务端访问 GitHub API 的网络及响应；用户名改名后更新启动参数 |
| GitHub 登录后 `list` 为空 | 检查浏览器登录的 GitHub 账号与在线客户端 `--allow` 是否一致；未获准的客户端不会显示 |
| 登录码无效、过期或已使用 | 使用当前连接的未使用码；没有有效码时新建连接取得。核对 OAuth 页面资源 URL 与终端输出完全一致 |
| MCP 返回 `401`、`error="invalid_token"` | 客户端用当前 refresh token 静默刷新，再用新访问令牌重试；刷新请求须带原 `resource` 和登记时的客户端认证 |
| 已登录，但客户端从不自动续期 | 检查首次令牌响应是否有 `refresh_token`、客户端是否实现刷新和保存轮换后的新值。旧版登记可重新注册；显式只登记 `authorization_code` 时需修改客户端注册请求 |
| `oauth_token_invalid_grant` / `invalid_grant` / 要求重新授权 | 检查授权寿命与服务器重启情况，按当前模式重新授权；登录码模式还需检查签发连接是否退出 |
| 服务重启后仍使用旧 `client_id`，重复授权失败 | MCP 客户端应重新注册；若它继续缓存旧登记，移除该 MCP 连接后用相同完整 URL 重新添加 |
| 登录成功后 `list` 找不到原来的 `client_id` | 客户端重连会分配新 ID。重新调用 `list`，按标题、名称和目录确认当前在线 ID；标签可能重复 |
| `list` 正常，`code_mode` 报系统运行环境不可用 | 在服务器检查系统 Node.js、bubblewrap、prlimit 和用户命名空间，见[部署说明](deployment.md#系统代码运行环境) |
| `exec.run` 返回 `running:true` 或 `wait_timed_out:true` | 保存 `job_id`，继续 `exec.wait` 或 `exec.read`。等待到期只结束本次等待；不要重复启动同一项工作 |
| `reload` 返回 busy | 先等待在途工具调用结束；重载会停止后台命令与转发，先处理需要保留的工作 |

需要定位 SSH 认证细节时，可在本机运行 `ssh -v -T alice@my-ssmcp` 查看认证阶段日志。受限账户认证成功后进入桥接，手工 SSH 会等待协议数据；看到认证成功后按 `Ctrl-C` 退出，再使用 ssmcp。分享诊断信息前移除登录码、OAuth 令牌和不希望公开的主机或路径信息。

## 参考与实现依据

- [ssmcp 客户端参数与 OpenSSH 调用](../src/client.rs)
- [OpenSSH 服务端限制配置](../packaging/sshd-ssmcp.conf)
- [OAuth 实现](../src/oauth.rs) 与 [OAuth 测试](../tests/oauth_security.rs)
- [GitHub 身份验证与稳定 ID](../src/identity.rs)
- [Linux 部署与升级](deployment.md)
- [原生后端配置与接口](backends.md)
- [OpenSSH ssh](https://man.openbsd.org/ssh)、[ssh_config](https://man.openbsd.org/ssh_config)、[sshd_config](https://man.openbsd.org/sshd_config)


## 本地显示与重载

默认显示留白较多的 `minimal` 界面，连接摘要给出 MCP URL 和登录方式，活动记录写入 stderr。请求、完成、错误和超时使用不同颜色；独立命令按任务标识区分，stdout 与 stderr 分开显示。请求 ID、方法和耗时用于对应同一次调用，统一 diff 显示增加、删除及原始行号。[本地渲染](../src/client/render.rs)

`--log-format pretty-verbose` 展开脱敏后的详细元数据，`--log-format jsonl` 输出脱敏事件 JSON；`--verbose` 是详细事件的别名，`--raw` 与 `--raw-events` 是 JSONL 的别名。重定向 stdout 时，连接信息始终是 JSON。`--no-color`、`NO_COLOR=1`、`TERM=dumb` 或重定向 stderr 会禁用 ANSI 颜色。这些配色设置不影响真实终端的交互式 PTY 字节透传。后台 stdout/stderr 按任务和流实时显示，读取历史 `wait/read` 结果不再重复打印正文。默认 pretty 使用短任务号和相对工作区路径，折叠内联脚本与 JSON stdout；完整值保留在事件和输出中。`[ui] activity_interval_secs = 5` 控制静默活动提示，设为 0 只关闭周期提示；只有原生事件提供真实总量时才显示百分比。主题、时间戳、折叠行数和 TOML 配置见[日志与显示说明](logging.md)。

```sh
ssmcp --title "检查构建与测试" --allow octocat@github my-ssmcp
ssmcp --theme nord --timestamps --allow octocat@github my-ssmcp
ssmcp --log-format jsonl --allow octocat@github my-ssmcp
NO_COLOR=1 ssmcp --allow octocat@github my-ssmcp
```

更新已安装的程序后，可以从 `code_mode` 调用 `await reload(clientId)`，让客户端从原安装入口重新启动，并保留启动参数、目录、环境和 `--allow`。先确认目录包含 `ssmcp.client.reload`；不支持该工具的旧客户端，初次升级须在本机普通重启。`accepted:true` 只是接收应答，应再调用 `list` 确认重连及新的 ID。重载会停止本连接拥有的命令任务和转发、关闭原生桥接；不会终止操作者已运行的外部 Pi/OpenCode 服务；在途调用会导致 busy 拒绝。[重载协议与回归测试](../tests/client_reload.rs)

GitHub 授权在仍有效、且新连接允许同一身份时继续可用。登录码授权绑定旧连接，重载断线后需要新登录码和新 OAuth 授权。调用方应按实际授权方式处理重连，避免把“程序已更新”或“重载已接收”报告为“新版本已经在线”。[重载完整说明](backends.md#9-更新后的客户端重载)


成功文件读写和可查询 Diff 来自原生操作完成后的证据；敏感、二进制或无法取得的快照明确标为未知。技能发现、正文读取和实际调用分别记录，Harness 外部副作用不作完整审计承诺。完整事件范围见[文件与技能审计](backends.md#文件与技能审计)。

## 会话标题与输出保留

`--title` 为这一条连接附加任务说明，`--name` 用于辨认机器或工作区；两者都不代替唯一客户端 ID。标题支持中文和空格，最多 512 个 UTF-8 字节。升级时保留原有 `--allow` 与 SSH 选项即可，无须为了增加标题重新登记同一把 SSH 公钥；首次使用标题前应确认服务端支持该元数据字段。[参数实现](../src/client.rs) · [会话说明](session-control.md)

```sh
ssmcp --name dev --title "修复构建并准备发布" --allow octocat@github my-ssmcp
```

重载只会重新启动原入口，不会下载更新或选择另一安装目录。Windows 手动按版本目录安装时，切换版本需停止旧进程并从新目录启动；使用安装器更新同一入口后，才可按上述重载流程接管。

长输出超过 32 KiB 或 500 行时会按来源转储，调用方获得转储 ID、客户端路径、SHA-256、字节数和行数，再用 `outputs.read` 分块读取。Unix 的默认目录是 `/tmp/ssmcp/<uid>`，Windows 默认是 `%LOCALAPPDATA%\ssmcp\output`，无该变量时使用用户临时目录下的 `ssmcp/output`；该路径属于目标客户端；文件仍按连接归属校验，其他客户端不能读取。默认保留 24 小时，已结束文件总量上限 512 MiB，单文件上限 256 MiB，运行期间每 60 秒清理一次。活动文件被跳过；达到单文件上限或发生写入错误时，应检查 `complete` 与 `error`，不能假定已保存完整输出。[输出实现](../src/output.rs) · [读取与清理](session-control.md#按需读取正文)

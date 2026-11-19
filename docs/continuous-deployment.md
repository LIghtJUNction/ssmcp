# 自动部署 ssmcp 服务器

正式版本发布后，GitHub Actions 自动部署对应的 Linux 服务器归档。门户 HTML、CSS 和脚本内嵌在 `ssmcpd` 中，因此前端和服务端随同一个版本切换。工作流不在服务器重新编译代码。

## 发布链路

`Release` 完成源码、六平台构建、安装和运行验证后发布 GitHub Release。`deploy_server` 依赖 `publish_release`，将已经验证的标签与完整提交传给可复用的 `Deploy server` 工作流。该工作流也提供手动入口，用于重新部署一个已经发布的稳定版本。

服务器从固定的 `LIghtJUNction/ssmcp` 仓库读取 `release-manifest.json`，核对版本、提交、归档名称、大小和 SHA-256。它只提取服务程序及配套 `code-mode.js`，并核对脚本与该提交的源码一致。预览版本、来源不符、归档损坏和自动降级都会失败。

## 服务器切换

候选程序先在独立 systemd 实例中运行，使用新建的私有状态目录和关闭的购买功能。运行检查、OAuth 元数据、未登录访问拒绝、客户端注册后重启、门户资源的内容指纹全部通过，才会修改正式服务的启动命令。核心 CSS 和复制脚本必须存在；品牌图片从候选页面的指纹地址收集，支持 SVG、PNG 和 WebP，并逐个校验实际内容、长度和 MIME。正式切换后，公开页面必须返回同一组资源。

正式切换只管理 `/etc/systemd/system/ssmcpd.service.d/zzz-release.conf`。现有人工 drop-in、配置、代理和旧运行文件保留原值；已有的 `check-runtime` 启动检查替换为新程序自己的检查。公开页面、价格与法律区域和运行指纹验收失败时，部署恢复原来的自有 drop-in，并重启部署前实际运行的程序。

部署由独立 systemd worker 完成。SSH 或 Actions 连接中断后，可以用相同 run ID 读取同一操作的结果；工作流不会把连接中断当成成功。回执仅在验证和清理完成后标记 `completed: true`，并作为 Actions artifact 保存 14 天。

## 初始化配置

服务器一次性安装两个 root 所有的程序：`scripts/deploy_release.py` 安装为 `/usr/local/libexec/ssmcp/deploy-release`，`scripts/deploy_entry.py` 安装为 `/usr/local/libexec/ssmcp/deploy-entry`。专用账户 `ssmcp-deploy` 使用新建的部署钥匙；其 authorized key 必须设置 `restrict` 和固定的 `deploy-entry` command，sudo 仅允许调用固定的 `deploy-release` 程序。

入口只接受 `status`，或 `deploy vX.Y.Z COMMIT RUN_ID-ATTEMPT`。标签、40 位提交和数字 run ID 都要通过严格校验；SSH 输入不会交给 shell 执行。`status` 用于确认账户与钥匙安装正确，不切换服务。

| GitHub 配置 | 类型 | 用途 |
| --- | --- | --- |
| `SSMCP_DEPLOY_HOST` | Secret | SSH 主机地址 |
| `SSMCP_DEPLOY_SSH_KEY` | Secret | 专用部署私钥 |
| `SSMCP_DEPLOY_KNOWN_HOSTS` | Secret | 从已有可信连接核对的服务器主机公钥 |
| `SSMCP_DEPLOY_PORT` | Variable | SSH 端口，默认 `22` |
| `SSMCP_DEPLOY_USER` | Variable | 专用账户，默认 `ssmcp-deploy` |

SSH 始终启用严格主机公钥检查。缺少必需配置会直接使部署失败；部署钥匙不提供交互式 shell、端口转发或任意 sudo 命令。服务器采用现有 `/etc/ssmcpd/ssmcpd.toml` 配置及以公开域名命名的 Nginx `conf.d` 文件。

## 验证与清理

运行 `python3 -W error -m unittest discover -s scripts -p 'test_server_deploy.py' -v` 可执行离线回归。测试覆盖发布身份、归档来源和完整性、SSH 输入、原有文件保护，以及首次和后续部署的成功切换与失败回滚。测试模拟系统命令和网络，真实服务器的结果以部署回执为准。

每轮完成后清理下载包、隔离状态和临时恢复材料。旧的自有运行目录只有在仍可从官方 Release 取得相同归档、文件未被修改且没有进程或服务引用时才会移除。人工安装的目录保留给运维管理；停止状态无法确认的隔离实例会保留现场，并使部署失败。

| 要核对的结论 | 证据 |
| --- | --- |
| 只接收已发布的指定版本和提交 | `select_release`、归档校验及对应离线测试 |
| 人工配置保持，原运行程序可以恢复 | 首次与后续部署的四项切换/回滚测试 |
| 前后端同版生效 | 运行程序指纹和实际 HTTP 资源指纹回执 |
| 真正完成部署 | `status=deployed`、`completed=true`，且 `running_sha256=target_sha256` |

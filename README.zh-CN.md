# SSMCP

**通过一个经过身份验证的 MCP 端点，让 AI Agent 操作你的开发设备。**

[![CI](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/LIghtJUNction/ssmcp/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/ssmcp.svg)](https://crates.io/crates/ssmcp) [![MIT 许可证](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[English](README.md) · 简体中文

[快速开始](#快速开始) · [演示](#演示) · [文档](#文档) · [参与开发](#参与开发)

SSMCP 是用 Rust 编写的 MCP 客户端和中继服务。在项目目录启动会话，完成设备授权，再让 MCP 应用连接中继。Agent 可以发现已获准访问的设备，并调用选中会话公开的工具。设备主动向中继建立连接，无需公网 IP。

- **使用自己的开发环境。** 在源码、工具和依赖所在的设备上运行命令、操作终端。
- **执行长任务。** 独立启动命令，按任务 ID 读取结果，并通过事件等待工具输出或人工输入。
- **让多个 Agent 协作。** 通过 Worker 池分派任务、发送定向消息，并在同一会话中查看进展。
- **选择使用界面。** 可用终端界面、浏览器工作台和已发布的桌面或移动客户端，也可以自建中继。

## 演示

[![SSMCP 演示视频预览，画面中有三个项目会话](https://i.ytimg.com/vi/9LqpS5BXRTQ/hqdefault.jpg)](https://youtu.be/9LqpS5BXRTQ)

[哔哩哔哩](https://b23.tv/wA4gzmV) · [抖音](https://v.douyin.com/_3yRZNA-Qls/) · [YouTube 英文演示](https://youtu.be/9LqpS5BXRTQ)

安装后可以用示例数据预览终端界面，无需登录或连接设备。

```sh
ssmcp ui preview
```

## 快速开始

### 安装客户端

从 [GitHub Releases](https://github.com/LIghtJUNction/ssmcp/releases/latest) 下载对应平台的压缩包，也可在同一页面找到安装脚本与 `SHA256SUMS`。如果已有兼容的 Rust 工具链，可以从 crates.io 安装 CLI。

```sh
cargo install ssmcp --locked --bin ssmcp
```

Linux 和 macOS 是首要支持平台。Windows、Android 和桌面版的安装方式见[平台与发布说明](docs/releases.md)。

在 Omarchy 上，[SSMCP 状态栏插件](packaging/omarchy/README.md)内嵌了可折叠的会话终端，并显示最近 10 分钟的工具调用次数。

### 连接项目

在项目目录运行以下命令，将 `alice` 替换为你的中继用户名。加上 `--webui` 即可打开浏览器工作台。

```sh
ssmcp alice@ssmcp.lmm.best --name dev
```

首次连接时按提示登录设备，并确认可控制会话的账号。自建中继时，将 `ssmcp.lmm.best` 替换为自己的服务器地址。

### 接入 MCP

在支持 OAuth 的 MCP 应用里添加 SSMCP 显示的 HTTPS MCP 地址，完成应用授权，再用 `list` 找到在线会话。在 `code_mode` 中用这次连接返回的客户端 ID 调用工具。

```javascript
const clientId = "CLIENT_ID_FROM_LIST";
return await exec.run(clientId, {
  command: ["git", "status", "--short"],
  wait_ms: 5000
});
```

命令以 SSMCP 客户端的系统权限运行。只将控制权授予你信任的账号和应用。[设备授权和 MCP 接入](https://ssmcp.lmm.best/guide/zh)

## 文档

| 主题 | 入口 |
| --- | --- |
| 安装和接入 | [入门指南](https://ssmcp.lmm.best/guide/zh) · [平台与发布说明](docs/releases.md) |
| 本地运行或自建 | [本地模式](docs/local-mode.md) · [部署中继](docs/deployment.md) |
| 工具调用和任务管理 | [Code Mode](docs/code-mode.md) · [命令执行](docs/commands.md) |
| Agent 协作 | [协作机制](docs/agent-collaboration.md) · [Worker 池](docs/workers.md) |
| 使用和扩展客户端 | [客户端总览](docs/clients.md) · [扩展接口](docs/client-extension.md) |
| 完整用法 | [中文使用指南](docs/guide.zh-CN.md) |

## 参与开发

递归克隆仓库，并使用项目固定的 Rust 工具链编译。

```sh
git clone --recurse-submodules https://github.com/LIghtJUNction/ssmcp.git
cd ssmcp
cargo build --locked --bins
make check
```

反馈问题时请附上 SSMCP 版本、平台和复现步骤。[GitHub Issues](https://github.com/LIghtJUNction/ssmcp/issues)

## 许可证

[MIT](LICENSE)

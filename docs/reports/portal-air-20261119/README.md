# ssmcp 账号页与品牌视觉更新

日期：2026-11-19（Asia/Taipei）  
代码版本：`86aa47acb688b23c5f7cc79409e78df81ff86af8`  
审阅入口：[PR #41](https://github.com/LIghtJUNction/ssmcp/pull/41)

## 交付结果

账号页采用近黑背景、浅薄荷色重点和宽留白。标题、账号操作、流量和连接指南各自形成清楚的阅读顺序，降低首屏信息密度。桌面保留宽阔的双栏布局，小屏幕按内容顺序自然堆叠。

新增原创 SVG 标识：一条连接两个端点的 S 形线路。标识同时用于页头和浏览器图标，无第三方图标或字体依赖。

## 页面与行为

- 首屏保留一个主要操作“连接电脑”，会话、历史记录和流量订单使用次级入口。
- 账号卡展示服务器用户名与 GitHub 账号；管理员的用户管理入口保留，并沿用现有可见性条件。
- 流量从账号卡独立出来。剩余、已用、额度先展示，低频规则收在可展开说明中；耗尽和仅剩 1 字节的状态仍明确可辨。
- 安装步骤采用说明与命令并列的排版。复制按钮保留成功反馈和手动选择回退。
- `#account` 仍精确定位到账号卡；移动端不会停在大标题顶部。
- 登录、退出、用户名设置等表单继续使用原来的 POST 路由与 CSRF 字段。
- 关闭 JavaScript 时，主要内容、表单和命令仍可阅读；依赖脚本的复制按钮隐藏。
- 浏览器图标使用同源静态路由，并在现有 CSP 中只增加 `img-src 'self'`。

## 验证结果

| 范围 | 结果 | 说明 |
|---|---:|---|
| 实际 Rust 页面渲染器测试 | 9 / 9 | 以最小类型夹具直接编译仓库 `src/portal/view.rs`；包含管理员入口断言 |
| 响应式页面组合 | 32 / 32 | 8 种状态 × 4 个真实 CSS 视口 |
| 交互检查 | 17 / 17 | 账号锚点、表单方法与 CSRF、用户名约束、流量入口、折叠说明、复制及回退 |
| 无 JavaScript 状态 | 5 / 5 | 访客、已登录、首次注册、额度耗尽、登录禁用 |
| 页面溢出、脚本异常、资源错误 | 0 | 包含实际 CSP 下的 SVG 图标加载 |
| 补丁空白检查 | 通过 | `git diff --check` |

四个视口为 1440×1100、768×1024、390×844 和 320×740。八种页面状态为访客、普通账号、管理员、首次注册、额度耗尽、登录禁用、长用户名和仅剩 1 字节。

浏览器使用仓库真实 Rust 渲染器输出的 HTML、CSS 和 JavaScript；预览服务器使用与页面相同的 CSP。账号、配额和用户名均为合成测试数据。复制检查在测试页面模拟剪贴板成功与拒绝，实际点击按钮，没有写入系统剪贴板。

这些结果覆盖本次前端变更。它们不代表完整后端集成测试，也不证明生产网站已部署。预览没有连接生产数据库、执行真实登录或购买流量。

完整逐项结果见 [browser-qa.json](browser-qa.json)。

## 预览

### 桌面账号页

[桌面账号页](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/portal-air-20261119/desktop-account.png`）

### 桌面访客页

[桌面访客页](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/portal-air-20261119/desktop-guest.png`）

### 安装与连接指南

[安装与连接指南](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/portal-air-20261119/desktop-install.png`）

### 移动端账号与异常状态

| 账号 | 流量耗尽 | 小屏首次注册 |
|---|---|---|
| [移动账号](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/portal-air-20261119/mobile-account.png`） | [移动流量耗尽](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/portal-air-20261119/mobile-exhausted.png`） | [小屏首次注册](https://drive.google.com/file/d/1Rr9t9SpoMdwQJuHbW3eYRbgtg-krhUkF/view)（ZIP 内：`current/docs/reports/portal-air-20261119/small-mobile-registration.png`） |

## 代码范围

本次修改集中在 `src/portal/view.rs`、`portal-refresh.css`、`traffic-meter.js`、`client-install.html`、新增 `favicon.svg`，以及 `src/portal.rs` 的静态图标路由和图片 CSP。页面布局不引入新的前端框架、构建工具或依赖。

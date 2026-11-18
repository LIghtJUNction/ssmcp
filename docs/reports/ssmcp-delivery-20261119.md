# SSMCP 0.10.0 整合与交付记录

更新：2026-11-19（Asia/Taipei）｜本次检查开始于 2026-11-18 21:06 UTC

Logs、Tasks、账号与流量显示已经通过 [PR #43](https://github.com/LIghtJUNction/ssmcp/pull/43) 正常合入 main。合并提交为 `035530624a8a055f0d3cf2c4a695ee28218ecff6`。本记录区分源码合入、组件测试、最终整合验收、公开发布和运行中程序升级。

## 当前交付状态

| 项目 | 已核实状态 |
| --- | --- |
| Logs、Tasks 与账号流量 | 源码已合并；保留最新 main 的整帧输出、门户与部署记录 |
| 最终源码保存 | 远端候选 c81ec562 的完整 tree 与失联前候选完全一致，含文件执行权限 |
| 本地最终验收 | lib-test 编译遭 SIGKILL，Cargo exit 101；实际执行测试数为 0 |
| 正式发布预检 | 首轮 #86 因两处 Clippy 诊断失败；修正已合并，第二轮 #88 正在运行 |
| 公开 0.10.0 发布 | 尚未完成；本轮未创建 v0.10.0 标签，未升级运行中的客户端 |
| 支付启用 | 保持默认关闭；公开结账入口限制尚未验证 |
| 生产网站 | 其他工作流报告首次切换已回滚；当前记录不宣称网站上线完成 |

开发机连接持续超时，备用执行环境随后也被平台标记为不可用。最终编译被终止的原因尚未确认，不能仅凭 SIGKILL 认定为内存不足。较低内存配置的重试脚本已经准备，但从未启动。GitHub 与 Google Drive 连接仍可用，源码和交付记录因此继续得到保存。

## 客户端的实际变化

### Logs、Tasks 与横向画布

原有紧凑工具卡片保留在对话中，完整日志在相邻 Logs 列阅读；Tasks 展示本地原生任务列表、详情与取消入口。八列画布保存各列书签，支持窄屏、键盘提示、作用域明确的技能来源、宠物和附件。`/login open/copy/link`、`/diff single/split` 与图片输入保护继续保留。

Logs 使用共享输入草稿。用户在 Logs 中打字时仍留在这一列，显式 Escape 才返回原书签，同时保留草稿与光标。这与旧浮层的行为不同，因此修正的是过时的 PTY 断言。`/exit` 和 `/quit` 正常退出并恢复终端模式、鼠标和备用屏幕。本次源实现冻结于 [PR #39](https://github.com/LIghtJUNction/ssmcp/pull/39) 的 `5c6f10c16a19880e81b9d7c812f3db8f6b3da651`，由 PR #43 完成整合。PR #39 后来继续加入原生启动和运行时预检等改动，当前已前进到 e8e9f8b；这些后续工作没有包含在本次候选中。

### 账号与剩余流量

侧栏显示当前连接服务器的账号和本月剩余流量，登录名与站内用户名不同时保留后者作为辅助信息。凭据按服务器 origin 选择，服务端返回的 principal 必须与已保存身份匹配；凭据保存了稳定 subject 时，还会核对 subject，避免把其他服务器或账号的余额显示进来。

只有 traffic 对象存在，而且 limit_bytes 与 remaining_bytes 都显式为 null 时，才显示“无限”。未登录、加载中、旧服务器缺字段、字段不完整和请求错误各有明确状态；错误会清除旧余额，不伪装成 0 或无限。GB 使用十进制；大于零但不足 0.01 GB 显示为 `<0.01 GB`，真实零值才显示 0。

读取沿用现有带认证的有界 API，保留 TLS 信任、15 秒超时、响应体上限与禁止重定向。每个终端会话独立异步刷新，刷新间隔至少 60 秒；网络等待期间不持有 UI 状态锁，会话关闭时取消刷新任务。交互预览创建终端会话时不再传入真实服务器，因此不会在填入合成数据之前意外发起账号探测。

### 中文截图与浅色主题

第一个问题来自绘制顺序：后一个单元格的背景会盖住前一个宽字符的右半边。修复后先画完整背景，再画字形。第二个问题来自 vt100 解析结果：中文延续单元格可能没有自己的颜色属性，如果把缺失背景解释为深色默认值，浅色界面就会出现黑色半格。修复后延续单元格继承前一个宽字符的样式，同时保留反色和暗化处理。

这两项改动针对截图渲染器，未改变生产 ANSI 输出协议。四个真实 ANSI→vt_snapshot→PNG 像素夹具已经通过，并检查了修复前后图像；覆盖浅色、深色、相邻不同背景和反色。它们不能替代最终 0.10.0 客户端完整账号页面的截图验收。

## 源码身份与恢复

| 位置 | 完整标识 |
| --- | --- |
| 失联前最终本地提交 | 935e2782bbdd6589917c120a2f9e2a4a6a946bf4 |
| 恢复到 GitHub 的候选提交 | c81ec562eb9d34a64c359520a1c23098fe16b76e |
| 二者相同的完整 Git tree | 762e4d34f5dbd5bf6af72d20d3203c5d08a0c050 |
| PR #43 正常合并提交 | 035530624a8a055f0d3cf2c4a695ee28218ecff6 |
| ssmcp / ztui-core 版本 | 0.10.0 / 0.1.1 |

tree 相同表示全部文件内容与 Git 模式一致；恢复时还纠正了 `scripts/e2e.py` 的可执行位，避免把内容相同但权限不同误记为完全一致。远端通过预期旧 HEAD 校验后正常前进，没有强制覆盖分支。最终合并保留 main 后续的部署记录，因此合并提交的完整 tree 会包含这些额外文档，不应要求它等于候选 tree。

账号四个文件先恢复为提交 `bbf5dbe8161b37c1c2914d05eb1630593d8360c9` 对应的工作候选；实际核对采用原始账号 tree `4cef61b39548b584e4dba7698c808b02f83d00de`。此处以 tree 和 PR #43 的已保存源码为恢复依据，原开发机提交是否存在于其他对象库不作为假设。

## 组件验证与最终验证边界

| 范围 | 实际结果 | 来源与限制 |
| --- | --- | --- |
| Logs / client / context | 590 passed；客户端与 vt_snapshot 构建通过 | 原生整合 3319056；与 5c6f10c 相同源码树 |
| 真实隔离 PTY | 67 checks passed | e6adee6 修正过时断言后；含草稿、Tasks、窄屏和退出恢复 |
| 账号与流量 | 6 账号测试、185 终端测试通过；构建通过 | 731c48c 的账号树；两组与其他测试重叠，不能相加 |
| 中文截图渲染 | 4 个像素夹具通过 | 4928484；tree 2bf5943777cf550159c06c732d863eaf5d20fbac |
| 最终本地整合 | 编译终止；0 个测试执行 | 0ef158d 上 lib-test 编译 SIGKILL，exit 101，216.38 秒 |
| 最终预览单行修正 | 源码恢复并核对 | 935e278 / c81ec56；未新增最终运行验收 |
| 较低内存重试 | 未启动 | 已准备 jobs=1、debug=0、incremental=0、codegen-units=256 |
| 最终正式 Release | 首轮检查失败，六平台发布构建成功；第二轮进行中 | #86 绑定 0355306；#88 validate 已确认 4500a956，后续门槛仍进行中 |

上述通过数都绑定各自源码范围。存在筛选交集，不汇总为唯一测试总数。最终 0.10.0 可执行文件、整套 PTY、SSH/HTTP E2E 与完整账号浅色/深色截图尚未取得本地通过证据；不使用旧 0.7.3 二进制替代。

## 已合入的服务端与交接能力

### 月度额度与调用者保留

[PR #36](https://github.com/LIghtJUNction/ssmcp/pull/36) 已合入普通账号每月 1 GB 免费额度，1 GB 为 1,000,000,000 字节，默认月份边界为 UTC+8。付费额度只属于购买月份，月底到期；管理员额度为无限，速率限制仍保留。角色由服务端账号决定。

额度在转发前持久化预扣，重连或重启不重置使用量，订单发放幂等，损坏账本拒绝继续放行。计量对象是应用协议规定的数据量，不等同于网卡或 TLS 总流量。转发失败或取消不会自动退还已预扣量，重试再次计入。冻结提交 fe8bf21 上的额度、认证、HTTP/WS/SSH 与持久化共 111 项测试实际通过。

调用者列表修复了先截断原始记录、再分组会隐藏其他调用者的问题；保留上限为 128 项和 2 MiB。功能提交 e26c576，audit_view 6 项与 caller 筛选 13 项通过，两组重叠。详见[调用者保留验证记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/retained-callers-20261118.md)。

### 支付、库存与真实收款限制

[PR #37](https://github.com/LIghtJUNction/ssmcp/pull/37) 已合入 Waffo 支付与月度库存。默认销售关闭，可售库存为 0，默认基础价格人民币 1 元/GB。下单要求登录、同源与 CSRF 校验；购买账号、月份、价格和库存由服务端保存。RSA 回调验签之后，仍核对订单、商户、环境、账号、币种、基础金额、税额、应付总额和实扣金额，再幂等发放。

旧月份迟到付款进入人工处理，不挪到新月份。待付款订单原子占库存，只有可信的终态取消证据才释放，本地超时不能证明订单已不可支付。账本隔离环境、商户、店铺和商品，测试环境使用独立 state_dir。冻结提交 c86ba97 的 26 项库测试与 2 项服务端配置测试实际通过；配置测试 Cargo exit 0。辅助汇总首次存在输出转义错误，已依据原始日志与确切测试名纠正，原始回执保留。

真实收款仍有平台入口限制：永久商品链接与公开 Store Slug 可能绕过应用的账号和库存流程直接创建结账。回调校验能阻止错误发额度，却不能撤销已经发生的收费；尚无验证证据表明这些公开入口已被封闭。因此继续保持销售关闭、商品测试或未发布。详见[支付实现与边界](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/commerce.md)及 [Waffo 商品文档](https://docs.waffo.ai/features/products)。

### 主桌面本地交接

[PR #38](https://github.com/LIghtJUNction/ssmcp/pull/38) 已合入原生 computer 保护。默认使用隔离桌面；显式主桌面可观察，七类输入变更需要用户在本地终端执行 `/computer host`。默认有效 15 秒，可选 1–60 秒。本地继续输入、`/computer stop`、到期、替换交接或关闭客户端都会撤销，重连不继承。远端工具不能自行授予或续期。

取消调用时会释放按键与按钮，清理完成之前继续持有本进程串行锁；清理失败则拒绝继续主桌面输入。检测覆盖 ssmcp 本地终端，不覆盖操作系统所有输入，也不是 CDP、shell 或外部工具的系统沙箱。功能树 1686da4 上 29 项通过、4 个既有 ignored，本地命令解析 1 项通过，最终 cargo check 成功；使用假输入，没有向真实主桌面发送测试键鼠。Windows/macOS 真实桌面行为仍需平台验收。详见[使用说明](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/computer-use.md)。

### 门户、整帧输出与部署边界

[PR #42](https://github.com/LIghtJUNction/ssmcp/pull/42) 的门户与终端整帧输出已经进入本次整合基础。后续[前端部署记录](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/reports/frontend-polish-20261119/README.md)报告：首次生产切换因公网静态资源不匹配而回滚，原服务健康检查恢复；这些记录在 PR #43 合并时保留。源码合并与开发机安装记录不证明本次 0.10.0 发布或生产网站再次上线。

## 正式发布流程与剩余工作

第一轮[正式预检 #86](https://github.com/LIghtJUNction/ssmcp/actions/runs/44398010956) 已结束，实际源码为 `035530624a8a055f0d3cf2c4a695ee28218ecff6`。版本与格式校验及 58 项发布护栏测试通过，六个平台发布构建及对应档案成功。六个 checks 任务均在 Clippy 阶段失败，之后的单元测试、SSH/HTTP、PTY 与安装链验收未执行；finalize、源码打包与公开发布任务跳过，没有 verified-crates 产物。

失败定位到 `src/context.rs` 的多余引用，以及 `src/client/ui/pet.rs` 测试中的多余 Vec。按编译器明确建议完成恰好两行语义等价修正，经 [PR #49](https://github.com/LIghtJUNction/ssmcp/pull/49) 正常合并为 `4500a956e5e348a06b20f958afeb9d615af0affd`，tree 为 `95ea69d88a0caf01dd4f36f2660bde234abb5b57`。严格警告与发布门槛保持。

第二轮[正式预检 #88](https://github.com/LIghtJUNction/ssmcp/actions/runs/44399566421) 已启动，validate 日志确认实际验收源码为 `4500a956e5e348a06b20f958afeb9d615af0affd`，版本 0.10.0，publish=false、build_only=true；58 项发布护栏与格式检查通过。release-check 上的触发标记提交为 `2b3e9c4b8b6f5eeccc045d011bf72b8368cd17c6`，不包含文件变化。六平台 checks 与 build 正在执行，后续结果待运行结束后更新。

仓库既有 [Release 工作流](https://github.com/LIghtJUNction/ssmcp/blob/main/.github/workflows/release.yml) 支持 `release-check` 正式预检，并始终从 main 检出。PR #43 因此经正常合并进入预检，保留分支保护；未修改工作流、权限或发布门槛，也未单独增加普通 CI。预检依次要求 validate、checks、六个平台构建和 finalize；源码包使用 `cargo package --workspace --locked` 验证。

六个平台为 Linux x64/ARM64、macOS x64/ARM64、Windows x64/ARM64。构建与安装验证成功也不自动等于真实桌面交互已验证。公开发布还要完成标签身份检查、全部档案与校验和、GitHub Release 及按顺序发布 ztui-core 和 ssmcp，并核对包校验值。ztui-core 继续独立使用 0.1.1，不能因客户端升至 0.10.0 而随意改号。

1. 读取正式运行的 validate 日志，记录其实际选择的完整 SHA；若 main 已前进，重新界定验收对象。

1. 依据真实失败日志修正问题，再通过既有流程预检；所有必须门槛成功前不创建发布标签。

1. 恢复可执行环境后，对最终源码构建的客户端补齐完整账号浅色/深色截图和未被正式流程覆盖的 PTY 交互验收。保留全局 rustc 串行 wrapper 与锁，不终止其他构建，不覆盖其他工作树。

1. 取得现有合法发布入口后再创建 v0.10.0 并完成发布验证。当前 GitHub connector 没有标签创建或 workflow_dispatch 接口，开发机不可达；浏览器连接检查也报告缺少 originating browser context。

1. 发布后另行验证安装与新进程版本。保留已有会话，不能把源码版本号或已下载二进制当作正在运行的进程已升级。

## 恢复所需的本地证据

备用工作树曾位于 `/workspace/scratch/9d70da32b330/ssmcp-logs-final-cloud-20261119`。原始最终编译日志位于 `.local/logs-final-cloud/client-context.log`，结果、环境与冻结信息分别为 `final-results.json`、`environment.json`、`frozen.json`。较低内存重试准备位于 `attempt-2-low-memory/`，没有最终结果文件，因为重试未运行。临时环境目前不可用，因此这些路径只是恢复定位信息，不承诺文件现在可以打开。

GitHub 中的完整候选与合并提交是持久恢复入口。已有账号、渲染、版本和离线预览修复都已进入该源码，恢复后不需要重新手工重建它们。不要对未物化的 partial-clone 根工作树执行 reset，也不要修改其他任务的缓存或正在使用的 target 目录。

## 店铺交接与协议模板

店铺创建按用户安排交给专门浏览器代理，本轮提供[完整中文开店提示词](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/commerce/store-setup-agent-prompt.zh-CN.md)，未创建新店、发起真实付款或编造经营主体资料。

- [服务条款模板](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/terms-of-service.template.zh-CN.md)

- [隐私政策模板](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/privacy-policy.template.zh-CN.md)

- [退款政策模板](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/refund-policy.template.zh-CN.md)

- [模板填写说明](https://github.com/LIghtJUNction/ssmcp/blob/main/docs/legal/README.md)

经营主体、联系方式、保存期限与退款细则仍需真实运营资料；这些是待填写模板，不能记录为已上线的商户法律页面。

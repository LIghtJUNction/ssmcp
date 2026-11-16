//! Static, script-free account pages. All stored display data is escaped.
use super::store::Account;

pub(super) const CSS: &str = r##"
:root{font-family:system-ui,-apple-system,sans-serif;color:#17202a;background:#fafafa;line-height:1.7}*{box-sizing:border-box}body{margin:0}main{max-width:920px;margin:auto;padding:48px 24px 80px}nav{display:flex;justify-content:space-between;gap:20px;margin-bottom:72px}a{color:#215a79;text-underline-offset:4px}nav a{text-decoration:none}h1{font-size:clamp(2rem,5vw,3.3rem);line-height:1.25;letter-spacing:-.04em;font-weight:600;max-width:720px}h2{font-size:1.4rem;margin-top:0}h3{font-size:1rem}p{max-width:720px}.muted,small{color:#596575}.hero{margin-bottom:64px}.card{background:white;border:1px solid #e0e5e9;border-radius:12px;padding:28px;margin:24px 0}input,textarea{display:block;width:100%;padding:12px;border:1px solid #bdc7cf;border-radius:6px;font:inherit;margin:8px 0 20px}textarea{min-height:112px}button,.button{display:inline-block;font:inherit;background:#182d3b;color:white;border:0;border-radius:6px;padding:10px 18px;cursor:pointer;text-decoration:none}button.secondary{background:#edf0f3;color:#17202a}code,pre{font-family:ui-monospace,monospace;font-size:.9em;overflow-wrap:anywhere}pre{background:#f4f6f8;padding:16px;white-space:pre-wrap;border-radius:6px}li{margin:14px 0}ol{padding-left:24px}.key{border-top:1px solid #e0e5e9;padding:18px 0}.key p{margin:6px 0}label{display:block}footer{margin-top:60px;border-top:1px solid #e0e5e9;padding-top:24px}@media(max-width:520px){main{padding:24px 18px 48px}nav{margin-bottom:44px}.card{padding:20px}}
"##;
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
pub(super) fn page(title: &str, body: &str) -> String {
    format!(
        r##"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{} · ssmcp</title><link rel="stylesheet" href="/portal.css"></head><body><main><nav><a href="/">ssmcp</a><span><a href="#guide">连接教程</a> · <a href="https://github.com/LIghtJUNction/ssmcp">GitHub</a></span></nav>{body}<footer class="muted">SSH 接入 · GitHub 身份验证 · MCP 工具调用</footer></main></body></html>"##,
        escape(title)
    )
}
pub(super) fn home(account: Option<&Account>, csrf: Option<&str>, enabled: bool) -> String {
    let csrf = escape(csrf.unwrap_or(""));
    let mut body = String::from(
        r##"<section class="hero"><p class="muted">你的电脑，远程可用。</p><h1>把本机工具<br>连接到 ChatGPT。</h1><p>ssmcp 通过 SSH 将本机工具接入 MCP 服务。项目、模型配置和工具执行留在本机；ChatGPT 可操作你明确授权的在线客户端。</p><a class="button" href="#guide">开始连接</a></section><section class="card" id="account"><h2>1. 登录账户</h2>"##,
    );
    if let Some(a) = account {
        body.push_str(&format!(r##"<p>已登录 GitHub：<strong>{}</strong></p><p>SSH 用户名：<code>{}</code></p><p class="muted">用户名绑定 GitHub numeric ID；GitHub 改名不会改变这个 SSH 用户名或公钥归属。</p><form method="post" action="/account/logout"><input type="hidden" name="csrf" value="{csrf}"><button class="secondary">退出登录</button></form>"##, escape(&a.github_login), escape(&a.username)));
    } else if enabled {
        body.push_str(&format!(r##"<p>使用 GitHub 登录，获取稳定的 SSH 用户名，并管理自己的公钥。</p><form method="post" action="/account/login"><input type="hidden" name="csrf" value="{csrf}"><button>使用 GitHub 登录</button></form>"##));
    } else {
        body.push_str(
            "<p>账户自助功能尚未启用。已有 SSH 账户仍可连接；首次接入请联系管理员登记公钥。</p>",
        );
    }
    body.push_str(r##"</section><section class="card" id="keys"><h2>2. 登记 SSH 公钥</h2><p>在本机查看 <code>~/.ssh/id_ed25519.pub</code>；没有密钥时运行：</p><pre>ssh-keygen -t ed25519</pre><p>只提交 <code>.pub</code> 公钥。私钥保留在本机，不要上传。当前仅接受标准单行 <code>ssh-ed25519</code> 公钥。</p>"##);
    if let Some(a) = account {
        for key in &a.keys {
            body.push_str(&format!(r##"<div class="key"><strong>{}</strong><p><code>{}</code></p><pre>{}</pre><form method="post" action="/account/keys/delete"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="fingerprint" value="{}"><button class="secondary">删除公钥</button></form></div>"##, escape(&key.label), escape(&key.fingerprint), escape(&key.public_key), escape(&key.fingerprint)));
        }
        body.push_str(&format!(r##"<form method="post" action="/account/keys"><input type="hidden" name="csrf" value="{csrf}"><label>备注（可选）<input name="label" maxlength="80" placeholder="例如：笔记本"></label><label>SSH 公钥<textarea name="public_key" maxlength="1024" required placeholder="ssh-ed25519 AAAA…"></textarea></label><button>添加公钥</button></form><p class="muted">每个账户最多 20 把。管理员启用 SSH 同步后，添加与删除通常在 15 秒内生效；同步失败时联系管理员。删除公钥阻止后续认证，不会断开已有连接。</p>"##));
    } else {
        body.push_str("<p class=muted>先登录，再添加、查看或删除自己的公钥。已有 SSH 账户继续使用原来的密钥。</p>");
    }
    let command = if let Some(a) = account {
        format!(
            "ssmcp --allow {}@github {}@ssmcp.lmm.best",
            a.github_login, a.username
        )
    } else {
        "ssmcp --allow 你的GitHub用户名@github 你的SSH用户名@ssmcp.lmm.best".into()
    };
    body.push_str(&format!(r##"</section><section class="card"><h2>3. 启动本机客户端</h2><p>安装 <code>ssmcp</code> 后，在需要操作的项目目录运行：</p><pre>{}</pre><p>使用 Codex 时加上 <code>-t codex</code>；也支持 Shell、Claude Code、Pi 和 OpenCode。请先在本机完成后端安装与登录。首次 SSH 连接需要核对服务器主机密钥。</p><p class="muted">保持进程运行。GitHub 改名后，更新命令中的 <code>--allow</code> 用户名，SSH 用户名不变。</p><a href="https://github.com/LIghtJUNction/ssmcp#安装与升级">安装与升级</a></section>"##, escape(&command)));
    body.push_str(r##"<section class="card" id="guide"><h2>4. 在 ChatGPT 中添加插件</h2><p>先完成 GitHub 登录、公钥登记和本机客户端连接，再进行以下操作：</p><ol><li>打开 <a href="https://chatgpt.com/plugins">https://chatgpt.com/plugins</a>。</li><li>点击 <strong>Add</strong>。</li><li><strong>Name</strong> 填写 <code>ssmcp</code>。</li><li><strong>Connection</strong> 填写 <code>https://ssmcp.lmm.best/mcp</code>。</li><li>勾选同意选项，然后点击 <strong>Create</strong>。</li><li>按提示使用 GitHub 登录，核对身份、MCP 客户端与回调来源，再点击“允许连接”。首页登录与插件授权是两个独立步骤。</li><li>回到 ChatGPT，调用 <code>list</code> 查看在线客户端，再使用 <code>code_mode</code> 调用其工具。列表为空时，检查本机进程和 <code>--allow</code> 是否包含当前 GitHub 用户。</li></ol><p class="muted">插件会获得你所授权客户端的工具能力，可能包含执行命令和修改文件。只连接可信任的应用，按需设置允许名单。</p></section>"##);
    page("连接本机工具", &body)
}

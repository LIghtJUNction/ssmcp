//! Run the installed SSMCP client in a window-owned PTY using the CLI's normal rules.
use crate::pty::{JobConfig, Terminal};
use std::{collections::HashMap, path::Path, sync::Arc};

pub use crate::client::terminal::Replies as TerminalReplies;

pub fn validate_destination(destination: &str) -> Result<(), String> {
    let (user, server) = destination.split_once('@').ok_or_else(|| {
        if destination.is_empty() {
            "请输入用户名和服务器域名。"
        } else {
            "用户名后需要 @ 和服务器域名。"
        }
        .to_owned()
    })?;
    crate::username::normalize(user).map_err(|error| {
        if error.contains("reserved") {
            "这个用户名由服务器保留，请使用你的账户名。"
        } else {
            "用户名需为 3–32 个字符，以字母开头，以字母或数字结尾，可含 _ 和 -。"
        }
        .to_owned()
    })?;
    if server.is_empty() {
        return Err("请填写服务器域名，或选择默认中继。".into());
    }
    crate::server_endpoint::parse_target(destination)
        .map(|_| ())
        .map_err(|_| "服务器地址无效，请检查域名和端口，例如 ssmcp.lmm.best。".into())
}

pub fn launch(
    executable: &Path,
    destination: &str,
    size: (u16, u16),
    output: impl Fn(&[u8]) + Send + Sync + 'static,
) -> crate::Result<Arc<Terminal>> {
    validate_destination(destination)?;
    if !executable.is_absolute() {
        return Err("CLI 路径必须为绝对路径。".into());
    }
    let argv = [
        executable.to_str().ok_or("CLI 路径不是 UTF-8")?.to_owned(),
        destination.to_owned(),
    ];
    Terminal::spawn_with_capture(
        JobConfig {
            command: &argv,
            cwd: &std::env::current_dir()?,
            env: &HashMap::new(),
            id: "desktop",
            size: Some(size),
            live: false,
            output_event: Some(Arc::new(move |_, bytes| output(bytes))),
        },
        None,
    )
}

pub async fn exit_status(terminal: &Terminal) -> std::io::Result<(Option<i32>, Option<i32>)> {
    terminal.wait_exit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_form_accepts_the_same_account_targets_as_the_cli() {
        for text in [
            "Alice_2@ssmcp.lmm.best",
            "Alice@[::1]:9443",
            "alice@http://127.0.0.1:5000",
        ] {
            assert!(validate_destination(text).is_ok(), "{text}");
        }
        for text in [
            "",
            "alice",
            "al@host",
            "admin@host",
            "alice@",
            "alice@host/path",
            "alice@@host",
            "alice@host --worker 5",
            "alice@host\n",
        ] {
            assert!(validate_destination(text).is_err(), "{text}");
        }
    }
}

//! Queue explicit local compatibility commands; never bind a key or run under the frame lock.
use super::State;
use crate::client::interface::computer::Action;

impl State {
    pub(super) fn computer_command(&mut self, arguments: &str) -> Result<(), String> {
        let action = Action::parse(arguments)?;
        if self.preview {
            return Err(
                "旧版 SSMCP 兼容开关：预览不读写兼容状态 · 不控制独立 Zuse CLI".into(),
            );
        }
        self.actions.push(action.queued());
        self.notice =
            Some("旧版 SSMCP 兼容开关：请求已排队 · 不控制独立 Zuse CLI".into());
        Ok(())
    }
}

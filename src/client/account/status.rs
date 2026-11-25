//! Device role and the owner selected for this connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) enum Status {
    Local,
    Device { owner: Option<String> },
}

impl Status {
    pub(in crate::client) fn compact_rows(&self) -> [String; 2] {
        match self {
            Self::Local => ["本地会话 当前系统用户".into(), "/logs 查看 MCP 授权".into()],
            Self::Device { owner } => [
                "Controlled device".into(),
                owner.as_ref().map_or_else(
                    || "/logs: MCP access".into(),
                    |owner| format!("Owner {owner}"),
                ),
            ],
        }
    }

    pub(in crate::client) fn rows(&self) -> [String; 3] {
        if *self == Self::Local {
            return [
                " 本地会话  当前系统用户".into(),
                " 授权  /logs 查看 MCP 地址和一次性登录码".into(),
                String::new(),
            ];
        }
        let [role, owner] = self.compact_rows();
        [format!(" {role}"), format!(" {owner}"), String::new()]
    }
}

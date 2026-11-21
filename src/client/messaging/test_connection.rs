//! An isolated messaging manager for terminal tests that inject service receipts.
use super::*;

pub(in crate::client) struct Connection {
    manager: Manager,
    requests: mpsc::Receiver<Request>,
}

impl Connection {
    pub(in crate::client) fn new(recipients: Vec<Recipient>) -> Self {
        let mut manager = Manager::default();
        let mut requests = manager.connect(true);
        let list = requests.try_recv().unwrap();
        manager.reply(
            list.id,
            Ok(Reply {
                recipients,
                ..Reply::default()
            }),
        );
        Self { manager, requests }
    }

    pub(in crate::client) fn snapshot(&self) -> Value {
        self.manager.snapshot()
    }

    pub(in crate::client) fn lookup_directory(&mut self) -> Result<DirectoryLookup, String> {
        self.manager.lookup_directory()
    }

    pub(in crate::client) fn confirm_directory_miss(
        &mut self,
        lookup: &DirectoryLookup,
    ) -> Result<Option<DirectoryLookup>, String> {
        self.manager.confirm_directory_miss(lookup)
    }

    pub(in crate::client) fn quiet_refresh(&mut self) {
        self.manager.refresh_with_visibility(true).unwrap();
    }

    pub(in crate::client) fn directory_result(
        &self,
        lookup: &mut DirectoryLookup,
    ) -> Option<Result<Vec<Recipient>, String>> {
        self.manager.directory_result(lookup)
    }

    pub(in crate::client) fn next_request(&mut self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    pub(in crate::client) fn reply(&mut self, id: u64, result: Result<Reply, String>) {
        self.manager.reply(id, result);
    }

    pub(in crate::client) fn send(
        &mut self,
        id: &str,
        account: &(String, String),
        text: &str,
        request_id: &str,
    ) -> Result<(), String> {
        self.manager.send_to_account(id, account, text, request_id)
    }

    pub(in crate::client) fn send_group(
        &mut self,
        recipients: &[Recipient],
        text: &str,
        request_id: &str,
    ) -> Result<(), String> {
        self.manager.group_send(recipients, text, request_id)
    }

    pub(in crate::client) fn retry_group(&mut self, request_id: &str) -> Result<(), String> {
        self.manager.group_retry(request_id)
    }
}

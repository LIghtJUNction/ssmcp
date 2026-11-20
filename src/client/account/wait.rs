//! Wait for correlated replies; a queued or delivered input is never a result.
use super::*;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_REQUESTS: usize = 32;
const MAX_STDIN: usize = 2 * 1024 * 1024;
const HELP: &str = "Usage: ssmcp wait [REQUEST_ID ...] [--stdin] [--server URL]\n\
                  [--any | --all | --count N] [--timeout SECONDS] [--json]\n\n\
Wait for actual replies to previously sent chat inputs. The default is --all.\n\
--any / --any-of returns after one reply; --all / --gather collects all replies;\n\
--count N returns after N distinct input results. One thread can answer more\n\
than one request; each input counts once. Errors are replies, but\n\
delivery acknowledgements and progress are not. Pending work is never cancelled.\n\
Read receipt JSON, JSONL, or request IDs from piped stdin or --stdin. Receipts\n\
must match the selected server and signed-in account; stdin never selects a server.\n\
stdout contains reply text, or one result JSON object per line with --json.\n\
A timeout or failed reply produces a nonzero exit status; already printed replies\n\
remain available to the next command in a pipe.\n\n\
--timeout bounds reply polling after stdin reaches EOF. It never cancels work.\n\
Receipts are retained in bounded server memory for at most 1 hour; restarting\n\
the relay loses pending inputs and retained replies. Client and relay need chat I/O support.\n\n\
Example: ssmcp send process 2 'Review the patch' | ssmcp wait --count 2 --timeout 120";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    Any,
    #[default]
    All,
    Count(usize),
}

#[derive(Default)]
struct Options {
    mode: Mode,
    server: Option<String>,
    timeout: Option<Duration>,
    json: bool,
    stdin: bool,
    ids: Vec<String>,
    help: bool,
}

fn identifier(value: &str) -> Result<()> {
    crate::wire::messages::validate_request_id(value).map_err(Into::into)
}

fn parse(args: &[OsString]) -> Result<Options> {
    let mut options = Options::default();
    let mut mode_set = false;
    let mut literal = false;
    let mut args = args.iter();
    if args.next().is_none_or(|arg| arg != "wait") {
        return Err("wait command is missing".into());
    }
    while let Some(arg) = args.next() {
        let arg = arg.to_str().ok_or("wait arguments must be UTF-8")?;
        let mut value = || -> Result<&str> {
            args.next()
                .ok_or("wait option requires a value")?
                .to_str()
                .ok_or_else(|| "wait arguments must be UTF-8".into())
        };
        let mode = match arg {
            "--" if !literal => {
                literal = true;
                continue;
            }
            "--help" | "-h" if !literal => {
                options.help = true;
                continue;
            }
            "--server" if !literal => {
                if options.server.is_some() {
                    return Err("--server may be supplied once".into());
                }
                options.server = Some(normalize_server(value()?)?);
                continue;
            }
            "--stdin" if !literal => {
                options.stdin = true;
                continue;
            }
            "--json" if !literal => {
                options.json = true;
                continue;
            }
            "--timeout" if !literal => {
                if options.timeout.is_some() {
                    return Err("--timeout may be supplied once".into());
                }
                let seconds: f64 = value()?
                    .parse()
                    .map_err(|_| "timeout must be a number of seconds")?;
                if !seconds.is_finite() || !(0.001..=86400.0).contains(&seconds) {
                    return Err("timeout must be between 0.001 and 86400 seconds".into());
                }
                options.timeout = Some(Duration::from_secs_f64(seconds));
                continue;
            }
            "--any" | "--any-of" | "--anyof" | "anyof" if !literal => Mode::Any,
            "--all" | "--gather" | "gather" if !literal => Mode::All,
            "--count" | "--n-of" if !literal => {
                let count = value()?
                    .parse::<usize>()
                    .map_err(|_| "--count needs a positive integer")?;
                if !(1..=MAX_REQUESTS * 32).contains(&count) {
                    return Err("--count must be between 1 and 1024".into());
                }
                Mode::Count(count)
            }
            _ if !literal && arg.starts_with('-') => {
                return Err("unknown wait option; see ssmcp wait --help".into());
            }
            _ => {
                identifier(arg)?;
                options.ids.push(arg.to_owned());
                continue;
            }
        };
        if mode_set {
            return Err("choose one wait mode: --any, --all, or --count N".into());
        }
        mode_set = true;
        options.mode = mode;
    }
    Ok(options)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct Account {
    provider: String,
    subject: String,
}

fn verify_account(account: &Account, credential: &Credential) -> Result<()> {
    if account.provider != "github"
        || account.subject.is_empty()
        || account.subject.len() > 64
        || !account.subject.bytes().all(|b| b.is_ascii_digit())
        || credential
            .subject
            .as_ref()
            .is_some_and(|subject| subject != &account.subject)
    {
        return Err("chat receipt belongs to a different or invalid account".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct Target {
    recipient_id: String,
    client_id: String,
    #[serde(default)]
    thread_id: Option<String>,
    input_id: String,
}

fn targets(value: &Value) -> Result<BTreeMap<String, Target>> {
    let rows = value.as_array().ok_or("chat receipt has no recipients")?;
    if !(1..=32).contains(&rows.len()) {
        return Err("chat receipt must have 1..=32 recipients".into());
    }
    let mut result = BTreeMap::new();
    let mut recipient_ids = BTreeSet::new();
    for row in rows {
        let target: Target = serde_json::from_value(row.clone())?;
        let expected = match &target.thread_id {
            Some(thread) => format!("agent:{}:{thread}", target.client_id),
            None => format!("client:{}", target.client_id),
        };
        if expected != target.recipient_id {
            return Err("chat receipt recipient identity does not match its client/thread".into());
        }
        crate::wire::messages::parse_recipient(&expected)?;
        let suffix = target
            .input_id
            .strip_prefix("input_")
            .ok_or("invalid chat input identifier")?;
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid chat input identifier".into());
        }
        if !recipient_ids.insert(target.recipient_id.clone())
            || result.insert(target.input_id.clone(), target).is_some()
        {
            return Err("chat receipt contains duplicate recipients or inputs".into());
        }
    }
    Ok(result)
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct Output {
    seq: u64,
    output_id: String,
    request_id: String,
    input_id: String,
    recipient_id: String,
    client_id: String,
    #[serde(default)]
    thread_id: Option<String>,
    kind: String,
    state: String,
    text: String,
    created_at_ms: u64,
}

struct Request {
    id: String,
    account: Option<Account>,
    targets: Option<BTreeMap<String, Target>>,
    completed: BTreeSet<String>,
    expired: BTreeSet<String>,
    after: u64,
    has_more: bool,
    failures: usize,
}

impl Request {
    fn new(id: String) -> Result<Self> {
        identifier(&id)?;
        Ok(Self {
            id,
            account: None,
            targets: None,
            completed: BTreeSet::new(),
            expired: BTreeSet::new(),
            after: 0,
            has_more: false,
            failures: 0,
        })
    }

    fn receipt(value: &Value, credential: &Credential) -> Result<Self> {
        if value["schema_version"] != 1 {
            return Err("unsupported chat receipt schema".into());
        }
        let origin = value["origin"]
            .as_str()
            .ok_or("piped receipt has no server origin; use its request ID explicitly")?;
        if normalize_server(origin)? != credential.server {
            return Err(
                "piped receipt belongs to a different server; select that server with --server"
                    .into(),
            );
        }
        let account: Account = serde_json::from_value(value["account"].clone())?;
        verify_account(&account, credential)?;
        if value["account"]["principal"]
            .as_str()
            .is_some_and(|principal| principal != credential.principal)
        {
            return Err("piped receipt belongs to a different saved principal".into());
        }
        let mut request = Self::new(
            value["request_id"]
                .as_str()
                .ok_or("receipt has no request_id")?
                .into(),
        )?;
        request.account = Some(account);
        request.targets = Some(targets(&value["recipients"])?);
        // Piped output is a hint only; real authenticated polling supplies replies.
        Ok(request)
    }

    fn consume(&mut self, value: Value, credential: &Credential) -> Result<Vec<Output>> {
        if value["schema_version"] != 1 || value["request_id"].as_str() != Some(self.id.as_str()) {
            return Err("server returned a different chat request or unsupported schema".into());
        }
        let account: Account = serde_json::from_value(value["account"].clone())?;
        verify_account(&account, credential)?;
        if self
            .account
            .as_ref()
            .is_some_and(|expected| expected != &account)
        {
            return Err("server chat reply account differs from the original receipt".into());
        }
        let current = targets(&value["recipients"])?;
        if self
            .targets
            .as_ref()
            .is_some_and(|expected| expected != &current)
        {
            return Err("chat request recipients changed after send".into());
        }
        let after = value["next_after"]
            .as_u64()
            .ok_or("chat reply has no sequence cursor")?;
        if after < self.after {
            return Err("chat reply cursor moved backwards".into());
        }
        let has_more = value["has_more"]
            .as_bool()
            .ok_or("chat reply has no pagination status")?;
        let rows = value["outputs"]
            .as_array()
            .ok_or("chat reply has no outputs")?;
        if rows.len() > 64 {
            return Err("chat reply exceeds its page limit".into());
        }
        let mut previous = self.after;
        let mut replies = Vec::new();
        for row in rows {
            let output: Output = serde_json::from_value(row.clone())?;
            let target = current
                .get(&output.input_id)
                .ok_or("reply belongs to an unknown input")?;
            if output.request_id != self.id
                || output.recipient_id != target.recipient_id
                || output.client_id != target.client_id
                || output.thread_id != target.thread_id
            {
                return Err("reply identity does not match the frozen recipient".into());
            }
            if output.seq <= previous || output.seq > after {
                return Err("chat output sequence is repeated or outside its cursor".into());
            }
            previous = output.seq;
            let suffix = output
                .output_id
                .strip_prefix("output_")
                .ok_or("invalid chat output identifier")?;
            if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid chat output identifier".into());
            }
            if output.text.trim().is_empty()
                || output.text.len() > 16 * 1024
                || output
                    .text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return Err("invalid chat reply text".into());
            }
            match (output.kind.as_str(), output.state.as_str()) {
                ("progress", "running") => {}
                ("result", "done" | "error") => replies.push(output),
                _ => return Err("chat reply has an invalid result state".into()),
            }
        }
        if after != previous || (has_more && after == self.after) {
            return Err("chat cursor skipped unavailable outputs or made no progress".into());
        }
        let mut expired = BTreeSet::new();
        for row in value["recipients"]
            .as_array()
            .expect("validated recipients")
        {
            match row["state"].as_str() {
                Some("expired") => {
                    expired.insert(
                        row["input_id"]
                            .as_str()
                            .expect("validated input")
                            .to_owned(),
                    );
                }
                Some("queued" | "included" | "done" | "error") => {}
                _ => return Err("chat recipient has an invalid state".into()),
            }
        }
        self.account = Some(account);
        self.targets = Some(current);
        self.expired = expired;
        self.after = after;
        self.has_more = has_more;
        replies.retain(|output| self.completed.insert(output.input_id.clone()));
        self.failures += replies
            .iter()
            .filter(|output| output.state == "error")
            .count();
        if !has_more {
            for row in value["recipients"]
                .as_array()
                .expect("validated recipients")
            {
                if matches!(row["state"].as_str(), Some("done" | "error"))
                    && !self
                        .completed
                        .contains(row["input_id"].as_str().expect("validated input"))
                {
                    return Err(
                        "server reported completion without an actual retained reply".into(),
                    );
                }
            }
        }
        Ok(replies)
    }

    fn total(&self) -> usize {
        self.targets.as_ref().map_or(0, BTreeMap::len)
    }
    fn possible(&self) -> usize {
        self.total() - self.expired.difference(&self.completed).count()
    }
    fn pending(&self) -> bool {
        self.has_more || self.completed.len() < self.possible()
    }
}

fn stdin_values(text: &str) -> Result<Vec<Value>> {
    if text.len() > MAX_STDIN {
        return Err("wait stdin exceeds 2 MiB".into());
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("wait stdin is empty".into());
    }
    let values = if trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('"')
    {
        let mut values = Vec::new();
        for value in serde_json::Deserializer::from_str(trimmed).into_iter::<Value>() {
            match value? {
                Value::Array(items) => values.extend(items),
                value => values.push(value),
            }
            if values.len() > MAX_REQUESTS {
                return Err("wait accepts at most 32 request IDs or receipts".into());
            }
        }
        values
    } else {
        trimmed
            .split_whitespace()
            .take(MAX_REQUESTS + 1)
            .map(|id| Value::String(id.into()))
            .collect()
    };
    if values.is_empty() || values.len() > MAX_REQUESTS {
        return Err("wait accepts 1..=32 request IDs or receipts".into());
    }
    Ok(values)
}

fn requests(values: Vec<Value>, credential: &Credential) -> Result<Vec<Request>> {
    if values.is_empty() || values.len() > MAX_REQUESTS {
        return Err("wait requires 1..=32 request IDs or receipts".into());
    }
    let mut requests = BTreeMap::<String, Request>::new();
    for value in values {
        let request = match value {
            Value::String(id) => Request::new(id)?,
            Value::Object(_) => Request::receipt(&value, credential)?,
            _ => return Err("wait stdin must contain request IDs or receipt objects".into()),
        };
        if let Some(existing) = requests.get(&request.id) {
            if existing.account.is_some()
                && request.account.is_some()
                && existing.account != request.account
                || existing.targets.is_some()
                    && request.targets.is_some()
                    && existing.targets != request.targets
            {
                return Err("duplicate request ID has conflicting receipts".into());
            }
            if existing.targets.is_some() {
                continue;
            }
        }
        requests.insert(request.id.clone(), request);
    }
    Ok(requests.into_values().collect())
}

fn threshold(mode: Mode, requests: &[Request]) -> Result<usize> {
    let total: usize = requests.iter().map(Request::total).sum();
    let count = match mode {
        Mode::Any => 1,
        Mode::All => total,
        Mode::Count(count) => count,
    };
    if count == 0 || count > total {
        return Err(format!(
            "requested {count} replies, but these receipts contain {total} recipients"
        )
        .into());
    }
    Ok(count)
}

fn write_outputs(outputs: &[Output], json: bool) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for output in outputs {
        if json {
            serde_json::to_writer(&mut stdout, output)?;
            writeln!(stdout)?;
        } else {
            stdout.write_all(output.text.as_bytes())?;
            if !output.text.ends_with('\n') {
                writeln!(stdout)?;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

async fn collect(
    api: &Api,
    credential: &Credential,
    requests: &mut [Request],
    mode: Mode,
    json: bool,
) -> Result<()> {
    let mut initialized = false;
    let mut wanted = match mode {
        Mode::Any => Some(1),
        Mode::Count(count) => Some(count),
        Mode::All => None,
    };
    let mut printed = 0usize;
    let mut failures = 0usize;
    loop {
        let polls: Vec<_> = requests
            .iter()
            .enumerate()
            .filter(|(_, request)| !initialized || request.pending())
            .map(|(index, request)| {
                (
                    index,
                    request.id.clone(),
                    request.after,
                    if initialized && !request.has_more {
                        1000
                    } else {
                        0
                    },
                )
            })
            .collect();
        if polls.is_empty() {
            return Err("no pending replies remain; some recipients expired".into());
        }
        let mut pages = stream::iter(polls).map(|(index, id, after, wait_ms)| async move {
            let page = api.post("/api/client/io", Some(&credential.token),
                json!({"action":"poll","request_id":id,"after":after,"limit":64,"wait_ms":wait_ms})).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((index, page))
        }).buffer_unordered(8);
        while let Some(page) = pages.next().await {
            let (index, page) = page?;
            let replies = requests[index].consume(page, credential)?;
            let take = wanted.map_or(replies.len(), |count| {
                count.saturating_sub(printed).min(replies.len())
            });
            write_outputs(&replies[..take], json)?;
            printed += take;
            failures += replies[..take]
                .iter()
                .filter(|output| output.state == "error")
                .count();
            if wanted.is_some_and(|count| printed >= count) {
                return if failures == 0 {
                    Ok(())
                } else {
                    Err(format!("{failures} received replies reported an error; remaining work was left running").into())
                };
            }
        }
        initialized = true;
        let count = threshold(mode, requests)?;
        wanted = Some(count);
        let complete = printed;
        if complete >= count {
            return if failures == 0 {
                Ok(())
            } else {
                Err(format!(
                    "{failures} received replies reported an error; remaining work was left running"
                )
                .into())
            };
        }
        if requests.iter().map(Request::possible).sum::<usize>() < count {
            return Err(format!("cannot receive {count} replies: recipients expired or disconnected; {complete} actual replies were printed").into());
        }
    }
}

async fn wait(
    api: &Api,
    credential: &Credential,
    mut requests: Vec<Request>,
    options: &Options,
) -> Result<()> {
    eprintln!(
        "Waiting for actual replies to {} request(s); pending work will stay running.",
        requests.len()
    );
    if let Some(duration) = options.timeout {
        match tokio::time::timeout(
            duration,
            collect(api, credential, &mut requests, options.mode, options.json),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let count: usize = requests.iter().map(|r| r.completed.len()).sum();
                Err(format!("wait timed out after {:.3}s; {count} actual replies were printed and pending work was left running", duration.as_secs_f64()).into())
            }
        }
    } else {
        collect(api, credential, &mut requests, options.mode, options.json).await
    }
}

pub(super) async fn for_receipt(api: &Api, credential: &Credential, receipt: Value) -> Result<()> {
    wait(
        api,
        credential,
        requests(vec![receipt], credential)?,
        &Options::default(),
    )
    .await
}

pub(super) async fn for_receipt_with_json(
    api: &Api,
    credential: &Credential,
    receipt: Value,
    json: bool,
) -> Result<()> {
    let options = Options {
        json,
        ..Options::default()
    };
    wait(
        api,
        credential,
        requests(vec![receipt], credential)?,
        &options,
    )
    .await
}

pub(super) async fn run(args: &[OsString]) -> Result<()> {
    let options = parse(args)?;
    if options.help {
        println!("{HELP}");
        return Ok(());
    }
    let mut values: Vec<Value> = options.ids.iter().cloned().map(Value::String).collect();
    if options.stdin || (values.is_empty() && !io::stdin().is_terminal()) {
        let text = tokio::task::spawn_blocking(|| -> Result<String> {
            let mut bytes = Vec::new();
            io::stdin()
                .take(MAX_STDIN as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_STDIN {
                return Err("wait stdin exceeds 2 MiB".into());
            }
            Ok(String::from_utf8(bytes).map_err(|_| "wait stdin must be UTF-8")?)
        })
        .await??;
        values.extend(stdin_values(&text)?);
    }
    let credential = require_credential(options.server.as_deref()).await?;
    let api = Api::new(&credential.server)?;
    wait(&api, &credential, requests(values, &credential)?, &options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> Credential {
        Credential {
            server: "https://example.test".into(),
            token: "x".repeat(32),
            principal: "github:alice".into(),
            subject: Some("42".into()),
            username: None,
            expires_at: u64::MAX,
        }
    }
    fn page(state: &str, kind: Option<&str>, sequence: u64) -> Value {
        let client = "a".repeat(64);
        let input = format!("input_{}", "1".repeat(32));
        let mut value = json!({"schema_version":1,"request_id":"io_fixture","origin":"https://example.test",
            "account":{"provider":"github","subject":"42"},"recipients":[{"recipient_id":format!("client:{client}"),
            "client_id":client,"thread_id":null,"input_id":input,"state":state}],
            "outputs":[],"next_after":sequence,"has_more":false});
        if let Some(kind) = kind {
            value["outputs"] = json!([{"seq":sequence,"output_id":format!("output_{:032x}",sequence),"request_id":"io_fixture",
                "input_id":input,"recipient_id":format!("client:{client}"),"client_id":client,"thread_id":null,
                "kind":kind,"state":if kind=="progress" {"running"} else {state},"text":"actual reply","created_at_ms":1}]);
        }
        value
    }

    #[test]
    fn wait_ack_and_progress_are_not_completion_and_duplicate_input_counts_once() {
        let mut request = Request::new("io_fixture".into()).unwrap();
        assert!(
            request
                .consume(page("included", None, 0), &credential())
                .unwrap()
                .is_empty()
        );
        assert!(
            request
                .consume(page("included", Some("progress"), 1), &credential())
                .unwrap()
                .is_empty()
        );
        assert_eq!(request.completed.len(), 0);
        assert_eq!(
            request
                .consume(page("done", Some("result"), 2), &credential())
                .unwrap()
                .len(),
            1
        );
        assert!(
            request
                .consume(page("done", Some("result"), 3), &credential())
                .unwrap()
                .is_empty()
        );
        assert_eq!(request.completed.len(), 1);
    }

    #[test]
    fn wait_rejects_wrong_scope_recipient_and_ack_only_terminal_state() {
        for mutate in [0, 1, 2, 3] {
            let mut request = Request::new("io_fixture".into()).unwrap();
            let mut value = page("done", Some("result"), 1);
            match mutate {
                0 => value["account"]["subject"] = json!("99"),
                1 => value["outputs"][0]["thread_id"] = json!("agent_fake"),
                2 => value["request_id"] = json!("io_other"),
                _ => {
                    value["outputs"] = json!([]);
                    value["next_after"] = json!(0);
                }
            }
            assert!(request.consume(value, &credential()).is_err());
        }
        let mut value = page("queued", None, 0);
        value["origin"] = json!("https://attacker.test");
        assert!(Request::receipt(&value, &credential()).is_err());
    }

    #[test]
    fn wait_receipts_cannot_move_targets_and_do_not_trust_piped_results() {
        let value = page("done", Some("result"), 1);
        let mut request = Request::receipt(&value, &credential()).unwrap();
        assert_eq!(request.completed.len(), 0);
        let mut changed = page("queued", None, 0);
        changed["recipients"][0]["input_id"] = json!(format!("input_{}", "2".repeat(32)));
        assert!(request.consume(changed, &credential()).is_err());
        assert_eq!(request.consume(value, &credential()).unwrap().len(), 1);
    }

    #[test]
    fn wait_modes_limits_and_expired_capacity() {
        let mut first = Request::new("io_fixture".into()).unwrap();
        first
            .consume(page("included", None, 0), &credential())
            .unwrap();
        let mut second = Request::new("io_fixture".into()).unwrap();
        second
            .consume(page("expired", None, 0), &credential())
            .unwrap();
        let requests = vec![first, second];
        assert_eq!(threshold(Mode::Any, &requests).unwrap(), 1);
        assert_eq!(threshold(Mode::All, &requests).unwrap(), 2);
        assert_eq!(threshold(Mode::Count(2), &requests).unwrap(), 2);
        assert!(threshold(Mode::Count(3), &requests).is_err());
        assert_eq!(requests.iter().map(Request::possible).sum::<usize>(), 1);
        for args in [
            vec!["wait", "--count", "0"],
            vec!["wait", "--any", "--all"],
            vec!["wait", "--timeout", "NaN"],
            vec!["wait", "--timeout", "0"],
            vec!["wait", "--timeout", "86401"],
        ] {
            assert!(parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).is_err());
        }
        let args = [
            "wait",
            "--count",
            "2",
            "--timeout",
            "0.05",
            "--json",
            "io_fixture",
        ]
        .map(OsString::from);
        let options = parse(&args).unwrap();
        assert_eq!(options.mode, Mode::Count(2));
        assert_eq!(options.timeout, Some(Duration::from_millis(50)));
        assert!(options.json);
    }

    #[test]
    fn wait_jsonl_pipes_are_bounded_and_duplicate_receipts_do_not_add_recipients() {
        let receipt = page("queued", None, 0);
        let values = stdin_values(&format!("{receipt}\n{receipt}\n")).unwrap();
        assert_eq!(requests(values, &credential()).unwrap().len(), 1);
        assert_eq!(stdin_values("io_a\nio_b").unwrap().len(), 2);
        assert!(stdin_values(&"io_a ".repeat(MAX_REQUESTS + 1)).is_err());
        assert!(stdin_values("{broken json}").is_err());
    }
}

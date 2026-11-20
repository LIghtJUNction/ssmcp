//! Bounded asynchronous completion of explicit workspace and skill references.
use crate::{context::Context, runtime::Runtime, workspace::Workspace};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

const MAX_ITEMS: usize = 1024;
const MAX_BYTES: usize = 256 * 1024;
const MAX_CACHE: usize = 8;
const MAX_MATCHES: usize = 64;
const TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Files(String),
    Skills,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Query {
    pub start: usize,
    pub end: usize,
    pub prefix: String,
    pub skill: bool,
}
impl Query {
    fn key(&self) -> Result<Key, String> {
        if self.skill {
            return Ok(Key::Skills);
        }
        if self.prefix.starts_with('/')
            || self.prefix.split('/').any(|part| part == "..")
            || self.prefix.chars().any(char::is_control)
            || self.prefix.len() > 1024
        {
            return Err("Completion uses workspace-relative paths without '..'".into());
        }
        let prefix = self.prefix.trim_start_matches("./");
        let parent = prefix.rsplit_once('/').map_or(".", |(path, _)| path);
        Ok(Key::Files(
            if parent.is_empty() { "." } else { parent }.into(),
        ))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub name: String,
    pub description: String,
    pub replacement: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Matches {
    pub items: Vec<Candidate>,
    pub notice: Option<String>,
    pub loading: bool,
}
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    description: String,
    directory: bool,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct Listing {
    entries: Vec<Entry>,
    partial: bool,
}

/// Same boundaries, quotes, escapes and backticks as reference expansion.
pub(crate) fn query(text: &str, cursor: usize) -> Option<Query> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let command = text.trim_start();
    let mut at = match command.chars().next() {
        Some('/') => {
            // Only /send carries a reference-bearing body. Preserve the command,
            // its separator and all other prose when replacing a suggestion.
            let rest = command.strip_prefix("/send")?;
            let separator = rest.chars().next().filter(|ch| ch.is_whitespace())?;
            text.len() - rest.len() + separator.len_utf8()
        }
        Some('!') => return None,
        _ => 0,
    };
    let mut ticks = 0;
    while at < text.len() && at < cursor {
        let ch = text[at..].chars().next()?;
        if ch == '\\' {
            at += 1;
            if let Some(next) = text[at..].chars().next()
                && matches!(next, '@' | '$' | '\x60' | '\\')
            {
                at += next.len_utf8();
            }
            continue;
        }
        if ch == '\x60' {
            let count = text[at..]
                .bytes()
                .take_while(|byte| *byte == b'\x60')
                .count();
            if ticks == 0 {
                ticks = count;
            } else if ticks == count {
                ticks = 0;
            }
            at += count;
            continue;
        }
        let boundary = text[..at].chars().next_back().is_none_or(|previous| {
            previous.is_whitespace() || "([{>\"'（【「『，。；：！？、".contains(previous)
        });
        if ticks != 0 || !boundary || !matches!(ch, '@' | '$') {
            at += ch.len_utf8();
            continue;
        }
        let start = at;
        at += 1;
        let quote = text[at..]
            .chars()
            .next()
            .filter(|ch| matches!(ch, '\'' | '"'));
        if quote.is_some() {
            at += 1;
        }
        let mut prefix = String::new();
        let mut end = at;
        while at < text.len() {
            let next = text[at..].chars().next()?;
            if Some(next) == quote {
                end = at + next.len_utf8();
                at = end;
                break;
            }
            if quote.is_none()
                && (next.is_whitespace()
                    || "()[]{}<>,;!?（）【】「」『』，。；：！？、".contains(next))
            {
                end = at;
                break;
            }
            if next == '\\'
                && let Some(escaped) = text[at + 1..].chars().next()
                && (escaped.is_whitespace() || matches!(escaped, '\\' | '\'' | '"' | '@' | '$'))
            {
                at += 1 + escaped.len_utf8();
                if at <= cursor {
                    prefix.push(escaped);
                }
            } else {
                at += next.len_utf8();
                if at <= cursor {
                    prefix.push(next);
                }
            }
            end = at;
        }
        if cursor > start && cursor <= end {
            if ch == '$' && quote.is_none() && prefix.starts_with(|ch: char| ch.is_ascii_digit()) {
                return None;
            }
            return Some(Query {
                start,
                end,
                prefix,
                skill: ch == '$',
            });
        }
        if cursor == start + 1 {
            return Some(Query {
                start,
                end,
                prefix: String::new(),
                skill: ch == '$',
            });
        }
        if at == start + 1 {
            at += text[at..].chars().next().map_or(0, char::len_utf8);
        }
    }
    None
}

fn reference(name: &str, skill: bool, directory: bool) -> String {
    let marker = if skill { '$' } else { '@' };
    let quoted = (skill && name.starts_with(|ch: char| ch.is_ascii_digit()))
        || name.chars().any(|ch| {
            ch.is_whitespace() || "()[]{}<>,;!?（）【】「」『』，。；：！？、\\\"'\x60".contains(ch)
        });
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    if directory {
        if quoted {
            format!("{marker}\"{escaped}")
        } else {
            format!("{marker}{name}")
        }
    } else if quoted {
        format!("{marker}\"{escaped}\" ")
    } else {
        format!("{marker}{name} ")
    }
}
fn matches(query: &Query, listing: &Listing) -> Matches {
    let mut found = listing
        .entries
        .iter()
        .filter(|entry| {
            entry.name.starts_with(if query.skill {
                &query.prefix
            } else {
                query.prefix.trim_start_matches("./")
            })
        })
        .collect::<Vec<_>>();
    found.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.name.cmp(&b.name)));
    let more = found.len() > MAX_MATCHES || listing.partial;
    let items = found
        .into_iter()
        .take(MAX_MATCHES)
        .map(|entry| Candidate {
            name: format!("{}{}", if query.skill { '$' } else { '@' }, entry.name),
            description: entry.description.clone(),
            replacement: reference(&entry.name, query.skill, entry.directory),
        })
        .collect::<Vec<_>>();
    Matches {
        notice: if more {
            Some("Partial list · keep typing or enter a subdirectory".into())
        } else if items.is_empty() {
            Some(
                if query.skill {
                    "No matching skills · you can still send"
                } else {
                    "No matching files · you can still send"
                }
                .into(),
            )
        } else {
            None
        },
        items,
        loading: false,
    }
}
pub(crate) fn apply(
    text: &str,
    cursor: usize,
    expected: &Query,
    replacement: &str,
    limit: usize,
) -> Result<(String, usize), String> {
    if query(text, cursor).as_ref() != Some(expected) {
        return Err("The input changed; choose a current suggestion".into());
    }
    let size = text.len() - (expected.end - expected.start) + replacement.len();
    if size > limit {
        return Err("Draft limit reached · 16 KiB".into());
    }
    let mut value = String::with_capacity(size);
    value.push_str(&text[..expected.start]);
    value.push_str(replacement);
    value.push_str(&text[expected.end..]);
    Ok((value, expected.start + replacement.len()))
}

struct Cached {
    key: Key,
    time: Instant,
    result: Result<Arc<Listing>, String>,
}
#[derive(Default)]
struct Service {
    runtime: Weak<Runtime>,
    handle: Option<tokio::runtime::Handle>,
    generation: u64,
    cache: VecDeque<Cached>,
    pending: Option<Key>,
    running: bool,
}
static SERVICE: OnceLock<Mutex<Service>> = OnceLock::new();

pub(crate) fn bind(runtime: &Arc<Runtime>) {
    let mut service = SERVICE
        .get_or_init(|| Mutex::new(Service::default()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    service.runtime = Arc::downgrade(runtime);
    service.handle = tokio::runtime::Handle::try_current().ok();
    service.generation = service.generation.wrapping_add(1);
    service.cache.clear();
    service.pending = None;
    service.running = false;
}

/// Return immediately; one worker coalesces cache misses. Never hold the cache
/// lock while updating the terminal, whose input path also reads this cache.
pub(crate) fn lookup(query: &Query) -> Matches {
    let key = match query.key() {
        Ok(key) => key,
        Err(error) => {
            return Matches {
                notice: Some(error),
                ..Matches::default()
            };
        }
    };
    let Some(service) = SERVICE.get() else {
        return Matches {
            notice: Some("Connect a client to discover files and skills".into()),
            ..Matches::default()
        };
    };
    let mut service = service.lock().unwrap_or_else(|error| error.into_inner());
    let cached = service
        .cache
        .iter()
        .find(|cached| cached.key == key)
        .map(|cached| (cached.time.elapsed() < TTL, cached.result.clone()));
    if let Some((true, result)) = &cached {
        return match result {
            Ok(listing) => matches(query, listing),
            Err(error) => Matches {
                notice: Some(error.clone()),
                ..Matches::default()
            },
        };
    }
    let Some(handle) = service.handle.clone() else {
        return Matches {
            notice: Some("Reference discovery requires a live client runtime".into()),
            ..Matches::default()
        };
    };
    if service.runtime.strong_count() == 0 {
        return Matches {
            notice: Some("Reconnect before discovering references".into()),
            ..Matches::default()
        };
    }
    service.pending = Some(key);
    if !service.running {
        service.running = true;
        let generation = service.generation;
        handle.spawn(worker(generation));
    }
    if let Some((_, Ok(listing))) = cached {
        return matches(query, &listing);
    }
    Matches {
        notice: Some("Loading reference suggestions…".into()),
        loading: true,
        ..Matches::default()
    }
}
async fn worker(generation: u64) {
    loop {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let work = {
            let mut service = SERVICE
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if service.generation != generation {
                return;
            }
            match (service.pending.take(), service.runtime.upgrade()) {
                (Some(key), Some(runtime)) => Some((key, runtime)),
                _ => {
                    service.running = false;
                    None
                }
            }
        };
        let Some((key, runtime)) = work else {
            return;
        };
        let result =
            tokio::time::timeout(Duration::from_secs(3), runtime.reference_completions(&key))
                .await
                .map_err(|_| "Reference discovery timed out; keep the draft and retry".to_owned())
                .and_then(|value| value)
                .map(Arc::new);
        {
            let mut service = SERVICE
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if service.generation != generation {
                return;
            }
            service.cache.retain(|cached| cached.key != key);
            service.cache.push_back(Cached {
                key: key.clone(),
                time: Instant::now(),
                result,
            });
            while service.cache.len() > MAX_CACHE {
                service.cache.pop_front();
            }
            if service.pending.as_ref() == Some(&key) {
                service.pending = None;
            }
        }
        super::terminal::invalidate();
    }
}

/// A compact display label; full source paths remain in the reference manifest.
fn skill_origin(item: &Value) -> String {
    let scope = item["scope"].as_str().unwrap_or("skill");
    let path = item["path"].as_str().unwrap_or("SKILL.md");
    let source = if scope == "builtin" {
        format!("embedded {path}")
    } else if scope == "project" {
        path.to_owned()
    } else {
        let root = std::path::Path::new(item["root"]["path"].as_str().unwrap_or(""));
        let tail = root
            .components()
            .rev()
            .take(2)
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let directory = tail.into_iter().rev().collect::<Vec<_>>().join("/");
        format!("{directory}/{path}")
    };
    let source = source
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(96)
        .collect::<String>();
    format!("{scope} · {source}")
}

/// Enumerate metadata only through the same rooted capabilities as submission.
pub(crate) async fn enumerate(
    workspace: &Workspace,
    context: &Context,
    key: &Key,
) -> Result<Listing, String> {
    let mut listing = Listing::default();
    let mut cursor: Option<String> = None;
    let mut cursors = BTreeSet::new();
    let mut bytes = 0;
    for _ in 0..16 {
        let page = match key {
            Key::Files(path) => {
                let mut args = json!({"path":path,"max_entries":128});
                if let Some(cursor) = &cursor {
                    args["cursor"] = json!(cursor);
                }
                workspace.perform("ssmcp.fs.list", args).await?
            }
            Key::Skills => {
                let mut args = json!({"scope":"effective","limit":64});
                if let Some(cursor) = &cursor {
                    args["cursor"] = json!(cursor);
                }
                context.local_skill_index(args).await?
            }
        };
        if matches!(key, Key::Skills) && page["truncated"] == true {
            return Err(
                "The configured skill index is incomplete; narrow the skill directories".into(),
            );
        }
        listing.partial |= page["truncated"] == true;
        let field = if matches!(key, Key::Skills) {
            "skills"
        } else {
            "entries"
        };
        for item in page[field]
            .as_array()
            .ok_or("Reference discovery returned no entries")?
        {
            let Some(name) = item["name"]
                .as_str()
                .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
            else {
                continue;
            };
            let entry = match key {
                Key::Files(path) => {
                    let directory = item["kind"] == "directory";
                    if !directory && item["kind"] != "file" {
                        continue;
                    }
                    let prefix = if path == "." {
                        String::new()
                    } else {
                        format!("{path}/")
                    };
                    Entry {
                        name: format!("{prefix}{name}{}", if directory { "/" } else { "" }),
                        description: if directory {
                            "Directory · continue".into()
                        } else {
                            format!("File · {} bytes", item["size"].as_u64().unwrap_or(0))
                        },
                        directory,
                    }
                }
                Key::Skills => {
                    if item["metadata_status"] != "ok" {
                        continue;
                    }
                    let origin = skill_origin(item);
                    let description = item["description"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .map(|ch| if ch.is_control() { ' ' } else { ch })
                        .collect::<String>();
                    Entry {
                        name: name.into(),
                        description: format!("{origin} · {description}"),
                        directory: false,
                    }
                }
            };
            bytes += entry.name.len() + entry.description.len();
            if listing.entries.len() >= MAX_ITEMS || bytes > MAX_BYTES {
                listing.partial = true;
                return Ok(listing);
            }
            listing.entries.push(entry);
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        match &cursor {
            None => return Ok(listing),
            Some(cursor) if !cursors.insert(cursor.clone()) => {
                return Err("Reference discovery repeated a page; retry".into());
            }
            _ => {}
        }
    }
    listing.partial = true;
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_query_respects_unicode_quotes_code_and_escapes() {
        let text = "检查 @\"docs/设计 文稿.md\" 然后继续";
        let q = query(text, text.find("文稿").unwrap()).unwrap();
        assert_eq!(q.prefix, "docs/设计 ");
        assert_eq!(&text[q.start..q.end], "@\"docs/设计 文稿.md\"");
        assert_eq!(query("@", 1).unwrap().prefix, "");
        assert!(query(r"\@secret", 8).is_none());
        assert!(query("\x60@secret\x60", 8).is_none());
        assert!(query("mail@example", 12).is_none());
        assert!(query("!echo $HOME", 11).is_none());
        assert!(query("$100", 4).is_none());
    }
    #[test]
    fn send_body_completes_references_without_changing_the_command() {
        for prefix in [
            "/send ",
            "  /send ",
            "\t/send\t",
            "/send\n    ",
            "\u{3000}/send\u{2003}",
        ] {
            let text = format!(r#"{prefix}看 @"资料/草稿.md" 然后继续"#);
            let cursor = text.find("草稿").unwrap();
            let found = query(&text, cursor).unwrap();
            assert_eq!(found.prefix, "资料/");
            assert_eq!(&text[found.start..found.end], r#"@"资料/草稿.md""#);
            let (updated, cursor) =
                apply(&text, cursor, &found, r#"@"资料/说明.md" "#, 1024).unwrap();
            assert_eq!(updated, format!(r#"{prefix}看 @"资料/说明.md"  然后继续"#));
            assert!(updated.is_char_boundary(cursor));

            let text = format!("{prefix}$review");
            let found = query(&text, text.len()).unwrap();
            assert!(found.skill);
            assert_eq!(found.prefix, "review");
            assert_eq!(found.start, prefix.len());
        }
    }

    #[test]
    fn send_completion_rejects_commands_shell_currency_and_code() {
        for text in [
            "/send",
            "/sendx @file",
            "/sender $skill",
            "/help @file",
            "/attach @file",
            "/message $skill",
            "!echo $HOME",
            "/send $100",
            r"/send \@escaped",
            "/send \x60@code\x60",
        ] {
            assert!(query(text, text.len()).is_none(), "{text}");
        }
        // A /send body is literal message prose, including a leading '!'.
        let prose = "/send !echo $HOME";
        assert!(query(prose, prose.len()).is_some());
        let text = "  /send @file";
        for cursor in 0..=8 {
            assert!(query(text, cursor).is_none(), "{cursor}");
        }
        assert_eq!(query("/send @", 7).unwrap().prefix, "");
        assert!(query("/send $", 7).unwrap().skill);
    }

    #[test]
    fn quoted_numeric_skills_remain_editable_without_matching_currency() {
        for text in [r#"$"007-helper""#, "$'007-helper'"] {
            let found = query(text, text.len()).unwrap();
            assert!(found.skill);
            assert_eq!(found.prefix, "007-helper");
            assert_eq!(&text[found.start..found.end], text);
            let replacement = reference("007-review", true, false);
            let (updated, cursor) = apply(text, text.len(), &found, &replacement, 1024).unwrap();
            assert_eq!(updated, "$\"007-review\" ");
            assert_eq!(cursor, updated.len());
        }
        for text in ["$100", "$3.50", "$007-helper"] {
            assert!(query(text, text.len()).is_none());
        }
    }

    #[test]
    fn chinese_punctuation_keeps_skill_and_file_completion_local() {
        let text = "按（$lint），检查@src/main.rs。";
        let cursor = text.find('）').unwrap();
        let found = query(text, cursor).unwrap();
        assert_eq!(found.prefix, "lint");
        assert_eq!(&text[found.start..found.end], "$lint");
        let (updated, _) = apply(text, cursor, &found, "$lint-check ", 1024).unwrap();
        assert_eq!(updated, "按（$lint-check ），检查@src/main.rs。");
        assert!(query(text, cursor + '）'.len_utf8()).is_none());

        let text = "检查 @src/main.rs，再按：$lint-check。";
        let file_cursor = text.find('，').unwrap();
        let file = query(text, file_cursor).unwrap();
        assert!(!file.skill);
        assert_eq!(file.prefix, "src/main.rs");
        let skill_cursor = text.find('。').unwrap();
        let skill = query(text, skill_cursor).unwrap();
        assert!(skill.skill);
        assert_eq!(skill.prefix, "lint-check");
        assert!(query(text, text.len()).is_none());
    }

    #[test]
    fn quoted_names_preserve_chinese_punctuation_when_completed() {
        for (name, skill) in [("检查，发布", true), ("设计，说明.md", false)] {
            let replacement = reference(name, skill, false);
            assert!(replacement.contains('"'));
            let cursor = replacement.len() - 1;
            let found = query(&replacement, cursor).unwrap();
            assert_eq!(found.prefix, name);
            assert_eq!(found.skill, skill);
        }
    }

    #[test]
    fn replacement_is_local_and_rejects_stale_input_or_oversize() {
        let text = "看 @src/mai 然后测试";
        let cursor = text.find(" 然后").unwrap();
        let q = query(text, cursor).unwrap();
        let (out, at) = apply(text, cursor, &q, "@src/main.rs ", 1024).unwrap();
        assert_eq!(out, "看 @src/main.rs  然后测试");
        assert!(out.is_char_boundary(at));
        assert!(apply("看 @other", "看 @other".len(), &q, "@x ", 1024).is_err());
        assert!(apply(text, cursor, &q, "@longer.rs ", 5).is_err());
    }
    #[test]
    fn quoted_directories_continue_paths_and_matches_are_bounded() {
        assert_eq!(reference("设计 文档/", false, true), "@\"设计 文档/");
        assert_eq!(
            reference("设计 文档/说明.md", false, false),
            "@\"设计 文档/说明.md\" "
        );
        let q = query("@", 1).unwrap();
        let listing = Listing {
            entries: (0..100)
                .map(|i| Entry {
                    name: format!("file-{i}"),
                    description: "file".into(),
                    directory: false,
                })
                .collect(),
            partial: false,
        };
        let result = matches(&q, &listing);
        assert_eq!(result.items.len(), MAX_MATCHES);
        assert!(result.notice.is_some());
        assert!(query("@../", 4).unwrap().key().is_err());
        assert!(query("@/tmp/", 6).unwrap().key().is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn rooted_enumeration_keeps_project_skill_precedence_and_skips_links() {
        use std::{fs, os::unix::fs::symlink, time::SystemTime};
        struct Temporary(std::path::PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = Temporary(std::env::temp_dir().join(format!(
                "ssmcp-completion-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        let project = dir.0.join("project");
        let global = dir.0.join("global");
        for root in [&project, &global] {
            fs::create_dir_all(root.join(".agents/skills/demo")).unwrap();
        }
        fs::write(
            project.join(".agents/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: project choice\n---\nProject instructions",
        )
        .unwrap();
        fs::write(
            global.join(".agents/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: global fallback\n---\nGlobal instructions",
        )
        .unwrap();
        fs::write(project.join("设计 说明.txt"), "fixture").unwrap();
        symlink(&global, project.join("outside")).unwrap();
        let workspace = Workspace::new(project.clone()).unwrap();
        let context = Context::new(project, vec![global]).unwrap();
        let files = enumerate(&workspace, &context, &Key::Files(".".into()))
            .await
            .unwrap();
        assert!(
            files
                .entries
                .iter()
                .any(|entry| entry.name == "设计 说明.txt")
        );
        assert!(
            !files
                .entries
                .iter()
                .any(|entry| entry.name.starts_with("outside"))
        );
        assert!(
            enumerate(&workspace, &context, &Key::Files("../".into()))
                .await
                .is_err()
        );
        let skills = enumerate(&workspace, &context, &Key::Skills).await.unwrap();
        let demo = skills
            .entries
            .iter()
            .filter(|entry| entry.name == "demo")
            .collect::<Vec<_>>();
        assert_eq!(demo.len(), 1);
        assert!(demo[0].description.contains("project choice"));
    }
}

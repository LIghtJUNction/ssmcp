//! Explicit local file and skill references carried by the existing text queue.
//! Reads use the same rooted capabilities as agents; no implicit home-directory access.
pub(crate) mod files;
pub(crate) mod paste;

use crate::{context::Context, wire::MAX_USER_MESSAGE_BYTES, workspace::Workspace};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

const MAX_REFERENCES: usize = 8;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const PREVIEW_BYTES: usize = 1024;
const SKILL_BYTES: usize = 256 * 1024;
const MAX_SKILL_PAGES: usize = 16;
const BEGIN: &str = "\n\n<ssmcp-references-v1>\n";
const END: &str = "\n</ssmcp-references-v1>";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reference {
    File(String),
    Skill(String),
}

/// Recover only a validated generated suffix, leaving arbitrary user text intact.
/// Queue editors can show this source while retaining the frozen transport payload.
pub(crate) fn source(text: &str) -> &str {
    if text.len() > MAX_USER_MESSAGE_BYTES {
        return text;
    }
    let Some((original, encoded)) = text.strip_suffix(END).and_then(|s| s.rsplit_once(BEGIN))
    else {
        return text;
    };
    let Ok(value) = serde_json::from_str::<Value>(encoded) else {
        return text;
    };
    if value["schema_version"] == 1
        && value["source_sha256"] == digest(original.as_bytes())
        && value["references"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    {
        original
    } else {
        text
    }
}

/// Prepare one immutable queue payload. Reusing a generated payload never rereads
/// files: a retry of the same command must not acquire different contents.
pub(crate) async fn expand(
    text: &str,
    workspace: &Workspace,
    context: &Context,
) -> Result<String, String> {
    if text.len() > MAX_USER_MESSAGE_BYTES {
        return Err("The request exceeds the 16 KiB message limit.".into());
    }
    if source(text).len() != text.len() {
        return Ok(text.into());
    }
    let references = parse(text)?;
    if references.is_empty() {
        return Ok(text.into());
    }
    let names = references
        .iter()
        .filter_map(|reference| match reference {
            Reference::Skill(name) => Some(name.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let skills = if names.is_empty() {
        BTreeMap::new()
    } else {
        discover_skills(context, &names).await?
    };
    let mut expanded = Vec::new();
    let mut files = BTreeSet::new();
    for reference in references {
        let value = match reference {
            Reference::File(path) => {
                let path = relative_path(&path, workspace.root())?;
                if !files.insert(path.clone()) {
                    continue;
                }
                file_reference(workspace, &path).await?
            }
            Reference::Skill(name) => {
                let entry = skills
                    .get(&name)
                    .ok_or("The selected skill disappeared; retry.")?;
                skill_reference(context, &name, entry).await?
            }
        };
        expanded.push(value);
    }
    let payload = json!({
        "schema_version":1,
        "source_sha256":digest(text.as_bytes()),
        "references":expanded,
        "usage":"These are explicit references to local files, not uploaded copies. Read them with the delivering client's ID. Previews may be partial; read a selected skill in full before applying it."
    });
    let result = format!("{text}{BEGIN}{payload}{END}");
    if result.len() > MAX_USER_MESSAGE_BYTES {
        return Err("The request and reference previews exceed 16 KiB; use fewer references or shorten the request. Your draft is unchanged.".into());
    }
    Ok(result)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn parse(text: &str) -> Result<Vec<Reference>, String> {
    let mut references = Vec::new();
    let mut seen = BTreeSet::new();
    let mut at = 0;
    let mut code_ticks = 0;
    while at < text.len() {
        let ch = text[at..].chars().next().unwrap();
        if ch == '\\' {
            at += ch.len_utf8();
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
            if code_ticks == 0 {
                code_ticks = count;
            } else if code_ticks == count {
                code_ticks = 0;
            }
            at += count;
            continue;
        }
        let boundary = text[..at]
            .chars()
            .next_back()
            .is_none_or(|previous| previous.is_whitespace() || "([{>\"'".contains(previous));
        if code_ticks == 0 && boundary && matches!(ch, '@' | '$') {
            let start = at + 1;
            let next = text[start..].chars().next();
            if next.is_none_or(|next| next.is_whitespace())
                || ch == '$' && next.is_some_and(|next| next.is_ascii_digit())
            {
                at = start;
                continue;
            }
            let (name, end) = token(text, start)?;
            at = end;
            if name.is_empty() {
                continue;
            }
            if name.chars().any(char::is_control) || name.len() > 4096 {
                return Err(
                    "A reference must be a bounded path or skill name without control characters."
                        .into(),
                );
            }
            let reference = if ch == '@' {
                Reference::File(name)
            } else {
                Reference::Skill(name)
            };
            if seen.insert(reference.clone()) {
                if references.len() == MAX_REFERENCES {
                    return Err(format!(
                        "Use at most {MAX_REFERENCES} file and skill references in one request."
                    ));
                }
                references.push(reference);
            }
            continue;
        }
        at += ch.len_utf8();
    }
    Ok(references)
}

fn token(text: &str, start: usize) -> Result<(String, usize), String> {
    let mut at = start;
    let quote = text[at..]
        .chars()
        .next()
        .filter(|ch| matches!(ch, '\'' | '"'));
    if quote.is_some() {
        at += 1;
    }
    let mut value = String::new();
    while at < text.len() {
        let ch = text[at..].chars().next().unwrap();
        if Some(ch) == quote {
            return Ok((value, at + ch.len_utf8()));
        }
        if quote.is_none() && (ch.is_whitespace() || "()[]{}<>,;!?".contains(ch)) {
            return Ok((value, at));
        }
        if ch == '\\'
            && let Some(next) = text[at + 1..].chars().next()
            && (next.is_whitespace() || matches!(next, '\\' | '\'' | '"' | '@' | '$'))
        {
            value.push(next);
            at += 1 + next.len_utf8();
            continue;
        }
        value.push(ch);
        at += ch.len_utf8();
    }
    if quote.is_some() {
        Err("The quoted file or skill reference is missing its closing quote.".into())
    } else {
        Ok((value, at))
    }
}

pub(crate) fn relative_path(value: &str, root: &Path) -> Result<String, String> {
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| format!("@{value}: the file is outside this session's workspace; import it before attaching."))?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                parts.push(part.to_str().ok_or("File reference paths must be UTF-8.")?)
            }
            _ => {
                return Err(format!(
                    "@{value}: parent traversal and external paths are not allowed."
                ));
            }
        }
    }
    if parts.is_empty() || parts[0] == "~" || value.contains("://") {
        return Err(format!(
            "@{value}: choose a file inside the session workspace."
        ));
    }
    Ok(parts.join("/"))
}

async fn file_reference(workspace: &Workspace, path: &str) -> Result<Value, String> {
    let fail = |error: String| format!("@{path}: {error}");
    let metadata = workspace
        .perform("ssmcp.fs.stat", json!({"path":path}))
        .await
        .map_err(&fail)?;
    if metadata["kind"] != "file" {
        return Err(format!(
            "@{path}: choose a regular file, not a directory or special file."
        ));
    }
    let size = metadata["size"]
        .as_u64()
        .ok_or("File size is unavailable.")?;
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "@{path}: the file is {size} bytes; the reference limit is 64 MiB."
        ));
    }
    let read = workspace
        .perform(
            "ssmcp.fs.read",
            json!({"path":path,"offset":0,"max_bytes":PREVIEW_BYTES + 4}),
        )
        .await
        .map_err(&fail)?;
    if read["size"].as_u64() != Some(size) {
        return Err(format!(
            "@{path}: the file changed while preparing its reference; retry."
        ));
    }
    let bytes = STANDARD
        .decode(
            read["data_base64"]
                .as_str()
                .ok_or("File read returned no bytes.")?,
        )
        .map_err(|_| "File read returned invalid binary data.")?;
    if let Some(mime_type) = crate::output::image_mime_type(&bytes) {
        if size > crate::output::MAX_IMAGE_BYTES as u64 {
            return Err(format!(
                "@{path}: image exceeds 400 KiB ({} bytes); use a smaller image.",
                crate::output::MAX_IMAGE_BYTES
            ));
        }
        let arguments = json!({"path":path});
        return Ok(json!({
            "kind":"image","path":path,"mime_type":mime_type,"size":size,
            "validation":"header signature only; the image is not decoded during submission",
            "read":{"tool":"ssmcp.fs.read_image","arguments":arguments},
            "view":format!("const result = await call(clientId, \"ssmcp.fs.read_image\", {arguments}); image(result); return result.structuredContent;")
        }));
    }
    let binary_type = binary_mime(&bytes);
    if binary_type.is_none()
        && let Some(preview) = utf8_preview(&bytes, read["eof"] == true)
    {
        return Ok(json!({
            "kind":"text","path":path,"mime_type":"text/plain","size":size,
            "preview":preview,"preview_bytes":preview.len(),
            "preview_truncated":(preview.len() as u64) < size,
            "read":{"tool":"ssmcp.fs.read_text","arguments":{"path":path,"offset":0,"max_bytes":65536}},
            "continuation":"Follow next_offset until eof; offsets are UTF-8 byte positions."
        }));
    }
    Ok(json!({
        "kind":"binary","path":path,"mime_type":binary_type.unwrap_or("application/octet-stream"),"size":size,
        "read":{"tool":"ssmcp.fs.read","arguments":{"path":path,"offset":0,"max_bytes":65536}},
        "encoding":"base64",
        "continuation":"Follow next_offset until eof. Decode each data_base64 chunk; no binary contents were embedded in this message."
    }))
}

fn utf8_preview(bytes: &[u8], eof: bool) -> Option<&str> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if !eof && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return None;
    }
    let mut end = text.len().min(PREVIEW_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(&text[..end])
}

fn binary_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        Some("application/zip")
    } else if bytes.starts_with(b"\x1f\x8b") {
        Some("application/gzip")
    } else if bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"MZ") {
        Some("application/octet-stream")
    } else {
        None
    }
}

async fn discover_skills(
    context: &Context,
    names: &BTreeSet<String>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut found = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut cursors = BTreeSet::new();
    for _ in 0..MAX_SKILL_PAGES {
        let mut arguments = json!({"scope":"effective","limit":64});
        if let Some(cursor) = &cursor {
            arguments["cursor"] = json!(cursor);
        }
        let page = context.perform("ssmcp.skills.list", arguments).await?;
        if page["truncated"] == true {
            return Err("The configured skill index is incomplete; narrow the configured skill directories before selecting a skill.".into());
        }
        for skill in page["skills"]
            .as_array()
            .ok_or("Skill discovery returned no index.")?
        {
            if let Some(name) = skill["name"].as_str()
                && names.contains(name)
                && skill["metadata_status"] == "ok"
            {
                found.insert(name.into(), skill.clone());
            }
        }
        if found.len() == names.len() {
            return Ok(found);
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        match &cursor {
            None => {
                let missing = names
                    .iter()
                    .filter(|name| !found.contains_key(*name))
                    .cloned()
                    .collect::<Vec<_>>();
                return Err(format!(
                    "Skills not found in the configured directories: {}.",
                    missing.join(", ")
                ));
            }
            Some(cursor) if !cursors.insert(cursor.clone()) => {
                return Err(
                    "Skill discovery repeated a page; retry from the current index.".into(),
                );
            }
            _ => {}
        }
    }
    Err("Skill discovery exceeded its page limit; narrow the configured directories.".into())
}

async fn skill_reference(context: &Context, name: &str, entry: &Value) -> Result<Value, String> {
    let root = entry["root"]["id"]
        .as_str()
        .ok_or("The selected skill has no root.")?;
    let path = entry["path"]
        .as_str()
        .ok_or("The selected skill has no path.")?;
    let arguments = json!({"root":root,"path":path,"max_bytes":SKILL_BYTES});
    let document = context
        .perform("ssmcp.skills.read", arguments.clone())
        .await
        .map_err(|error| format!("${name}: {error}"))?;
    if document["metadata"]["name"] != name || document["metadata"]["status"] != "ok" {
        return Err(format!(
            "${name}: the skill metadata changed during discovery; retry."
        ));
    }
    let description = document["metadata"]["description"].as_str().unwrap_or("");
    let mut end = description.len().min(512);
    while !description.is_char_boundary(end) {
        end -= 1;
    }
    Ok(json!({
        "kind":"skill","name":name,"root":root,"path":path,
        "description":&description[..end],"description_truncated":end < description.len(),
        "size":document["bytes"],"sha256":document["sha256"],
        "read":{"tool":"ssmcp.skills.read","arguments":arguments},
        "usage":"Read the complete content field before applying this skill; no partial skill instructions are embedded."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-references-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
            let target = self.0.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, bytes).unwrap();
        }
        fn clients(&self) -> (Workspace, Context) {
            (
                Workspace::new(self.0.clone()).unwrap(),
                Context::new(self.0.clone(), vec![]).unwrap(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn payload(text: &str) -> Value {
        serde_json::from_str(
            text.strip_suffix(END)
                .unwrap()
                .rsplit_once(BEGIN)
                .unwrap()
                .1,
        )
        .unwrap()
    }

    #[test]
    fn references_support_quotes_unicode_and_literal_code() {
        assert_eq!(
            parse("看 @\"资料 文件.txt\" @'second file' @escaped\\ file $lint-check \\@literal user@example.com \x60$HOME @literal\x60 $20").unwrap(),
            vec![
                Reference::File("资料 文件.txt".into()),
                Reference::File("second file".into()),
                Reference::File("escaped file".into()),
                Reference::Skill("lint-check".into()),
            ]
        );
        assert!(parse("@\"unfinished").is_err());
        assert!(parse("@'line\nbreak'").is_err());
        assert_eq!(parse("@a @a $same $same").unwrap().len(), 2);
        assert!(parse(&(0..9).map(|i| format!("@file{i} ")).collect::<String>()).is_err());
    }

    #[tokio::test]
    async fn text_images_and_binary_files_have_real_native_read_actions() {
        let fixture = Fixture::new();
        fixture.write("资料 文件.txt", "这是中文草稿对应的文件。\n");
        fixture.write("image.png", b"\x89PNG\r\n\x1a\nfixture");
        fixture.write("archive.zip", b"PK\x03\x04contents");
        let (workspace, context) = fixture.clients();
        let draft = "看看 @\"资料 文件.txt\" @image.png @archive.zip";
        let expanded = expand(draft, &workspace, &context).await.unwrap();
        assert_eq!(source(&expanded), draft);
        let data = payload(&expanded);
        let refs = data["references"].as_array().unwrap();
        assert_eq!(refs[0]["kind"], "text");
        assert_eq!(refs[0]["preview"], "这是中文草稿对应的文件。\n");
        assert_eq!(refs[0]["read"]["tool"], "ssmcp.fs.read_text");
        assert_eq!(refs[1]["kind"], "image");
        assert_eq!(refs[1]["mime_type"], "image/png");
        assert_eq!(refs[1]["read"]["tool"], "ssmcp.fs.read_image");
        assert!(refs[1]["view"].as_str().unwrap().contains("image(result)"));
        assert_eq!(refs[2]["mime_type"], "application/zip");
        assert_eq!(refs[2]["read"]["tool"], "ssmcp.fs.read");
        assert!(!expanded.contains("\"data_base64\":"));
    }

    #[tokio::test]
    async fn retries_reuse_frozen_content_and_editing_recovers_exact_source() {
        let fixture = Fixture::new();
        fixture.write("notes.txt", "first");
        let (workspace, context) = fixture.clients();
        let draft = "保留原稿与位置 @notes.txt";
        let first = expand(draft, &workspace, &context).await.unwrap();
        fixture.write("notes.txt", "second");
        assert_eq!(expand(&first, &workspace, &context).await.unwrap(), first);
        let edited = expand(source(&first), &workspace, &context).await.unwrap();
        assert_eq!(payload(&edited)["references"][0]["preview"], "second");
        assert_eq!(source(&first), draft);
        let forged = first.replacen("保留原稿", "已经修改", 1);
        assert_eq!(source(&forged), forged);
    }

    #[tokio::test]
    async fn references_reject_missing_directories_external_paths_and_symlinks() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fixture.write("allowed.txt", "readable");
        outside.write("outside.txt", "private fixture");
        let (workspace, context) = fixture.clients();
        for draft in [
            "@missing.txt".to_owned(),
            "@.".into(),
            "@../outside.txt".into(),
            format!("@\"{}\"", outside.0.join("outside.txt").display()),
        ] {
            assert!(
                expand(&draft, &workspace, &context).await.is_err(),
                "{draft}"
            );
        }
        let absolute = format!("@\"{}\"", fixture.0.join("allowed.txt").display());
        assert_eq!(
            payload(&expand(&absolute, &workspace, &context).await.unwrap())["references"][0]["path"],
            "allowed.txt"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.0.join("outside.txt"), fixture.0.join("linked.txt"))
                .unwrap();
            assert!(expand("@linked.txt", &workspace, &context).await.is_err());
        }
    }

    #[tokio::test]
    async fn size_and_message_budgets_fail_without_truncating_the_draft() {
        let fixture = Fixture::new();
        fixture.write("oversized.png", b"\x89PNG\r\n\x1a\n");
        fs::OpenOptions::new()
            .write(true)
            .open(fixture.0.join("oversized.png"))
            .unwrap()
            .set_len(crate::output::MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        fixture.write("large.bin", []);
        fs::OpenOptions::new()
            .write(true)
            .open(fixture.0.join("large.bin"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        fixture.write("notes.txt", "x".repeat(PREVIEW_BYTES * 2));
        let (workspace, context) = fixture.clients();
        assert!(
            expand("@oversized.png", &workspace, &context)
                .await
                .unwrap_err()
                .contains("400 KiB")
        );
        assert!(
            expand("@large.bin", &workspace, &context)
                .await
                .unwrap_err()
                .contains("64 MiB")
        );
        let draft = format!(
            "{} @notes.txt",
            "字".repeat((MAX_USER_MESSAGE_BYTES - 32) / 3)
        );
        let original = draft.clone();
        assert!(
            expand(&draft, &workspace, &context)
                .await
                .unwrap_err()
                .contains("16 KiB")
        );
        assert_eq!(draft, original);
        let preview = payload(&expand("@notes.txt", &workspace, &context).await.unwrap());
        assert_eq!(preview["references"][0]["preview_bytes"], PREVIEW_BYTES);
        assert_eq!(preview["references"][0]["preview_truncated"], true);
    }

    #[tokio::test]
    async fn skill_names_use_effective_precedence_and_explicit_roots() {
        let project = Fixture::new();
        let global = Fixture::new();
        let skill = |name: &str, description: &str| {
            format!(
                "---\nname: {name}\ndescription: {description}\n---\nComplete skill instructions.\n"
            )
        };
        project.write(
            ".agents/skills/check/SKILL.md",
            skill("lint-check", "Project instructions."),
        );
        global.write(
            ".agents/skills/check/SKILL.md",
            skill("lint-check", "Global instructions."),
        );
        global.write(
            ".agents/skills/extra/SKILL.md",
            skill("external-only", "Explicit global instructions."),
        );
        let workspace = Workspace::new(project.0.clone()).unwrap();
        let context = Context::new(project.0.clone(), vec![global.0.clone()]).unwrap();
        let expanded = expand("使用 $lint-check 和 $external-only", &workspace, &context)
            .await
            .unwrap();
        let data = payload(&expanded);
        assert_eq!(data["references"][0]["root"], "workspace");
        assert_eq!(
            data["references"][0]["description"],
            "Project instructions."
        );
        assert_eq!(data["references"][1]["root"], "context-1");
        assert_eq!(data["references"][1]["read"]["tool"], "ssmcp.skills.read");
        assert!(
            expand("$not-installed", &workspace, &context)
                .await
                .unwrap_err()
                .contains("not found")
        );
    }

    #[test]
    fn utf8_previews_never_split_characters_or_embed_terminal_controls() {
        let text = "界".repeat(400);
        let preview = utf8_preview(&text.as_bytes()[..PREVIEW_BYTES + 1], false).unwrap();
        assert!(preview.len() <= PREVIEW_BYTES);
        assert!(preview.chars().all(|ch| ch == '界'));
        assert!(utf8_preview(b"hello\x1b[31m", true).is_none());
        assert!(utf8_preview(b"invalid\xff", true).is_none());
    }
}

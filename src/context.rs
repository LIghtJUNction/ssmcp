//! Deterministic skill metadata and instruction views over explicit read-only roots.
use crate::workspace::Workspace;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path, PathBuf},
};

const MAX_PATH: usize = 1024;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const MAX_SCAN_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 32;
const DEFAULT_PATHS: [&str; 4] = [
    ".agents/skills",
    ".codex/skills",
    ".claude/skills",
    "skills",
];
const INSTRUCTIONS: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", "agents.md"];

struct ContextRoot {
    id: String,
    workspace: Workspace,
}

/// Extra roots are explicitly granted at client startup; they do not extend ordinary fs tools.
pub struct Context {
    roots: Vec<ContextRoot>,
}

impl Context {
    pub fn new(root: PathBuf, extra_roots: Vec<PathBuf>) -> Result<Self, String> {
        if extra_roots.len() > 8 {
            return Err("at most 8 additional context roots are allowed".into());
        }
        let mut roots = vec![ContextRoot {
            id: "workspace".into(),
            workspace: Workspace::new(root)?,
        }];
        for (index, path) in extra_roots.into_iter().enumerate() {
            roots.push(ContextRoot {
                id: format!("context-{}", index + 1),
                workspace: Workspace::new(path)?,
            });
        }
        Ok(Self { roots })
    }

    pub fn tools() -> Vec<Value> {
        let root = json!({"type":"string","maxLength":64,"default":"workspace","description":"Explicit context root ID from context.roots; additional roots are never scanned automatically."});
        let path = json!({"type":"string","maxLength":MAX_PATH,"description":"Path relative to the selected root. Parent traversal and symlinks are rejected."});
        let scan = json!({
            "root":root,
            "paths":{"type":"array","minItems":1,"maxItems":16,"items":path,"default":DEFAULT_PATHS},
            "max_depth":{"type":"integer","minimum":0,"maximum":8,"default":4},
            "max_nodes":{"type":"integer","minimum":1,"maximum":4096,"default":2048},
            "limit":{"type":"integer","minimum":1,"maximum":64,"default":32},
            "cursor":{"type":"string","maxLength":128}
        });
        let read = json!({"root":root,"path":path,"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_DOCUMENT_BYTES,"default":MAX_DOCUMENT_BYTES}});
        let target = json!({"root":root,"path":{"type":"string","maxLength":MAX_PATH,"default":".","description":"Existing directory, file, or a not-yet-created file. Only existing ancestors inside the selected root contribute instructions."}});
        vec![
            tool(
                "ssmcp.context.roots",
                "List explicitly granted read-only context roots; this is not a view of all context loaded by a native agent.",
                json!({}),
                &[],
            ),
            tool(
                "ssmcp.skills.list",
                "Discover bounded SKILL.md metadata in stable path order. Parses name/description using a documented YAML subset; diagnostics identify missing or unsupported metadata. No model calls or body summaries.",
                scan.clone(),
                &[],
            ),
            tool(
                "ssmcp.skills.view",
                "Generate a compact Markdown skill index from the same bounded metadata page as skills.list. The skill body is not included.",
                scan,
                &[],
            ),
            tool(
                "ssmcp.skills.read",
                "Read exact UTF-8 content of one workspace-relative SKILL.md. Oversized files fail explicitly instead of returning incomplete instructions.",
                read.clone(),
                &["path"],
            ),
            tool(
                "ssmcp.agents.list",
                "List instruction origins from the selected root to a target. At each directory choose the first nonempty file among AGENTS.override.md, AGENTS.md, agents.md; zero-byte files are skipped. No parent outside that root is included.",
                target.clone(),
                &[],
            ),
            tool(
                "ssmcp.agents.resolve",
                "Return ordered instruction origins and exact contents for the selected root-to-target chain, with a 256 KiB aggregate content limit. Contents are data, not executed or automatically applied.",
                target,
                &[],
            ),
            tool(
                "ssmcp.agents.read",
                "Read exact UTF-8 content of one AGENTS.override.md, AGENTS.md, or agents.md within an explicit context root.",
                read,
                &["path"],
            ),
        ]
    }

    pub async fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        let value = match name {
            "ssmcp.context.roots" => {
                let _: Empty = parse(args)?;
                json!({"roots":self.roots.iter().map(|root| json!({
                    "id":root.id,"path":root.workspace.root(),"read_only":true,
                    "kind":if root.id=="workspace" {"workspace"} else {"explicit"}
                })).collect::<Vec<_>>(),"boundary":boundary()})
            }
            "ssmcp.skills.list" | "ssmcp.skills.view" => {
                let args: ScanArgs = parse(args)?;
                let root = self.root(&args.root)?;
                skills(root, args, name.ends_with(".view")).await?
            }
            "ssmcp.skills.read" | "ssmcp.agents.read" => {
                let args: ReadArgs = parse(args)?;
                let root = self.root(&args.root)?;
                let path = normalize(&args.path)?;
                let filename = path.rsplit('/').next().unwrap_or("");
                let skill = name == "ssmcp.skills.read";
                if (skill && filename != "SKILL.md")
                    || (!skill && !INSTRUCTIONS.contains(&filename))
                {
                    return Err(
                        "context reads accept only the tool's supported instruction filename"
                            .into(),
                    );
                }
                let content = read_document(&root.workspace, &path, args.max_bytes).await?;
                let mut result = json!({"root":origin(root),"path":path,"bytes":content.len(),"sha256":digest(content.as_bytes()),"content":content,"boundary":boundary()});
                if skill {
                    let mut diagnostics = Vec::new();
                    let metadata = metadata(&content, true, &path, &mut diagnostics);
                    result["metadata"] = metadata;
                    result["diagnostics"] = json!(diagnostics);
                }
                result
            }
            "ssmcp.agents.list" | "ssmcp.agents.resolve" => {
                let args: TargetArgs = parse(args)?;
                agents(
                    self.root(&args.root)?,
                    &args.path,
                    name.ends_with(".resolve"),
                )
                .await?
            }
            _ => return Err(format!("unknown context capability: {name}")),
        };
        if serde_json::to_vec(&value)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_RESPONSE_BYTES
        {
            return Err(
                "context response exceeds 1 MiB; narrow the selected paths or page size".into(),
            );
        }
        Ok(value)
    }

    fn root(&self, id: &str) -> Result<&ContextRoot, String> {
        self.roots.iter().find(|root| root.id == id).ok_or_else(|| {
            "unknown context root ID; use context.roots to discover explicitly granted roots".into()
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScanArgs {
    #[serde(default = "default_root")]
    root: String,
    paths: Option<Vec<String>>,
    #[serde(default = "default_depth")]
    max_depth: usize,
    #[serde(default = "default_nodes")]
    max_nodes: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[serde(default = "default_root")]
    root: String,
    path: String,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    #[serde(default = "default_root")]
    root: String,
    #[serde(default = "default_path")]
    path: String,
}
fn default_root() -> String {
    "workspace".into()
}
fn default_path() -> String {
    ".".into()
}
fn default_depth() -> usize {
    4
}
fn default_nodes() -> usize {
    2048
}
fn default_limit() -> usize {
    32
}
fn default_bytes() -> usize {
    MAX_DOCUMENT_BYTES
}
fn parse<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("invalid context arguments: {error}"))
}
fn boundary() -> &'static str {
    "Only the selected explicitly granted root is inspected. Outside parents, other roots, and native agent-loaded context are not implicitly included."
}
fn origin(root: &ContextRoot) -> Value {
    json!({"id":root.id,"path":root.workspace.root()})
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn normalize(path: &str) -> Result<String, String> {
    if path.len() > MAX_PATH || path.chars().any(char::is_control) {
        return Err(format!(
            "context path must be at most {MAX_PATH} bytes without control characters"
        ));
    }
    let mut parts = Vec::new();
    for part in Path::new(path).components() {
        match part {
            Component::CurDir => {}
            Component::Normal(name) => {
                parts.push(name.to_str().ok_or("context path must be UTF-8")?)
            }
            _ => return Err("context path must be relative without parent traversal".into()),
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}
fn join(directory: &str, name: &str) -> String {
    if directory == "." {
        name.into()
    } else {
        format!("{directory}/{name}")
    }
}
fn missing(error: &str) -> bool {
    error.ends_with("(os error 2)") || cfg!(windows) && error.ends_with("(os error 3)")
}
fn diagnostic(items: &mut Vec<Value>, path: &str, code: &str, message: &str) {
    if items.len() < MAX_DIAGNOSTICS {
        items.push(json!({"path":path,"code":code,"message":message}));
    } else if items
        .last()
        .is_none_or(|item| item["code"] != "diagnostic_limit")
    {
        items[MAX_DIAGNOSTICS - 1] = json!({"path":".","code":"diagnostic_limit","message":"Additional diagnostics were omitted to bound the response."});
    }
}
async fn bytes(
    workspace: &Workspace,
    path: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, u64, bool), String> {
    let result = workspace
        .perform("ssmcp.fs.read", json!({"path":path,"max_bytes":max_bytes}))
        .await?;
    let data = STANDARD
        .decode(
            result["data_base64"]
                .as_str()
                .ok_or("invalid file response")?,
        )
        .map_err(|error| error.to_string())?;
    Ok((
        data,
        result["size"].as_u64().ok_or("invalid file size")?,
        result["eof"].as_bool().ok_or("invalid file EOF")?,
    ))
}
async fn read_document(
    workspace: &Workspace,
    path: &str,
    max_bytes: usize,
) -> Result<String, String> {
    if max_bytes == 0 || max_bytes > MAX_DOCUMENT_BYTES {
        return Err(format!("max_bytes must be 1..={MAX_DOCUMENT_BYTES}"));
    }
    let (data, size, eof) = bytes(workspace, path, max_bytes).await?;
    if !eof {
        return Err(format!(
            "{path} is {size} bytes and exceeds the {max_bytes}-byte full-document limit; no partial instruction content was returned"
        ));
    }
    String::from_utf8(data).map_err(|_| format!("{path} is not valid UTF-8 text"))
}

async fn skills(root: &ContextRoot, args: ScanArgs, view: bool) -> Result<Value, String> {
    if args.max_depth > 8
        || args.max_nodes == 0
        || args.max_nodes > 4096
        || args.limit == 0
        || args.limit > 64
    {
        return Err("max_depth must be 0..=8, max_nodes 1..=4096, and limit 1..=64".into());
    }
    let requested = args
        .paths
        .unwrap_or_else(|| DEFAULT_PATHS.iter().map(|path| (*path).into()).collect());
    if requested.is_empty() || requested.len() > 16 {
        return Err("paths must contain 1..=16 relative directories".into());
    }
    let mut paths = Vec::new();
    for path in requested {
        let path = normalize(&path)?;
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let mut diagnostics = Vec::new();
    let mut sources = Vec::new();
    let mut found = BTreeMap::<String, Value>::new();
    let mut visited = BTreeMap::<String, usize>::new();
    let mut nodes = 0;
    let mut read_bytes = 0;
    let mut truncated = false;
    for source in &paths {
        let stat = root
            .workspace
            .perform("ssmcp.fs.stat", json!({"path":source}))
            .await;
        match stat {
            Ok(value) if value["kind"] == "directory" => {
                sources.push(json!({"path":source,"status":"scanned"}))
            }
            Ok(_) => {
                sources.push(json!({"path":source,"status":"error"}));
                diagnostic(
                    &mut diagnostics,
                    source,
                    "not_directory",
                    "Skill discovery paths must be directories.",
                );
                truncated = true;
                continue;
            }
            Err(error) => {
                let absent = missing(&error);
                sources.push(json!({"path":source,"status":if absent {"missing"} else {"error"}}));
                diagnostic(
                    &mut diagnostics,
                    source,
                    if absent {
                        "missing_root"
                    } else {
                        "unreadable_root"
                    },
                    &error,
                );
                truncated |= !absent;
                continue;
            }
        }
        let mut pending = vec![(source.clone(), 0usize)];
        while let Some((directory, depth)) = pending.pop() {
            if visited
                .get(&directory)
                .is_some_and(|previous| *previous <= depth)
            {
                continue;
            }
            visited.insert(directory.clone(), depth);
            let mut cursor: Option<String> = None;
            let mut entries = Vec::new();
            loop {
                if nodes >= args.max_nodes {
                    truncated = true;
                    diagnostic(
                        &mut diagnostics,
                        &directory,
                        "node_limit",
                        "Discovery stopped at max_nodes; narrow paths or increase the bounded limit.",
                    );
                    break;
                }
                let mut request =
                    json!({"path":directory,"max_entries":(args.max_nodes-nodes).min(128)});
                if let Some(cursor) = &cursor {
                    request["cursor"] = json!(cursor);
                }
                match root.workspace.perform("ssmcp.fs.list", request).await {
                    Ok(page) => {
                        let page_entries = page["entries"]
                            .as_array()
                            .ok_or("invalid directory response")?;
                        nodes += page_entries.len();
                        entries.extend(page_entries.iter().cloned());
                        cursor = page["next_cursor"].as_str().map(str::to_owned);
                        if cursor.is_none() {
                            break;
                        }
                    }
                    Err(error) => {
                        truncated = true;
                        diagnostic(&mut diagnostics, &directory, "unreadable_directory", &error);
                        break;
                    }
                }
            }
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            let mut children = Vec::new();
            for entry in entries {
                let name = entry["name"].as_str().ok_or("invalid directory entry")?;
                let path = join(&directory, name);
                if normalize(&path).is_err() {
                    diagnostic(
                        &mut diagnostics,
                        &directory,
                        "path_limit",
                        "A child path exceeded the context path limit.",
                    );
                    truncated = true;
                    continue;
                }
                if entry["kind"] == "directory" {
                    if depth < args.max_depth {
                        children.push((path, depth + 1));
                    } else {
                        truncated = true;
                        diagnostic(
                            &mut diagnostics,
                            &path,
                            "depth_limit",
                            "Directory omitted at max_depth.",
                        );
                    }
                } else if entry["kind"] == "symlink" {
                    diagnostic(
                        &mut diagnostics,
                        &path,
                        "symlink_skipped",
                        "Context discovery does not follow symlinks.",
                    );
                } else if name == "SKILL.md"
                    && entry["kind"] == "file"
                    && !found.contains_key(&path)
                {
                    if read_bytes >= MAX_SCAN_BYTES {
                        truncated = true;
                        diagnostic(
                            &mut diagnostics,
                            &path,
                            "metadata_byte_limit",
                            "The 1 MiB metadata scan budget was exhausted.",
                        );
                        continue;
                    }
                    match bytes(
                        &root.workspace,
                        &path,
                        MAX_METADATA_BYTES.min(MAX_SCAN_BYTES - read_bytes),
                    )
                    .await
                    {
                        Ok((data, size, eof)) => {
                            read_bytes += data.len();
                            let text = match std::str::from_utf8(&data) {
                                Ok(text) => Some(text),
                                Err(error) if error.error_len().is_none() && !eof => {
                                    std::str::from_utf8(&data[..error.valid_up_to()]).ok()
                                }
                                Err(_) => None,
                            };
                            let meta = if let Some(text) = text {
                                metadata(text, eof, &path, &mut diagnostics)
                            } else {
                                diagnostic(
                                    &mut diagnostics,
                                    &path,
                                    "invalid_utf8",
                                    "Skill metadata is not UTF-8 text.",
                                );
                                json!({"name":null,"description":null,"status":"invalid","parser":"yaml-frontmatter-subset"})
                            };
                            found.insert(path.clone(), json!({"path":path,"directory":directory,"source":source,"name":meta["name"],"description":meta["description"],"metadata_status":meta["status"],"size":size,"metadata_sha256":digest(&data)}));
                        }
                        Err(error) => {
                            truncated = true;
                            diagnostic(&mut diagnostics, &path, "unreadable_skill", &error);
                        }
                    }
                }
            }
            pending.extend(children.into_iter().rev());
        }
    }
    let all: Vec<Value> = found.into_values().collect();
    let mut names = HashSet::new();
    for skill in &all {
        if let Some(name) = skill["name"].as_str()
            && !names.insert(name)
        {
            diagnostic(
                &mut diagnostics,
                skill["path"].as_str().unwrap(),
                "duplicate_name",
                "Another skill has this metadata name; select skills by root ID and path, not by name.",
            );
        }
    }
    let fingerprint = digest(&serde_json::to_vec(&json!({"root":origin(root),"paths":paths,"max_depth":args.max_depth,"max_nodes":args.max_nodes,"skills":all,"sources":sources,"truncated":truncated})).map_err(|error|error.to_string())?);
    let offset = if let Some(cursor) = &args.cursor {
        if cursor.len() > 128 {
            return Err("invalid skill cursor".into());
        }
        let (snapshot, offset) = cursor.split_once(':').ok_or("invalid skill cursor")?;
        if snapshot != fingerprint {
            return Err(
                "skill discovery changed or cursor options differ; restart without cursor".into(),
            );
        }
        offset
            .parse::<usize>()
            .map_err(|_| "invalid skill cursor")?
    } else {
        0
    };
    if offset > all.len() {
        return Err("skill cursor is beyond the discovered index".into());
    }
    let end = offset.saturating_add(args.limit).min(all.len());
    let page = &all[offset..end];
    let mut result = json!({"root":origin(root),"sources":sources,"diagnostics":diagnostics,"total":all.len(),"shown":page.len(),"truncated":truncated,"snapshot":fingerprint,"boundary":boundary(),"limits":{"max_depth":args.max_depth,"max_nodes":args.max_nodes,"visited_nodes":nodes,"metadata_bytes":read_bytes,"max_metadata_bytes_per_file":MAX_METADATA_BYTES,"max_metadata_bytes_total":MAX_SCAN_BYTES,"metadata_field_bytes":2048,"page_size":args.limit},"metadata_parser":"yaml-frontmatter-subset"});
    if end < all.len() {
        result["next_cursor"] = json!(format!("{fingerprint}:{end}"));
    }
    if view {
        result["markdown"] = json!(markdown(page));
    } else {
        result["skills"] = json!(page);
    }
    Ok(result)
}

async fn agents(root: &ContextRoot, target: &str, resolve: bool) -> Result<Value, String> {
    let target = normalize(target)?;
    let parts: Vec<_> = if target == "." {
        vec![]
    } else {
        target.split('/').collect()
    };
    if parts.len() > 63 {
        return Err("instruction target exceeds 64 ancestor directories".into());
    }
    let mut directories = vec![".".to_string()];
    let mut target_exists = true;
    for index in 0..parts.len() {
        let path = parts[..=index].join("/");
        match root
            .workspace
            .perform("ssmcp.fs.stat", json!({"path":path}))
            .await
        {
            Ok(stat) if stat["kind"] == "directory" => directories.push(path),
            Ok(stat) if stat["kind"] == "file" && index + 1 == parts.len() => {}
            Ok(_) => {
                return Err(format!(
                    "{path} is not a directory ancestor or regular target file"
                ));
            }
            Err(error) if missing(&error) => {
                target_exists = false;
                break;
            }
            Err(error) => return Err(format!("cannot inspect instruction target {path}: {error}")),
        }
    }
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    let mut total_bytes = 0;
    for (depth, directory) in directories.into_iter().enumerate() {
        for filename in INSTRUCTIONS {
            let path = normalize(&join(&directory, filename))?;
            let stat = match root
                .workspace
                .perform("ssmcp.fs.stat", json!({"path":path}))
                .await
            {
                Ok(stat) => stat,
                Err(error) if missing(&error) => continue,
                Err(error) => {
                    return Err(format!("cannot inspect instruction file {path}: {error}"));
                }
            };
            if stat["kind"] != "file" {
                return Err(format!("instruction path {path} is not a regular file"));
            }
            let size = stat["size"]
                .as_u64()
                .ok_or("invalid instruction file size")?;
            if size == 0 {
                diagnostic(
                    &mut diagnostics,
                    &path,
                    "empty_instruction",
                    "Zero-byte instruction file skipped; trying the next filename in this directory.",
                );
                continue;
            }
            let mut file = json!({"path":path,"directory":directory,"filename":filename,"depth":depth,"size":size});
            if resolve {
                if size > (MAX_DOCUMENT_BYTES - total_bytes) as u64 {
                    return Err(format!(
                        "instruction chain exceeds the {MAX_DOCUMENT_BYTES}-byte aggregate limit at {path}; list origins and read files individually; no partial chain was returned"
                    ));
                }
                let content =
                    read_document(&root.workspace, &path, MAX_DOCUMENT_BYTES - total_bytes).await?;
                total_bytes += content.len();
                file["sha256"] = json!(digest(content.as_bytes()));
                file["content"] = json!(content);
            }
            files.push(file);
            break;
        }
    }
    Ok(
        json!({"root":origin(root),"target":target,"target_exists":target_exists,"files":files,"diagnostics":diagnostics,"boundary":boundary(),"truncated":false,"content_bytes":if resolve {Some(total_bytes)} else {None},"max_content_bytes":MAX_DOCUMENT_BYTES,"filename_priority":INSTRUCTIONS}),
    )
}

fn metadata(text: &str, eof: bool, path: &str, diagnostics: &mut Vec<Value>) -> Value {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<_> = text.lines().collect();
    if lines.first().copied() != Some("---") {
        diagnostic(
            diagnostics,
            path,
            "missing_frontmatter",
            "SKILL.md must begin with a --- YAML frontmatter block; no metadata was inferred from its body.",
        );
        return json!({"name":null,"description":null,"status":"missing","parser":"yaml-frontmatter-subset"});
    }
    let Some(end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| matches!(*line, "---" | "...").then_some(index))
    else {
        diagnostic(
            diagnostics,
            path,
            if eof {
                "unterminated_frontmatter"
            } else {
                "frontmatter_limit"
            },
            "No closing frontmatter delimiter was found within the bounded metadata read.",
        );
        return json!({"name":null,"description":null,"status":"invalid","parser":"yaml-frontmatter-subset"});
    };
    let mut values = BTreeMap::<&str, String>::new();
    let mut seen = HashSet::new();
    let mut invalid = false;
    let mut index = 1;
    while index < end {
        let line = lines[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            diagnostic(
                diagnostics,
                path,
                "unsupported_yaml",
                "Expected a top-level scalar mapping; complex YAML is not interpreted.",
            );
            invalid = true;
            continue;
        };
        if line.starts_with(char::is_whitespace)
            || key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            || (!raw.is_empty() && !raw.starts_with(char::is_whitespace))
        {
            diagnostic(
                diagnostics,
                path,
                "unsupported_yaml",
                "Only unquoted top-level keys and whitespace-separated mapping values are interpreted by the metadata subset.",
            );
            invalid = true;
            continue;
        }
        let relevant = key == "name" || key == "description";
        let duplicate = !seen.insert(key);
        if duplicate {
            diagnostic(
                diagnostics,
                path,
                "duplicate_metadata_key",
                "Duplicate frontmatter key; its metadata value was not selected.",
            );
            invalid = true;
            values.remove(key);
        }
        let next = (index..end)
            .find(|&position| {
                !lines[position].trim().is_empty()
                    && !lines[position].starts_with(char::is_whitespace)
            })
            .unwrap_or(end);
        if !relevant {
            index = next;
            continue;
        }
        let raw = raw.trim();
        let result = if raw.starts_with(['|', '>']) {
            block_scalar(raw, &lines[index..next])
        } else if lines[index..next]
            .iter()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            Err(
                "Multiline plain or quoted scalars are unsupported; use | or > block syntax."
                    .into(),
            )
        } else {
            scalar(raw)
        };
        index = next;
        match result {
            Ok(value) if !duplicate => {
                if value.is_empty()
                    || value.len() > 2048
                    || value
                        .chars()
                        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
                    || (key == "name" && value.chars().any(char::is_control))
                {
                    diagnostic(
                        diagnostics,
                        path,
                        "metadata_value_limit",
                        "Metadata must be nonempty text of at most 2048 bytes without unsupported control characters.",
                    );
                    invalid = true;
                } else {
                    values.insert(key, value);
                }
            }
            Ok(_) => {}
            Err(error) => {
                diagnostic(diagnostics, path, "unsupported_yaml", &error);
                invalid = true;
            }
        }
    }
    for key in ["name", "description"] {
        if !values.contains_key(key) {
            diagnostic(
                diagnostics,
                path,
                "missing_metadata",
                &format!("No supported {key} string was found in frontmatter."),
            );
        }
    }
    let status = if invalid {
        "invalid"
    } else if values.len() == 2 {
        "ok"
    } else {
        "partial"
    };
    json!({"name":values.get("name"),"description":values.get("description"),"status":status,"parser":"yaml-frontmatter-subset"})
}

fn scalar(raw: &str) -> Result<String, String> {
    if raw.starts_with('"') {
        let mut escaped = false;
        for (index, ch) in raw.char_indices().skip(1) {
            if !escaped && ch == '"' {
                scalar_suffix(&raw[index + 1..])?;
                return serde_json::from_str(&raw[..=index]).map_err(|_| "Unsupported double-quoted scalar escape; JSON-compatible YAML escapes are supported.".into());
            }
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            }
        }
        return Err("Unterminated double-quoted scalar.".into());
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        let mut output = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            if ch == '\'' {
                if chars.peek().is_some_and(|(_, ch)| *ch == '\'') {
                    chars.next();
                    output.push('\'');
                } else {
                    scalar_suffix(&rest[index + 1..])?;
                    return Ok(output);
                }
            } else {
                output.push(ch);
            }
        }
        return Err("Unterminated single-quoted scalar.".into());
    }
    let plain = raw
        .char_indices()
        .find_map(|(index, ch)| {
            (ch == '#' && (index == 0 || raw[..index].ends_with(char::is_whitespace)))
                .then_some(&raw[..index])
        })
        .unwrap_or(raw)
        .trim();
    if plain.starts_with(['&', '*', '!', '[', '{', '@', '`'])
        || plain.starts_with("- ")
        || plain.starts_with("? ")
        || plain.starts_with(": ")
        || plain.contains(": ")
        || matches!(plain, "null" | "Null" | "NULL" | "~" | "true" | "false")
        || plain.parse::<f64>().is_ok()
    {
        return Err("Expected a text scalar; YAML aliases, tags, collections and typed non-string values are not interpreted.".into());
    }
    Ok(plain.into())
}
fn scalar_suffix(suffix: &str) -> Result<(), String> {
    if suffix.trim().is_empty()
        || (suffix.starts_with(char::is_whitespace) && suffix.trim_start().starts_with('#'))
    {
        Ok(())
    } else {
        Err("Unexpected text after a quoted scalar.".into())
    }
}
fn block_scalar(header: &str, lines: &[&str]) -> Result<String, String> {
    let marker = header.split_whitespace().next().unwrap_or("");
    scalar_suffix(&header[marker.len()..])?;
    let header = marker;
    if !matches!(header, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
        return Err(
            "Only | and > block scalars with optional + or - chomping are supported.".into(),
        );
    }
    if lines.iter().any(|line| line.starts_with('\t')) {
        return Err("Tabs are not supported for YAML block indentation.".into());
    }
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    if indent == 0 && lines.iter().any(|line| !line.trim().is_empty()) {
        return Err("Block scalar content must be indented.".into());
    }
    let values: Vec<_> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                ""
            } else {
                &line[indent..]
            }
        })
        .collect();
    let end = values
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |index| index + 1);
    let mut output = String::new();
    for (index, line) in values[..end].iter().enumerate() {
        if header.starts_with('|') {
            if index > 0 {
                output.push('\n');
            }
        } else if line.is_empty() {
            output.push('\n');
            continue;
        } else if index > 0 && !values[index - 1].is_empty() {
            output.push(
                if line.starts_with(' ') || values[index - 1].starts_with(' ') {
                    '\n'
                } else {
                    ' '
                },
            );
        }
        output.push_str(line);
    }
    if end > 0 && !header.ends_with('-') {
        output.push('\n');
        if header.ends_with('+') {
            output.push_str(&"\n".repeat(values.len() - end));
        }
    }
    Ok(output)
}
fn markdown(skills: &[Value]) -> String {
    let mut output = "# Available skills\n\nGenerated from name and description frontmatter only.\n\n| Skill | Description | Path |\n| --- | --- | --- |\n".to_string();
    for skill in skills {
        let name = skill["name"].as_str().unwrap_or("Metadata unavailable");
        let description = skill["description"]
            .as_str()
            .unwrap_or("Read SKILL.md and inspect metadata diagnostics.");
        output.push_str(&format!(
            "| {} | {} | {} |\n",
            escape_markdown(name),
            escape_markdown(description),
            escape_markdown(skill["path"].as_str().unwrap_or(""))
        ));
    }
    output
}
fn escape_markdown(value: &str) -> String {
    let mut result = String::new();
    for ch in value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
    {
        match ch {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '|' | '\\' | '*' | '_' | '[' | ']' | '`' => {
                result.push('\\');
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    result
}

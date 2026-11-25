//! Deterministic skill metadata and instruction views over explicit read-only roots.
use crate::workspace::Workspace;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
};
mod skill_sources;
pub(crate) use skill_sources::BUILTIN_SSMCP_SKILL;

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
    workspace: Option<Workspace>,
    kind: &'static str,
    scope: &'static str,
    skill_only: bool,
    aliases: Vec<PathBuf>,
}

impl ContextRoot {
    fn workspace(&self) -> Result<&Workspace, String> {
        self.workspace
            .as_ref()
            .ok_or_else(|| "This skill is embedded in ssmcp".into())
    }
}

/// Extra roots are explicitly granted at client startup; they do not extend ordinary fs tools.
pub struct Context {
    roots: Vec<ContextRoot>,
    skill_sources: OnceLock<skill_sources::Sources>,
    output: Arc<crate::output::OutputStore>,
}

impl Context {
    pub fn new(root: PathBuf, extra_roots: Vec<PathBuf>) -> Result<Self, String> {
        Self::with_output(root, extra_roots, crate::output::OutputStore::global()?)
    }

    pub fn with_output(
        root: PathBuf,
        extra_roots: Vec<PathBuf>,
        output: std::sync::Arc<crate::output::OutputStore>,
    ) -> Result<Self, String> {
        if extra_roots.len() > 8 {
            return Err("at most 8 additional context roots are allowed".into());
        }
        let mut roots = vec![ContextRoot {
            id: "workspace".into(),
            workspace: Some(Workspace::with_output(root, output.clone())?),
            kind: "workspace",
            scope: "project",
            skill_only: false,
            aliases: Vec::new(),
        }];
        for (index, path) in extra_roots.into_iter().enumerate() {
            roots.push(ContextRoot {
                id: format!("context-{}", index + 1),
                workspace: Some(Workspace::with_output(path, output.clone())?),
                kind: "explicit",
                scope: "global",
                skill_only: false,
                aliases: Vec::new(),
            });
        }
        Ok(Self {
            roots,
            skill_sources: OnceLock::new(),
            output,
        })
    }

    /// Opt in only from real client startup. Explicit constructors remain hermetic.
    pub fn enable_default_skill_sources(&self) -> Result<(), String> {
        if self.skill_sources.get().is_some() {
            return Ok(());
        }
        let (paths, home) = skill_sources::default_paths();
        self.enable_skill_sources(paths, home)
    }

    pub(crate) fn enable_skill_sources(
        &self,
        paths: Vec<PathBuf>,
        home: Option<PathBuf>,
    ) -> Result<(), String> {
        if self.skill_sources.get().is_none() {
            let sources = skill_sources::Sources::discover(paths, home, self.output.clone())?;
            let _ = self.skill_sources.set(sources);
        }
        Ok(())
    }

    fn all_roots(&self) -> Vec<&ContextRoot> {
        self.roots
            .iter()
            .chain(
                self.skill_sources
                    .get()
                    .into_iter()
                    .flat_map(|sources| sources.roots.iter()),
            )
            .collect()
    }

    pub fn tools() -> Vec<Value> {
        let root = json!({"type":"string","maxLength":64,"default":"workspace","description":"Source ID from context.roots: project, explicitly configured roots, exact installed global skill directories or packages, and embedded skills."});
        let path = json!({"type":"string","maxLength":MAX_PATH,"description":"Path relative to the selected root. Parent traversal and symlinks are rejected."});
        let scan = json!({
            "root":root,
            "scope":{"type":"string","enum":["root","effective"],"description":"root scans one granted root; effective overlays project, explicit context roots in configuration order, installed global skill sources, then builtins. view defaults to effective when neither root nor scope is supplied; list defaults to root."},
            "paths":{"type":"array","minItems":1,"maxItems":16,"items":path,"default":DEFAULT_PATHS},
            "max_depth":{"type":"integer","minimum":0,"maximum":8,"default":4},
            "max_nodes":{"type":"integer","minimum":1,"maximum":4096,"default":2048},
            "limit":{"type":"integer","minimum":1,"maximum":64,"default":32},
            "cursor":{"type":"string","maxLength":128}
        });
        let read = json!({"root":root,"path":path,"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_DOCUMENT_BYTES,"default":MAX_DOCUMENT_BYTES}});
        let target = json!({"root":root,"path":{"type":"string","maxLength":MAX_PATH,"default":".","description":"Existing directory, file, or a not-yet-created file. Only existing ancestors inside the selected root contribute instructions."}});
        let mut tools = vec![
            tool(
                "ssmcp.context.roots",
                "List read-only context and skill sources with their exact scope, canonical paths and aliases. Skill-only sources do not extend ordinary file or instruction access.",
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
                "Generate a live Markdown skill index. Project skills take precedence over configured roots, installed global skills and builtins; root selects one source. Duplicate origins remain visible in shadowed. Bodies are read separately.",
                scan,
                &[],
            ),
            tool(
                "ssmcp.skills.read",
                "Read exact UTF-8 content of one SKILL.md from its granted source or the embedded registry. Oversized files fail explicitly instead of returning incomplete instructions.",
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
        ];
        tools.extend(crate::client::context_memory::tools());
        tools
    }

    /// Metadata discovery initiated by the local picker or reference resolver.
    /// This entry point is not a native tool and cannot be enabled by JSON flags.
    pub(crate) async fn local_skill_index(&self, args: Value) -> Result<Value, String> {
        let args: ScanArgs = parse(args)?;
        self.skill_list(&args, false, true).await
    }

    async fn skill_list(
        &self,
        args: &ScanArgs,
        view: bool,
        local_discovery: bool,
    ) -> Result<Value, String> {
        // The overview has its own discovery summary. Keep its underlying
        // prefix reads in the file audit alongside those of the local picker.
        let audit_only = local_discovery || view;
        let (all, result) = if args.scope == SkillScope::Effective {
            if args.root != "workspace" {
                return Err("effective skills use workspace as the project root; omit root or select scope: root".into());
            }
            effective_skills(
                &self.all_roots(),
                args,
                self.skill_sources.get(),
                audit_only,
            )
            .await?
        } else {
            scan_skills(self.root(&args.root)?, args, audit_only).await?
        };
        skill_page(all, result, args, view)
    }

    pub async fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        let value = match name {
            "ssmcp.context.status"
            | "ssmcp.context.read"
            | "ssmcp.context.list"
            | "ssmcp.context.compact"
            | "ssmcp.context.fork"
            | "ssmcp.context.switch" => {
                let name = name.to_owned();
                let source = crate::runtime::call_scope::Source::capture();
                return tokio::task::spawn_blocking(move || {
                    source.blocking(|| crate::client::context_memory::call(&name, args))
                })
                .await
                .map_err(|_| "context memory worker stopped".to_owned())?;
            }
            "ssmcp.context.roots" => {
                let _: Empty = parse(args)?;
                json!({"roots":self.all_roots().into_iter().map(origin).collect::<Vec<_>>(),
                    "diagnostics":self.skill_sources.get().map(|sources| &sources.diagnostics),"boundary":boundary()})
            }
            "ssmcp.skills.list" | "ssmcp.skills.view" => {
                let view = name.ends_with(".view");
                let mut args = args;
                if view
                    && args.get("root").is_none()
                    && args.get("scope").is_none()
                    && let Some(object) = args.as_object_mut()
                {
                    object.insert("scope".into(), json!("effective"));
                }
                let args: ScanArgs = parse(args)?;
                self.skill_list(&args, view, false).await?
            }
            "ssmcp.skills.read" | "ssmcp.agents.read" => {
                let args: ReadArgs = parse(args)?;
                let root = self.root(&args.root)?;
                let path = normalize(&args.path)?;
                let filename = path.rsplit('/').next().unwrap_or("");
                let skill = name == "ssmcp.skills.read";
                if !skill && root.skill_only {
                    return Err(
                        "This root grants skills only; it is not an instruction or filesystem root"
                            .into(),
                    );
                }
                if (skill && filename != "SKILL.md")
                    || (!skill && !INSTRUCTIONS.contains(&filename))
                {
                    return Err(
                        "context reads accept only the tool's supported instruction filename"
                            .into(),
                    );
                }
                let content = if root.kind == "builtin" {
                    skill_sources::read_builtin(&path, args.max_bytes)?
                } else {
                    read_document(root.workspace()?, &path, args.max_bytes).await?
                };
                let mut result = json!({"root":origin(root),"scope":root.scope,"source":document_source(root,&path),"path":path,"bytes":content.len(),"sha256":digest(content.as_bytes()),"content":content,"boundary":boundary()});
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
                if self.root(&args.root)?.skill_only {
                    return Err(
                        "This root grants skills only; it is not an instruction or filesystem root"
                            .into(),
                    );
                }
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
        self.all_roots()
            .into_iter()
            .find(|root| root.id == id)
            .ok_or_else(|| {
                "unknown context root ID; use context.roots to discover the granted sources".into()
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SkillScope {
    #[default]
    Root,
    Effective,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScanArgs {
    #[serde(default = "default_root")]
    root: String,
    #[serde(default)]
    scope: SkillScope,
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
    "Reads stay inside the selected granted root. Automatic global roots and linked packages grant SKILL.md access only; the ordinary filesystem workspace and instruction roots are unchanged."
}
fn origin(root: &ContextRoot) -> Value {
    json!({"id":root.id,"path":root.workspace.as_ref().map(|workspace| workspace.root().to_string_lossy().into_owned()).unwrap_or_else(|| "ssmcp-client://skill".into()),
        "kind":root.kind,"scope":root.scope,"read_only":true,"skills_only":root.skill_only,"aliases":root.aliases})
}
fn document_source(root: &ContextRoot, path: &str) -> String {
    root.workspace
        .as_ref()
        .map(|workspace| workspace.root().join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("ssmcp-client://skill#{path}"))
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}
fn digest(bytes: &[u8]) -> String {
    crate::hex::encode(Sha256::digest(bytes))
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
    local_discovery: bool,
) -> Result<(Vec<u8>, u64, bool), String> {
    let result = if local_discovery {
        workspace
            .read_metadata_for_completion(path, max_bytes)
            .await?
    } else {
        workspace
            .perform("ssmcp.fs.read", json!({"path":path,"max_bytes":max_bytes}))
            .await?
    };
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
    let (data, size, eof) = bytes(workspace, path, max_bytes, false).await?;
    if !eof {
        return Err(format!(
            "{path} is {size} bytes and exceeds the {max_bytes}-byte full-document limit; no partial instruction content was returned"
        ));
    }
    String::from_utf8(data).map_err(|_| format!("{path} is not valid UTF-8 text"))
}

async fn metadata_bytes(
    workspace: &Workspace,
    path: &str,
    budget: usize,
    local_discovery: bool,
) -> Result<(Vec<u8>, u64, bool, usize, bool), String> {
    let mut limit = 512.min(budget).min(MAX_METADATA_BYTES);
    let mut spent = 0;
    loop {
        let (data, size, eof) = bytes(workspace, path, limit, local_discovery).await?;
        spent += data.len();
        let complete = eof || metadata_prefix_complete(&data);
        if complete || limit == MAX_METADATA_BYTES {
            return Ok((data, size, eof, spent, false));
        }
        let next = limit
            .saturating_mul(4)
            .min(MAX_METADATA_BYTES)
            .min(budget.saturating_sub(spent));
        if next <= limit {
            return Ok((data, size, eof, spent, true));
        }
        limit = next;
    }
}

fn metadata_prefix_complete(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    let text = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&data[..error.valid_up_to()]).unwrap_or("")
        }
        Err(_) => return true,
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("").trim_end_matches(['\r', '\n']);
    if first != "---" {
        return true;
    }
    lines.any(|line| {
        line.ends_with('\n') && matches!(line.trim_end_matches(['\r', '\n']), "---" | "...")
    })
}

async fn scan_skills(
    root: &ContextRoot,
    args: &ScanArgs,
    local_discovery: bool,
) -> Result<(Vec<Value>, Value), String> {
    if args.max_depth > 8
        || args.max_nodes == 0
        || args.max_nodes > 4096
        || args.limit == 0
        || args.limit > 64
    {
        return Err("max_depth must be 0..=8, max_nodes 1..=4096, and limit 1..=64".into());
    }
    if root.kind == "builtin" {
        return skill_sources::scan_builtin(root, args);
    }
    let workspace = root.workspace()?;
    let requested = args.paths.clone().unwrap_or_else(|| {
        if root.skill_only {
            vec![".".into()]
        } else {
            DEFAULT_PATHS.iter().map(|path| (*path).into()).collect()
        }
    });
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
        let stat = workspace
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
                match workspace.perform("ssmcp.fs.list", request).await {
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
            // A skill package owns its assets and scripts. Their descendants
            // are not additional skills and must not consume discovery budgets.
            let package = entries
                .iter()
                .any(|entry| entry["name"] == "SKILL.md" && entry["kind"] == "file");
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
                    // Automatic descent excludes discarded packages. An
                    // explicit paths entry rooted inside .trash still scans
                    // normally, and exact document reads remain available.
                    if package
                        || matches!(
                            name.to_ascii_lowercase().as_str(),
                            ".trash" | ".archive" | ".cache"
                        )
                    {
                        continue;
                    }
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
                    match metadata_bytes(
                        workspace,
                        &path,
                        MAX_SCAN_BYTES - read_bytes,
                        local_discovery,
                    )
                    .await
                    {
                        Ok((data, size, eof, spent, limited)) => {
                            read_bytes += spent;
                            if limited {
                                truncated = true;
                                diagnostic(
                                    &mut diagnostics,
                                    &path,
                                    "metadata_byte_limit",
                                    "The metadata scan budget ended before this frontmatter was complete",
                                );
                            }
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
                            found.insert(path.clone(), json!({"root":origin(root),"scope":root.scope,"origin_path":document_source(root,&path),"path":path,"directory":directory,"source":source,"name":meta["name"],"description":meta["description"],"metadata_status":meta["status"],"size":size,"metadata_sha256":digest(&data)}));
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
    let result = json!({"root":origin(root),"scope":"root","sources":sources,"diagnostics":diagnostics,"total":all.len(),"truncated":truncated,"snapshot":fingerprint,"boundary":boundary(),"limits":{"max_depth":args.max_depth,"max_nodes":args.max_nodes,"visited_nodes":nodes,"metadata_bytes":read_bytes,"max_metadata_bytes_per_file":MAX_METADATA_BYTES,"max_metadata_bytes_total":MAX_SCAN_BYTES,"metadata_field_bytes":2048,"page_size":args.limit},"metadata_parser":"yaml-frontmatter-subset"});
    Ok((all, result))
}

/// Scan each authorized root once per read. No file contents or metadata are cached.
async fn effective_skills(
    roots: &[&ContextRoot],
    args: &ScanArgs,
    defaults: Option<&skill_sources::Sources>,
    local_discovery: bool,
) -> Result<(Vec<Value>, Value), String> {
    let paths = args
        .paths
        .clone()
        .unwrap_or_else(|| DEFAULT_PATHS.iter().map(|path| (*path).into()).collect());
    let mut effective = Vec::<Value>::new();
    let mut names = BTreeMap::<String, usize>::new();
    let mut documents = BTreeMap::<String, usize>::new();
    let mut snapshots = Vec::new();
    let mut sources = Vec::new();
    let mut diagnostics = defaults
        .map(|sources| sources.diagnostics.clone())
        .unwrap_or_default();
    let mut truncated = defaults.is_some_and(|sources| sources.truncated);
    let mut visited_nodes = 0;
    let mut metadata_bytes = 0;
    for root in roots {
        let (mut candidates, scan) = scan_skills(root, args, local_discovery).await?;
        snapshots.push(scan["snapshot"].clone());
        truncated |= scan["truncated"] == true;
        visited_nodes += scan["limits"]["visited_nodes"].as_u64().unwrap_or(0);
        metadata_bytes += scan["limits"]["metadata_bytes"].as_u64().unwrap_or(0);
        for source in scan["sources"].as_array().ok_or("invalid skill sources")? {
            let mut source = source.clone();
            source["root"] = origin(root);
            sources.push(source);
        }
        for item in scan["diagnostics"]
            .as_array()
            .ok_or("invalid skill diagnostics")?
        {
            let mut item = item.clone();
            item["root"] = origin(root);
            diagnostics.push(item);
        }
        // Explicit search-path order breaks ties within one root.
        candidates.sort_by_key(|skill| {
            (
                paths
                    .iter()
                    .position(|path| normalize(path).ok().as_deref() == skill["source"].as_str())
                    .unwrap_or(usize::MAX),
                skill["path"].as_str().unwrap_or("").to_owned(),
            )
        });
        for mut candidate in candidates {
            let name = (candidate["metadata_status"] == "ok")
                .then(|| candidate["name"].as_str())
                .flatten()
                .map(str::to_owned);
            let document = candidate["origin_path"].as_str().unwrap_or("").to_owned();
            if let Some(index) = name
                .as_ref()
                .and_then(|name| names.get(name))
                .or_else(|| documents.get(&document))
                .copied()
            {
                let same_document = effective[index]["origin_path"] == candidate["origin_path"];
                effective[index]["shadowed"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({
                        "root":candidate["root"],"path":candidate["path"],
                        "source":candidate["source"],"scope":candidate["scope"],"origin_path":candidate["origin_path"],
                        "same_document":same_document
                    }));
            } else {
                if let Some(name) = name {
                    names.insert(name, effective.len());
                }
                documents.insert(document, effective.len());
                candidate["scope"] = json!(root.scope);
                candidate["shadowed"] = json!([]);
                effective.push(candidate);
            }
        }
    }
    effective.sort_by(|a, b| {
        a["name"]
            .as_str()
            .cmp(&b["name"].as_str())
            .then_with(|| a["root"]["id"].as_str().cmp(&b["root"]["id"].as_str()))
            .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
    });
    let fingerprint = digest(
        &serde_json::to_vec(&json!({
            "scope":"effective","snapshots":snapshots,"skills":effective
        }))
        .map_err(|error| error.to_string())?,
    );
    let result = json!({
        "root":origin(roots[0]),"roots":roots.iter().map(|root| origin(root)).collect::<Vec<_>>(),
        "scope":"effective","sources":sources,"diagnostics":diagnostics,
        "total":effective.len(),"truncated":truncated,"snapshot":fingerprint,
        "boundary":boundary(),
        "precedence":"project workspace, explicit context roots in configuration order, global skill directories and their exact linked packages in source order, then embedded ssmcp skills. Same names and physical documents are deduplicated; every later origin is retained in shadowed.",
        "limits":{"max_depth":args.max_depth,"max_nodes_per_root":args.max_nodes,
            "visited_nodes":visited_nodes,"metadata_bytes":metadata_bytes,
            "max_metadata_bytes_per_file":MAX_METADATA_BYTES,
            "max_metadata_bytes_total":MAX_SCAN_BYTES * roots.len(),
            "metadata_field_bytes":2048,"page_size":args.limit},
        "metadata_parser":"yaml-frontmatter-subset"
    });
    Ok((effective, result))
}

fn skill_page(
    all: Vec<Value>,
    mut result: Value,
    args: &ScanArgs,
    view: bool,
) -> Result<Value, String> {
    let fingerprint = result["snapshot"]
        .as_str()
        .ok_or("missing skill snapshot")?
        .to_owned();
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
    result["shown"] = json!(page.len());
    if end < all.len() {
        result["next_cursor"] = json!(format!("{fingerprint}:{end}"));
    }
    if view {
        result["markdown"] = json!(markdown(page, result["scope"] == "effective"));
    } else {
        result["skills"] = json!(page);
    }
    Ok(result)
}

async fn agents(root: &ContextRoot, target: &str, resolve: bool) -> Result<Value, String> {
    let workspace = root.workspace()?;
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
        match workspace
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
            let stat = match workspace
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
                    read_document(workspace, &path, MAX_DOCUMENT_BYTES - total_bytes).await?;
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
fn markdown(skills: &[Value], effective: bool) -> String {
    let mut output = if effective {
        "# Available skills\n\nProject skills overlay configured roots, installed global skill sources and builtins. Metadata only; read SKILL.md from the listed source for the complete content.\n\n| Skill | Description | Root | Path | Shadowed |\n| --- | --- | --- | --- | --- |\n"
    } else {
        "# Available skills\n\nGenerated from name and description frontmatter only.\n\n| Skill | Description | Root | Path | Shadowed |\n| --- | --- | --- | --- | --- |\n"
    }.to_string();
    for skill in skills {
        let name = skill["name"].as_str().unwrap_or("Metadata unavailable");
        let description = skill["description"]
            .as_str()
            .unwrap_or("Read SKILL.md and inspect metadata diagnostics.");
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            escape_markdown(name),
            escape_markdown(description),
            escape_markdown(skill["root"]["id"].as_str().unwrap_or("")),
            escape_markdown(skill["path"].as_str().unwrap_or("")),
            skill["shadowed"].as_array().map_or(0, Vec::len)
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

#[cfg(test)]
mod completion_tests;

#[cfg(test)]
pub(crate) mod read_activity_tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Directory, Context) {
        let base = std::env::temp_dir().join(format!(
            "ssmcp-read-activity-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("workspace");
        fs::create_dir(&root).unwrap();
        for name in ["review", "testing", "write", ".trash/archived"] {
            let path = root.join(format!(".agents/skills/{name}/SKILL.md"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!(
                    "---\nname: {}\ndescription: Fixture metadata\n---\nExact body for {name}.\n",
                    name.rsplit('/').next().unwrap()
                ),
            )
            .unwrap();
        }
        let output = crate::output::OutputStore::at_for_test(base.join("output")).unwrap();
        let context = Context::with_output(root, vec![], output).unwrap();
        (Directory(base), context)
    }

    /// Actual native file audit records, consumed by the display projection's
    /// scan regression instead of manufacturing successful read observations.
    pub(crate) async fn native_scan() -> (Vec<Value>, Value) {
        let (_directory, context) = fixture();
        let listed = context
            .perform("ssmcp.skills.list", json!({}))
            .await
            .unwrap();
        let audit = context.roots[0]
            .workspace()
            .unwrap()
            .perform("ssmcp.fs.events", json!({"max_events":256}))
            .await
            .unwrap();
        let events = audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["event"].clone())
            .collect();
        drop(context);
        (events, listed)
    }

    #[tokio::test]
    async fn read_activity_skips_automatic_trash_and_keeps_explicit_access() {
        let (directory, context) = fixture();
        let listed = context
            .perform("ssmcp.skills.list", json!({}))
            .await
            .unwrap();
        assert_eq!(listed["total"], 3);
        assert!(
            listed["skills"]
                .as_array()
                .unwrap()
                .iter()
                .all(|skill| { !skill["path"].as_str().unwrap().contains(".trash/") })
        );
        let archive = ".agents/skills/.trash/archived/SKILL.md";
        let audit = context.roots[0]
            .workspace()
            .unwrap()
            .perform("ssmcp.fs.events", json!({"max_events":256}))
            .await
            .unwrap();
        assert_eq!(audit["events"].as_array().unwrap().len(), 3);
        assert!(audit["events"].as_array().unwrap().iter().all(|item| {
            item["event"]["type"] == "ssmcp.file.read" && item["event"]["path"] != archive
        }));
        let explicit = context
            .perform(
                "ssmcp.skills.list",
                json!({
                    "paths":[".agents/skills/.trash"]
                }),
            )
            .await
            .unwrap();
        assert_eq!(explicit["total"], 1);
        assert_eq!(explicit["skills"][0]["path"], archive);
        let read = context
            .perform("ssmcp.skills.read", json!({"path":archive}))
            .await
            .unwrap();
        assert!(
            read["content"]
                .as_str()
                .unwrap()
                .ends_with("Exact body for .trash/archived.\n")
        );
        assert_eq!(
            fs::read_to_string(directory.0.join("workspace").join(archive)).unwrap(),
            read["content"].as_str().unwrap()
        );
        drop(context);
    }
}

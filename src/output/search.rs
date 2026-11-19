//! Bounded literal search over an owned artifact. No shell, regex, extra copy of
//! the resource, or model call is involved. Byte offsets work for binary data.
use super::{Access, OutputStore, artifact_name, error, read_at};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io;

pub(super) const SCAN_BYTES: usize = 1024 * 1024;
pub(super) const RESPONSE_BYTES: usize = 24 * 1024;
const MATCHES: usize = 32;
const CONTEXT_BYTES: usize = 512;
const QUERY_BYTES: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    id: String,
    query: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    snapshot_bytes: Option<u64>,
    #[serde(default = "scan_bytes")]
    max_scan_bytes: usize,
    #[serde(default = "matches")]
    max_matches: usize,
    #[serde(default = "context_bytes")]
    context_bytes: usize,
}

fn scan_bytes() -> usize {
    SCAN_BYTES
}
fn matches() -> usize {
    8
}
fn context_bytes() -> usize {
    128
}

pub(super) fn tool() -> Value {
    json!({"name":"ssmcp.output.search",
        "description":"Search an owned output resource for an exact, case-sensitive UTF-8 keyword (1..256 bytes). Each call scans at most 1 MiB of match-start positions plus bounded lookahead/context, returns absolute byte offsets and raw base64, and limits the serialized search value to 24 KiB (excluding the MCP envelope). Continue with next_offset and the same snapshot_bytes. scan_complete means the selected saved snapshot was searched; capture_complete separately reports whether the original capture finished. No regex, shell or model is run. Use output.stat when a fresh whole-resource hash is required.",
        "inputSchema":{"type":"object","properties":{
            "id":{"type":"string","pattern":"^[a-f0-9]{32}$"},
            "query":{"type":"string","minLength":1,"maxLength":QUERY_BYTES,"description":"Literal UTF-8 keyword; maximum 256 encoded bytes."},
            "offset":{"type":"integer","minimum":0,"default":0},
            "snapshot_bytes":{"type":"integer","minimum":0,"description":"Fix the exclusive byte boundary across pages of a growing capture; omitted means its current size."},
            "max_scan_bytes":{"type":"integer","minimum":1,"maximum":SCAN_BYTES,"default":SCAN_BYTES},
            "max_matches":{"type":"integer","minimum":1,"maximum":MATCHES,"default":8},
            "context_bytes":{"type":"integer","minimum":0,"maximum":CONTEXT_BYTES,"default":128}
        },"required":["id","query"],"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false}})
}

pub(super) fn perform(store: &OutputStore, value: Value) -> Result<Value, String> {
    let args: Arguments = serde_json::from_value(value).map_err(error)?;
    if args.query.is_empty() || args.query.len() > QUERY_BYTES {
        return Err(format!("query must contain 1..={QUERY_BYTES} UTF-8 bytes"));
    }
    if !(1..=SCAN_BYTES).contains(&args.max_scan_bytes)
        || !(1..=MATCHES).contains(&args.max_matches)
        || args.context_bytes > CONTEXT_BYTES
    {
        return Err("search limits exceed the bounded scan, match or context budget".into());
    }
    let metadata = store.manifest(&args.id)?;
    let file = store
        .open(&artifact_name(&args.id), Access::Read)
        .map_err(error)?;
    let size = file.metadata().map_err(error)?.len();
    store.check_snapshot_size(&metadata, size)?;
    let snapshot = args.snapshot_bytes.unwrap_or(size);
    if snapshot > size || args.offset > snapshot {
        return Err("search offset or snapshot is beyond the saved resource".into());
    }
    let scan_end = args
        .offset
        .saturating_add(args.max_scan_bytes as u64)
        .min(snapshot);
    let read_start = if args.offset == snapshot {
        snapshot
    } else {
        args.offset.saturating_sub(args.context_bytes as u64)
    };
    // Look ahead by query_len-1 so a match starting before scan_end cannot be
    // lost at a page boundary. Context adds only a fixed amount on either side.
    let read_end = scan_end
        .saturating_add((args.query.len() - 1 + args.context_bytes) as u64)
        .min(snapshot);
    let mut data = vec![0; (read_end - read_start) as usize];
    let mut read = 0;
    while read < data.len() {
        match read_at(&file, &mut data[read..], read_start + read as u64) {
            Ok(0) => return Err("output resource ended before the selected snapshot".into()),
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    let mut result = json!({
        "id":args.id,"query":args.query,"offset":args.offset,"next_offset":scan_end,
        "snapshot_bytes":snapshot,"current_size_bytes":size,"read_bytes":read,
        "scan_complete":scan_end == snapshot,"capture_complete":metadata.complete,
        "capture_error":metadata.error,"observed_bytes":metadata.observed_bytes,
        "recorded_snapshot":{"bytes":metadata.bytes,"sha256":metadata.sha256,"sha256_scope":metadata.sha256_scope},
        "whole_resource_hash_rechecked":false,
        "matches":[],"match_count":0,"response_budget_bytes":RESPONSE_BYTES,
        "response_budget_scope":"search_value_json"
    });
    // Reserve space for changing counters. Count actual escaped JSON, not a
    // character estimate, so binary/NUL-heavy contexts cannot defeat the cap.
    let mut encoded_bytes = serde_json::to_vec(&result).map_err(error)?.len() + 256;
    let mut found = Vec::new();
    let mut next = scan_end;
    let start = (args.offset - read_start) as usize;
    let limit = (scan_end - read_start) as usize;
    let matching_end = limit.saturating_add(args.query.len() - 1).min(data.len());
    let finder = memchr::memmem::Finder::new(args.query.as_bytes());
    let mut cursor = start;
    while cursor < limit {
        let Some(relative) = finder.find(&data[cursor..matching_end]) else {
            break;
        };
        let position = cursor + relative;
        if position >= limit {
            break;
        }
        let first = position.saturating_sub(args.context_bytes);
        let last = position
            .saturating_add(args.query.len() + args.context_bytes)
            .min(data.len());
        let context = &data[first..last];
        let entry = json!({
            "offset":read_start + position as u64,"match_bytes":args.query.len(),
            "context_offset":read_start + first as u64,"context_end_offset":read_start + last as u64,
            "text":String::from_utf8_lossy(context),"text_is_lossy":std::str::from_utf8(context).is_err(),
            "data_base64":STANDARD.encode(context),"sha256":format!("{:x}",Sha256::digest(context)),"sha256_scope":"returned_context"
        });
        let entry_bytes = serde_json::to_vec(&entry).map_err(error)?.len() + 1;
        if encoded_bytes + entry_bytes > RESPONSE_BYTES {
            if found.is_empty() {
                return Err("one search context exceeds the response budget".into());
            }
            next = read_start + position as u64;
            break;
        }
        encoded_bytes += entry_bytes;
        found.push(entry);
        cursor = position + 1; // Retain overlapping matches across pages.
        if found.len() == args.max_matches {
            next = (read_start + cursor as u64).min(scan_end);
            if next.saturating_add(args.query.len() as u64) > snapshot {
                next = snapshot;
            }
            break;
        }
    }
    result["next_offset"] = json!(next);
    result["scan_complete"] = json!(next == snapshot);
    result["match_count"] = json!(found.len());
    result["matches"] = json!(found);
    if serde_json::to_vec(&result).map_err(error)?.len() > RESPONSE_BYTES {
        return Err("search response exceeded its fixed byte budget".into());
    }
    Ok(result)
}

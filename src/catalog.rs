//! Compact immutable tool descriptions shared by providers, routes and list snapshots.
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::{borrow::Cow, sync::Arc};

#[derive(Debug)]
struct Descriptor {
    bytes: Box<[u8]>,
    original_len: usize,
    compressed: bool,
    name: Option<String>,
    input_object: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolDescriptor(Arc<Descriptor>);

impl ToolDescriptor {
    pub(crate) fn from_value(value: Value) -> Result<Self, String> {
        let raw = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
        let name = value.get("name").and_then(Value::as_str).map(str::to_owned);
        let input_object = value.get("inputSchema").is_some_and(Value::is_object);
        // Drop the expanded tree before allocating its long-lived compressed
        // representation, so temporary JSON nodes do not pin adjacent heap pages.
        drop(value);
        let original_len = raw.len();
        // Tiny schemas are cheaper to retain directly. Larger descriptions contain
        // repeated JSON keys and prose; decode only the requested page or route.
        let compressed = if raw.len() >= 512 {
            let encoded = miniz_oxide::deflate::compress_to_vec(&raw, 6);
            (encoded.len() + 64 < raw.len()).then_some(encoded)
        } else {
            None
        };
        let is_compressed = compressed.is_some();
        Ok(Self(Arc::new(Descriptor {
            bytes: compressed.unwrap_or(raw).into_boxed_slice(),
            original_len,
            compressed: is_compressed,
            name,
            input_object,
        })))
    }

    fn json(&self) -> Result<Cow<'_, [u8]>, String> {
        if !self.0.compressed {
            return Ok(Cow::Borrowed(&self.0.bytes));
        }
        let bytes =
            miniz_oxide::inflate::decompress_to_vec_with_limit(&self.0.bytes, self.0.original_len)
                .map_err(|error| format!("cannot decode native tool description: {error:?}"))?;
        if bytes.len() != self.0.original_len {
            return Err("native tool description length changed".into());
        }
        Ok(Cow::Owned(bytes))
    }

    pub(crate) fn value(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.json()?).map_err(|error| error.to_string())
    }

    pub(crate) fn len(&self) -> usize {
        self.0.original_len
    }

    pub(crate) fn header(&self) -> Result<(String, bool), String> {
        Ok((
            self.0.name.clone().ok_or("native tool has no name")?,
            self.0.input_object,
        ))
    }

    #[cfg(test)]
    pub(crate) fn shares_storage(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq for ToolDescriptor {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.original_len == other.0.original_len
                && self.0.compressed == other.0.compressed
                && self.0.bytes == other.0.bytes)
    }
}

impl Serialize for ToolDescriptor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

pub(crate) fn compact(tools: Vec<Value>) -> Result<Vec<ToolDescriptor>, String> {
    tools.into_iter().map(ToolDescriptor::from_value).collect()
}

pub(crate) fn encoded_len(tools: &[ToolDescriptor]) -> usize {
    2 + tools.iter().map(ToolDescriptor::len).sum::<usize>() + tools.len().saturating_sub(1)
}

pub(crate) fn fingerprint(tools: &[ToolDescriptor]) -> String {
    // Hash the immutable stored representation without decoding the whole catalog.
    // The cursor is an opaque generation marker, not an authentication token.
    let mut hash = 0xcbf29ce484222325u64;
    for tool in tools {
        for byte in (tool.len() as u64)
            .to_le_bytes()
            .iter()
            .chain(tool.0.bytes.iter())
        {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

pub(crate) fn page(
    tools: &[ToolDescriptor],
    cursor: Option<&str>,
    fingerprint: &str,
) -> Result<Value, String> {
    let start = match cursor {
        None => 0,
        Some(cursor) => {
            let (requested, index) = cursor.split_once(':').ok_or("invalid tool cursor")?;
            if requested != fingerprint {
                return Err("tool catalog changed; restart tools/list without a cursor".into());
            }
            index.parse::<usize>().map_err(|_| "invalid tool cursor")?
        }
    };
    if start > tools.len() {
        return Err("tool cursor is beyond the catalog".into());
    }
    let mut end = start;
    let mut bytes = 0;
    while end < tools.len() && end - start < 64 {
        let length = tools[end].len();
        if length > 896 * 1024 {
            return Err("one tool schema exceeds the transport limit".into());
        }
        if end > start && bytes + length > 512 * 1024 {
            break;
        }
        bytes += length;
        end += 1;
    }
    let values: Vec<_> = tools[start..end]
        .iter()
        .map(ToolDescriptor::value)
        .collect::<Result<_, _>>()?;
    let mut result = Value::Object(serde_json::Map::from_iter([(
        "tools".into(),
        Value::Array(values),
    )]));
    if end < tools.len() {
        result["nextCursor"] = json!(format!("{fingerprint}:{end}"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_pages_preserve_native_fields_and_share_storage() {
        let values: Vec<_> = (0..130)
            .map(|n| {
                json!({
                    "name": format!("native.tool{n}"),
                    "description": "Complete native description. ".repeat(200),
                    "inputSchema": {"$defs":{"node":{"type":"object"}},"$ref":"#/$defs/node"},
                    "annotations": {"custom": "保留"},
                    "x-native": [1, null, {"value": "complete"}]
                })
            })
            .collect();
        let tools = compact(values.clone()).unwrap();
        let cloned = tools.clone();
        assert!(tools[0].shares_storage(&cloned[0]));
        assert_eq!(
            encoded_len(&tools),
            serde_json::to_vec(&values).unwrap().len()
        );
        assert!(tools.iter().map(|t| t.0.bytes.len()).sum::<usize>() < encoded_len(&tools) / 2);
        let first = page(&tools, None, &fingerprint(&tools)).unwrap();
        let legacy = crate::backend::page(&values, None).unwrap();
        assert_eq!(first["tools"], legacy["tools"]);
        let next = page(&tools, first["nextCursor"].as_str(), &fingerprint(&tools)).unwrap();
        assert_eq!(next["tools"], json!(&values[64..128]));
    }

    #[test]
    fn malformed_or_mismatched_compressed_payloads_fail_with_a_bound() {
        let mut tool = ToolDescriptor::from_value(json!({
            "name":"native.large","inputSchema":{},"description":"large ".repeat(1000)
        }))
        .unwrap();
        Arc::get_mut(&mut tool.0).unwrap().original_len = 8;
        assert!(tool.value().is_err());
        Arc::get_mut(&mut tool.0).unwrap().bytes = vec![0xff, 0xff].into_boxed_slice();
        assert!(tool.value().is_err());
    }
}

//! Real cdylib used only by the explicitly selected native-plugin contract test.
use serde_json::{Value, json};
use zharness::plugin::{Extension, ExtensionExport};
use zplugin::{Error, Result};

struct Fixture {
    calls: u64,
}
fn write(value: Value, output: &mut [u8]) -> Result<u64> {
    let bytes = serde_json::to_vec(&value).map_err(|_| Error::InvalidArgument)?;
    if bytes.len() > output.len() {
        return Err(Error::BufferTooSmall);
    }
    output[..bytes.len()].copy_from_slice(&bytes);
    Ok(bytes.len() as u64)
}
impl Extension for Fixture {
    fn call(&mut self, operation: &str, input: &[u8], output: &mut [u8]) -> Result<u64> {
        let arguments: Value = serde_json::from_slice(input).map_err(|_| Error::InvalidArgument)?;
        if operation == "ssmcp.catalog.v1" {
            let mut tools = [
                "echo",
                "denied",
                "stats",
                "large",
                "business_error",
                "invalid_utf8",
                "oversized",
            ]
            .into_iter()
            .map(|name| {
                json!({
                    "name":name,"description":"Deterministic native fixture",
                    "inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},
                    "outputSchema":{"type":"object"},
                    "annotations":{"readOnlyHint":false,"idempotentHint":false},
                    "x-fixture":{"unicode":"原生","preserved":[1,true]}
                })
            })
            .collect::<Vec<_>>();
            if arguments["namespace"] == "duplicate" {
                tools.push(tools[0].clone());
            }
            return write(json!({"schema_version":1,"tools":tools}), output);
        }
        if operation == "invalid_utf8" {
            output[0] = 0xff;
            return Ok(1);
        }
        if operation == "oversized" {
            return Ok(output.len() as u64 + 1);
        }
        match operation {
            "stats" => write(
                json!({"content":[],"structuredContent":{"calls":self.calls}}),
                output,
            ),
            "echo" | "denied" => {
                self.calls += 1;
                write(
                    json!({"content":[{"type":"text","text":"native fixture reply"}],
                    "structuredContent":{"arguments":arguments,"calls":self.calls}}),
                    output,
                )
            }
            "large" => write(
                json!({"content":[{"type":"text","text":"x".repeat(80*1024)}]}),
                output,
            ),
            "business_error" => write(
                json!({"content":[{"type":"text","text":"fixture business error"}],"isError":true}),
                output,
            ),
            _ => Err(Error::InvalidArgument),
        }
    }
}
fn create() -> Result<Fixture> {
    Ok(Fixture { calls: 0 })
}
zplugin::export!(
    ExtensionExport<Fixture>,
    constructor = create,
    version = (1, 0, 0),
    unload = pinned
);

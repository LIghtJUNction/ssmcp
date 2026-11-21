//! Workspace, context and transfer state owned by the packaged files plugin.
use crate::{
    backend,
    client::{attachments, completion, references},
    context::Context,
    registry::Provider,
    rsync::Rsync,
    runtime::native_host::{Call, ConstructorContext, NativePlugin, PluginFuture},
    workspace::Workspace,
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct Files {
    workspace: Workspace,
    context: Context,
    rsync: Rsync,
}

impl Files {
    pub(crate) fn new(context: &ConstructorContext) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            workspace: Workspace::with_output(context.root.clone(), context.output.clone())?,
            context: Context::with_output(
                context.root.clone(),
                context.context_roots.clone(),
                context.output.clone(),
            )?,
            rsync: Rsync::new(context.root.clone())?,
        }))
    }

    // The local composer uses the same granted roots and state as remote calls.
    // These crate-private methods do not add another remotely callable surface.
    pub(crate) fn enable_default_skill_sources(&self) -> Result<(), String> {
        self.context.enable_default_skill_sources()
    }

    pub(crate) async fn prepare_references(
        &self,
        text: &str,
    ) -> Result<references::PreparedReferences, String> {
        references::prepare(text, &self.workspace, &self.context).await
    }

    #[cfg(test)]
    pub(crate) async fn expand_references(&self, text: &str) -> Result<String, String> {
        self.prepare_references(text)
            .await
            .map(|prepared| prepared.text)
    }

    pub(crate) async fn reference_completions(
        &self,
        key: &completion::Key,
    ) -> Result<completion::Listing, String> {
        completion::enumerate(&self.workspace, &self.context, key).await
    }

    pub(crate) async fn import_input(
        &self,
        source: attachments::Source,
    ) -> Result<attachments::Loaded, String> {
        attachments::load(source, &self.workspace).await
    }

    /// Resource responses retain their raw shape; Runtime applies MCP wrapping.
    pub(crate) async fn context_resource(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        self.context.perform(name, arguments).await
    }
}

impl NativePlugin for Files {
    const ID: &'static str = "files";

    fn providers(&self) -> &'static [Provider] {
        &[Provider::Workspace, Provider::Context, Provider::Rsync]
    }

    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![
            (Provider::Workspace, Workspace::tools()),
            (Provider::Context, Context::tools()),
            (Provider::Rsync, Rsync::tools()),
        ]
    }

    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move {
            let value = match call.provider {
                Provider::Workspace => {
                    self.workspace
                        .perform(&call.native_name, call.arguments)
                        .await?
                }
                Provider::Context => {
                    self.context
                        .perform(&call.native_name, call.arguments)
                        .await?
                }
                Provider::Rsync => {
                    self.rsync
                        .perform(&call.native_name, call.arguments)
                        .await?
                }
                provider => {
                    return Err(format!(
                        "provider {} does not belong to the files plugin",
                        provider.label()
                    ));
                }
            };
            if call.provider == Provider::Workspace && call.native_name == "ssmcp.fs.read_image" {
                // This backend already returns MCP image/text content blocks.
                Ok(value)
            } else {
                Ok(backend::result(value))
            }
        })
    }

    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            tokio::join!(self.workspace.shutdown(), self.rsync.shutdown());
            Ok(Value::Null)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        output::OutputStore,
        registry::Registry,
        runtime::native_host::{Definition, NativeHost},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        host: NativeHost,
        files: Arc<Files>,
        registry: Arc<Registry>,
        scratch: Scratch,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ssmcp-native-files-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let scratch = Scratch(path);
            let root = scratch.0.join("workspace");
            let extra = scratch.0.join("context");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&extra).unwrap();
            let registry = Registry::new();
            let files = Files::new(&ConstructorContext {
                root,
                context_roots: vec![extra],
                output: OutputStore::at_for_test(scratch.0.join("output")).unwrap(),
            })
            .unwrap();
            let host =
                NativeHost::new(registry.clone(), vec![Definition::new(files.clone())]).unwrap();
            for (provider, tools) in host.catalogs().unwrap() {
                registry.replace(provider, tools).unwrap();
            }
            Self {
                host,
                files,
                registry,
                scratch,
            }
        }

        async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
            self.host
                .call(&self.registry.lookup(name)?, arguments)
                .await
        }

        async fn shutdown(&self) {
            let report = tokio::time::timeout(Duration::from_secs(3), self.host.shutdown())
                .await
                .expect("files cleanup must not wait for a pending operation's timeout");
            assert_eq!(report, vec![("files".into(), Ok(Value::Null))]);
        }
    }

    #[tokio::test]
    async fn native_files_host_preserves_large_results_images_and_catalogs() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.host.catalogs().unwrap(),
            vec![
                (Provider::Workspace, Workspace::tools()),
                (Provider::Context, Context::tools()),
                (Provider::Rsync, Rsync::tools()),
            ]
        );
        // The real request exceeds the legacy extension's 64 KiB input limit;
        // the MCP text + structured result exceeds its 256 KiB result limit.
        let text = "native file payload\n".repeat(10_000);
        fixture
            .call(
                "ssmcp.fs.write_text",
                json!({"path":"large.txt","text":text}),
            )
            .await
            .unwrap();
        let read = fixture
            .call(
                "ssmcp.fs.read_text",
                json!({"path":"large.txt","max_bytes":crate::workspace::MAX_FILE_BYTES}),
            )
            .await
            .unwrap();
        assert_eq!(read["structuredContent"]["text"], text);
        assert!(read.to_string().len() > 256 * 1024);
        assert_eq!(
            serde_json::from_str::<Value>(read["content"][0]["text"].as_str().unwrap()).unwrap(),
            read["structuredContent"]
        );

        let image_path = fixture.scratch.0.join("workspace/image.png");
        image::RgbImage::new(2, 2)
            .save_with_format(&image_path, image::ImageFormat::Png)
            .unwrap();
        let bytes = std::fs::read(image_path).unwrap();
        let image = fixture
            .call("ssmcp.fs.read_image", json!({"path":"image.png"}))
            .await
            .unwrap();
        assert_eq!(image["content"][0]["type"], "image");
        assert_eq!(image["content"][0]["mimeType"], "image/png");
        assert_eq!(
            STANDARD
                .decode(image["content"][0]["data"].as_str().unwrap())
                .unwrap(),
            bytes
        );
        assert_eq!(image["structuredContent"]["path"], "image.png");
        assert_eq!(image["isError"], false);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn native_files_frontends_share_state_without_extending_context_grants() {
        let fixture = Fixture::new();
        fixture
            .call(
                "ssmcp.fs.write_text",
                json!({"path":"notes.txt","text":"shared workspace contents"}),
            )
            .await
            .unwrap();
        assert!(
            fixture
                .files
                .expand_references("@notes.txt")
                .await
                .unwrap()
                .contains("shared workspace contents")
        );
        #[cfg(not(unix))]
        let private_inputs = fixture.scratch.0.join("workspace/.ssmcp/inputs");
        #[cfg(not(unix))]
        assert_eq!(
            std::fs::symlink_metadata(&private_inputs).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let imported = fixture
            .files
            .import_input(attachments::Source::Text(
                "local attachment contents".into(),
            ))
            .await;
        #[cfg(unix)]
        {
            let attachments::Loaded::Reference { reference, .. } = imported.unwrap() else {
                panic!("text import must retain its ordinary workspace reference");
            };
            assert!(
                fixture
                    .files
                    .expand_references(&reference)
                    .await
                    .unwrap()
                    .contains("local attachment contents")
            );
        }
        #[cfg(not(unix))]
        {
            // Private paste storage deliberately requires the Unix implementation.
            let Ok(attachments::Loaded::RetainedText { text, error }) = imported else {
                panic!("unsupported private paste must retain the original text and warning");
            };
            assert_eq!(text.as_bytes(), b"local attachment contents");
            assert_eq!(
                error,
                concat!(
                    "Could not save the private input in .ssmcp/inputs: ",
                    "automatic private paste files are not supported on this platform yet; ",
                    "save the text inside the workspace and reference it with @path"
                )
            );
            // The preceding native write can create .ssmcp/managed-files.
            // Unsupported private paste must not create its own input storage.
            assert_eq!(
                std::fs::symlink_metadata(private_inputs).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
        fixture
            .files
            .reference_completions(&completion::Key::Files(".".into()))
            .await
            .unwrap();

        let skill = "---\nname: scoped-fixture\ndescription: Scoped fixture instructions\n---\nRead only the explicit source.\n";
        std::fs::write(fixture.scratch.0.join("context/SKILL.md"), skill).unwrap();
        let arguments = json!({"root":"context-1","path":"SKILL.md"});
        let raw = fixture
            .files
            .context_resource("ssmcp.skills.read", arguments.clone())
            .await
            .unwrap();
        assert_eq!(raw["content"], skill);
        assert!(raw.get("structuredContent").is_none());
        assert_eq!(
            fixture.call("ssmcp.skills.read", arguments).await.unwrap()["structuredContent"],
            raw
        );
        assert!(
            fixture
                .call("ssmcp.fs.read_text", json!({"path":"../context/SKILL.md"}),)
                .await
                .is_err()
        );
        let expanded = fixture
            .files
            .expand_references("@../context/SKILL.md")
            .await
            .unwrap();
        assert_eq!(
            crate::client::references::source(&expanded),
            "@../context/SKILL.md"
        );
        assert_eq!(crate::client::references::warnings(&expanded).len(), 1);
        let encoded = expanded
            .split_once("<ssmcp-references-v1>\n")
            .unwrap()
            .1
            .strip_suffix("\n</ssmcp-references-v1>")
            .unwrap();
        let payload: Value = serde_json::from_str(encoded).unwrap();
        let reference = &payload["references"][0];
        assert_eq!(reference["status"], "unavailable");
        assert!(reference.get("read").is_none());
        assert!(reference.get("preview").is_none());
        assert!(
            fixture
                .files
                .reference_completions(&completion::Key::Files("../context".into()))
                .await
                .is_err()
        );
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn native_files_cancellation_and_shutdown_release_owned_listener() {
        let fixture = Fixture::new();
        let listener = fixture.call("ssmcp.net.listen", json!({})).await.unwrap();
        let listener = &listener["structuredContent"];
        let address: std::net::SocketAddr = listener["address"].as_str().unwrap().parse().unwrap();
        assert!(
            std::net::TcpListener::bind(address).is_err(),
            "the files plugin must own the address before shutdown"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                fixture.call(
                    "ssmcp.net.accept",
                    json!({"id":listener["id"],"wait_ms":5000}),
                ),
            )
            .await
            .is_err()
        );
        fixture.shutdown().await;
        // Reclaim the exact address instead of timing a refused TCP connection.
        // No connections were established, so this does not depend on TIME_WAIT.
        let rebound = std::net::TcpListener::bind(address)
            .expect("files shutdown must release its listener address");
        assert_eq!(rebound.local_addr().unwrap(), address);
        assert!(
            fixture
                .files
                .reference_completions(&completion::Key::Files(".".into()))
                .await
                .unwrap_err()
                .contains("workspace is closed")
        );
        assert!(fixture.call("ssmcp.net.list", json!({})).await.is_err());
    }
}

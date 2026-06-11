//! Tests for compiled-in MCP resources exposed over the transport.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    AionforgeMcp, CLAUDE_CODE_CONFIG_RESOURCE_URI, CODEX_CONFIG_RESOURCE_URI,
    CURSOR_CONFIG_RESOURCE_URI, MCP_SURFACE_GUIDE_RESOURCE_URI, OPENCODE_CONFIG_RESOURCE_URI,
    RECALL_UNTRUSTED_DATA_PROMPT_RESOURCE_URI, TOOL_APPROVAL_POLICY_RESOURCE_URI,
    TOOL_MANIFEST_RESOURCE_URI,
};
use rmcp::ServiceExt;
use rmcp::model::ReadResourceRequestParams;

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
}

#[tokio::test]
async fn mcp_transport_lists_client_policy_resources() -> TestResult {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server = AionforgeMcp::new(memory());
    let server_handle = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = ().serve(client_transport).await?;
    let info = client.peer_info().expect("initialized server info");
    assert!(
        info.instructions
            .as_deref()
            .expect("server instructions")
            .contains(MCP_SURFACE_GUIDE_RESOURCE_URI),
        "instructions point clients at the guide resource"
    );

    let uris: BTreeSet<String> = client
        .list_all_resources()
        .await?
        .into_iter()
        .map(|resource| resource.raw.uri)
        .collect();

    for uri in [
        TOOL_MANIFEST_RESOURCE_URI,
        RECALL_UNTRUSTED_DATA_PROMPT_RESOURCE_URI,
        MCP_SURFACE_GUIDE_RESOURCE_URI,
        TOOL_APPROVAL_POLICY_RESOURCE_URI,
        CODEX_CONFIG_RESOURCE_URI,
        CLAUDE_CODE_CONFIG_RESOURCE_URI,
        OPENCODE_CONFIG_RESOURCE_URI,
        CURSOR_CONFIG_RESOURCE_URI,
    ] {
        assert!(uris.contains(uri), "{uri} listed in {uris:?}");
    }

    let manifest = read_text_resource(&client, TOOL_MANIFEST_RESOURCE_URI).await?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest)?;
    assert_eq!(manifest["schema"], "aionforge.mcp_tools.v1");
    assert_eq!(manifest["server"]["resource_count"].as_u64(), Some(8));
    assert_eq!(
        manifest["policy"]["read_like_approval"],
        "allow_without_prompt"
    );
    assert!(
        manifest["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "server_status"
                && tool["class"] == "read_like"
                && tool["approval"] == "allow_without_prompt")
    );
    assert!(
        manifest["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "forget"
                && tool["class"] == "mutating"
                && tool["approval"] == "ask_user"
                && tool["errors"]
                    .as_array()
                    .expect("errors")
                    .iter()
                    .any(|error| error == "ERR_NOT_FOUND"))
    );

    let codex = read_text_resource(&client, CODEX_CONFIG_RESOURCE_URI).await?;
    assert!(codex.contains("[mcp_servers.aionforge_memory]"));
    assert!(codex.contains("\"server_status\""));
    assert!(codex.contains("bearer_token_env_var = \"AIONFORGE_MCP_TOKEN\""));
    assert!(codex.contains("approval_mode = \"prompt\""));

    let policy = read_text_resource(&client, TOOL_APPROVAL_POLICY_RESOURCE_URI).await?;
    assert!(policy.contains("Read-like tools"));
    assert!(policy.contains("server_status"));
    assert!(policy.contains("Prompt-gated mutating tools"));
    assert!(policy.contains("ERR_CONSOLIDATE_BUSY"));

    client.cancel().await?;
    server_handle.await??;
    Ok(())
}

async fn read_text_resource(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    uri: &str,
) -> TestResult<String> {
    let resource = client
        .read_resource(ReadResourceRequestParams::new(uri))
        .await?;
    assert_eq!(resource.contents.len(), 1);
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } = &resource.contents[0]
    else {
        panic!("resource should be text");
    };
    Ok(text.clone())
}

//! Tests for compiled-in MCP resources exposed over the transport.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    AionforgeMcp, CLAUDE_CODE_CONFIG_RESOURCE_URI, CLIENT_OAUTH_GUIDE_RESOURCE_URI,
    CODEX_CONFIG_RESOURCE_URI, CURSOR_CONFIG_RESOURCE_URI, MCP_SURFACE_GUIDE_RESOURCE_URI,
    OPENCODE_CONFIG_RESOURCE_URI, PLUGIN_PACKAGE_GUIDE_RESOURCE_URI,
    RECALL_UNTRUSTED_DATA_PROMPT_RESOURCE_URI, TOOL_APPROVAL_POLICY_RESOURCE_URI,
    TOOL_MANIFEST_RESOURCE_URI,
};
use rmcp::ServiceExt;
use rmcp::model::ReadResourceRequestParams;

type TestError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;

const TOTAL_STATIC_RESOURCE_BUDGET_BYTES: usize = 17_408;

const RESOURCE_BODY_BUDGETS: &[(&str, usize)] = &[
    (TOOL_MANIFEST_RESOURCE_URI, 6_900),
    (RECALL_UNTRUSTED_DATA_PROMPT_RESOURCE_URI, 1_200),
    (MCP_SURFACE_GUIDE_RESOURCE_URI, 2_100),
    (TOOL_APPROVAL_POLICY_RESOURCE_URI, 1_664),
    (CLIENT_OAUTH_GUIDE_RESOURCE_URI, 2_000),
    (PLUGIN_PACKAGE_GUIDE_RESOURCE_URI, 2_300),
    (CODEX_CONFIG_RESOURCE_URI, 3_800),
    (CLAUDE_CODE_CONFIG_RESOURCE_URI, 512),
    (OPENCODE_CONFIG_RESOURCE_URI, 1_024),
    (CURSOR_CONFIG_RESOURCE_URI, 512),
];

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
        CLIENT_OAUTH_GUIDE_RESOURCE_URI,
        PLUGIN_PACKAGE_GUIDE_RESOURCE_URI,
        CODEX_CONFIG_RESOURCE_URI,
        CLAUDE_CODE_CONFIG_RESOURCE_URI,
        OPENCODE_CONFIG_RESOURCE_URI,
        CURSOR_CONFIG_RESOURCE_URI,
    ] {
        assert!(uris.contains(uri), "{uri} listed in {uris:?}");
    }
    assert_eq!(RESOURCE_BODY_BUDGETS.len(), uris.len());

    let mut total_resource_bytes = 0;
    for (uri, max_bytes) in RESOURCE_BODY_BUDGETS {
        let body = read_text_resource(&client, uri).await?;
        let body_bytes = body.len();
        total_resource_bytes += body_bytes;
        assert!(
            body_bytes <= *max_bytes,
            "{uri} exceeds resource body budget: {body_bytes} > {max_bytes}"
        );
    }
    assert!(
        total_resource_bytes <= TOTAL_STATIC_RESOURCE_BUDGET_BYTES,
        "compiled-in MCP resources exceed total budget: {total_resource_bytes} > {TOTAL_STATIC_RESOURCE_BUDGET_BYTES}"
    );

    let manifest = read_text_resource(&client, TOOL_MANIFEST_RESOURCE_URI).await?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest)?;
    assert_eq!(manifest["schema"], "aionforge.mcp_tools.v1");
    assert_eq!(
        manifest["server"]["resource_count"].as_u64(),
        Some(uris.len() as u64)
    );
    assert_eq!(
        manifest["policy"]["read_like_approval"],
        "allow_without_prompt"
    );
    assert_eq!(
        manifest["resources"]["plugin_guide"],
        PLUGIN_PACKAGE_GUIDE_RESOURCE_URI
    );
    let manifest_tools: BTreeSet<String> = manifest["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_string())
        .collect();
    let listed_tools = client.list_all_tools().await?;
    let listed_tool_names: BTreeSet<String> = listed_tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    assert_eq!(manifest_tools, listed_tool_names);
    let surface = read_text_resource(&client, MCP_SURFACE_GUIDE_RESOURCE_URI).await?;
    assert!(surface.contains("Keep the built-in HTTP server on loopback"));
    assert!(surface.contains("does not implement transport authentication"));

    let listed_tools_by_name: BTreeMap<String, _> = listed_tools
        .iter()
        .map(|tool| (tool.name.to_string(), tool))
        .collect();
    for manifest_tool in manifest["tools"].as_array().expect("tools") {
        let name = manifest_tool["name"].as_str().expect("tool name");
        let listed_tool = listed_tools_by_name.get(name).expect("listed tool");
        let annotations = listed_tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("{name} has annotations"));
        assert_eq!(
            annotations.read_only_hint,
            manifest_tool["read_only_hint"].as_bool()
        );
        assert_eq!(
            annotations.destructive_hint,
            manifest_tool["destructive_hint"].as_bool()
        );
        assert_eq!(
            annotations.idempotent_hint,
            manifest_tool["idempotent_hint"].as_bool()
        );
        assert_eq!(
            annotations.open_world_hint,
            manifest_tool["open_world_hint"].as_bool()
        );
    }
    let read_only_tool_names: BTreeSet<String> = listed_tools
        .iter()
        .filter(|tool| {
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                .unwrap_or(false)
        })
        .map(|tool| tool.name.to_string())
        .collect();
    assert!(read_only_tool_names.contains("search"));
    assert!(!read_only_tool_names.contains("capture"));
    assert!(
        manifest["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "server_status"
                && tool["class"] == "read_like"
                && tool["approval"] == "allow_without_prompt"
                && tool["read_only_hint"] == true
                && tool["open_world_hint"] == false)
    );
    assert!(
        manifest["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["name"] == "forget"
                && tool["class"] == "mutating"
                && tool["approval"] == "ask_user"
                && tool["destructive_hint"] == true
                && tool["idempotent_hint"] == true
                && tool["errors"]
                    .as_array()
                    .expect("errors")
                    .iter()
                    .any(|error| error == "ERR_NOT_FOUND"))
    );

    let codex = read_text_resource(&client, CODEX_CONFIG_RESOURCE_URI).await?;
    assert!(codex.contains("[mcp_servers.aionforge_memory]"));
    assert!(!codex.contains("aionforge_memory_plugin"));
    assert!(codex.contains("\"server_status\""));
    assert!(!codex.contains("bearer_token_env_var"));
    assert!(codex.contains("approval_mode = \"prompt\""));

    let policy = read_text_resource(&client, TOOL_APPROVAL_POLICY_RESOURCE_URI).await?;
    assert!(policy.contains("Read-like tools"));
    assert!(policy.contains("server_status"));
    assert!(policy.contains("Prompt-gated mutating tools"));
    assert!(policy.contains("ERR_CONSOLIDATE_BUSY"));

    let oauth = read_text_resource(&client, CLIENT_OAUTH_GUIDE_RESOURCE_URI).await?;
    assert!(oauth.contains("resource_metadata"));
    assert!(oauth.contains("codex mcp login aionforge_memory"));
    assert!(oauth.contains("does not validate OAuth tokens"));
    assert!(oauth.contains("omit Authorization headers"));

    let plugin = read_text_resource(&client, PLUGIN_PACKAGE_GUIDE_RESOURCE_URI).await?;
    assert!(plugin.contains("plugins/aionforge-memory"));
    assert!(plugin.contains("memory-loop"));
    assert!(plugin.contains("memory-recall"));
    assert!(plugin.contains("memory-capture"));
    assert!(plugin.contains("memory-maintenance"));
    assert!(plugin.contains("aionforge-memory-steward"));
    assert!(plugin.contains("memory-session"));
    assert!(plugin.contains("[mcp_servers.aionforge_memory]"));
    assert!(!plugin.contains("aionforge_memory_plugin"));

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

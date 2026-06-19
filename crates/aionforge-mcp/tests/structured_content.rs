//! Transport-level assertions for read-like MCP `structuredContent`.

mod common;

use aionforge_domain::ids::Id;
use aionforge_mcp::{
    AionforgeMcp, AuthEnabled, WorkCreateToolParams, capture_tool, work_create_tool,
};
use common::{capture_params, memory, now};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .map(|text| text.text.to_string())
        .unwrap_or_else(|| format!("{result:?}"))
}

fn object_args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().expect("tool args object").clone()
}

fn receipt_id(line: &str) -> String {
    line.split_whitespace()
        .nth(1)
        .expect("receipt id")
        .to_string()
}

#[tokio::test]
async fn read_like_transport_results_include_structured_content() -> TestResult {
    let memory = memory();
    let agent = Id::generate();
    let session = Id::generate();
    let mut capture = capture_params("structured transport memory", &agent.to_string());
    capture.session_id = Some(session.to_string());
    let capture_line = capture_tool(&memory, capture, &now(), None, AuthEnabled(false)).await?;
    let memory_id = receipt_id(&capture_line);
    let work_line = work_create_tool(
        &memory,
        WorkCreateToolParams {
            title: "Structured content transport test".to_string(),
            body: Some("Exercise work read DTOs".to_string()),
            level: "task".to_string(),
            parent_id: None,
            ordinal: None,
            target_namespace: None,
            viewer: Some(format!("agent:{agent}")),
            principal: None,
            teams: Vec::new(),
        },
        &now(),
        None,
        AuthEnabled(false),
    )?;
    let work_id = receipt_id(&work_line);

    let (server_transport, client_transport) = tokio::io::duplex(32 * 1024);
    let server = AionforgeMcp::new(memory);
    let server_handle = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = ().serve(client_transport).await?;
    let principal = serde_json::json!({
        "agent_id": agent.to_string(),
        "teams": [],
    });
    let calls = [
        (
            "server_status",
            serde_json::json!({ "verbose": true }),
            "aionforge.server_status.v1",
            "[server] ",
        ),
        (
            "consolidation_status",
            serde_json::json!({ "verbose": true }),
            "aionforge.consolidation_status.v1",
            "[consolidation] ",
        ),
        (
            "search",
            serde_json::json!({
                "query": "structured transport memory",
                "principal": principal.clone(),
            }),
            "aionforge.search_results.v1",
            "hits: ",
        ),
        (
            "read_memory",
            serde_json::json!({
                "memory_ids": [memory_id],
                "principal": principal.clone(),
                "verbose": true,
            }),
            "aionforge.read_memory.v1",
            "[read_memory] ",
        ),
        (
            "session_manifest",
            serde_json::json!({
                "session_id": session.to_string(),
                "principal": principal.clone(),
            }),
            "aionforge.session_manifest.v1",
            "[session_manifest] ",
        ),
        (
            "memory_census",
            serde_json::json!({
                "principal": principal.clone(),
            }),
            "aionforge.memory_census.v1",
            "[memory_census] ",
        ),
        (
            "audit_history",
            serde_json::json!({
                "kind": "capture",
                "principal": principal.clone(),
                "limit": 5,
                "verbose": true,
            }),
            "aionforge.audit_history.v1",
            "[audit] ",
        ),
        (
            "work_query",
            serde_json::json!({
                "work_status": "todo",
                "principal": principal.clone(),
            }),
            "aionforge.work_query.v1",
            "[work_query] ",
        ),
        (
            "work_tree",
            serde_json::json!({
                "root_id": work_id,
                "principal": principal.clone(),
            }),
            "aionforge.work_tree.v1",
            "[work_tree] ",
        ),
    ];

    for (tool, args, schema, text_prefix) in calls {
        let result = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(object_args(args)))
            .await?;
        let text = first_text(&result);
        assert!(
            text.starts_with(text_prefix),
            "{tool} preserves compact text output: {text}"
        );
        let structured = result
            .structured_content
            .as_ref()
            .unwrap_or_else(|| panic!("{tool} has structuredContent"));
        assert_eq!(
            structured.get("schema").and_then(serde_json::Value::as_str),
            Some(schema),
            "{tool} schema-bearing structuredContent: {structured}"
        );
    }

    client.cancel().await?;
    server_handle.await??;
    Ok(())
}

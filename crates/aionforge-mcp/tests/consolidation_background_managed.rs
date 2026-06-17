//! Background-managed consolidation posture disables the foreground MCP tool.

use aionforge_mcp::AionforgeMcp;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;

mod common;

use common::memory;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn background_managed_transport_rejects_foreground_consolidate_tool() -> TestResult {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server = AionforgeMcp::new_with_auth_and_consolidation(memory(), false, true);
    let server_handle = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = ().serve(client_transport).await?;
    let result = client
        .call_tool(
            CallToolRequestParams::new("consolidate").with_arguments(
                serde_json::json!({
                    "max_ticks": 1,
                    "verbose": false,
                })
                .as_object()
                .expect("object")
                .clone(),
            ),
        )
        .await;
    let text = match result {
        Ok(result) => result
            .content
            .first()
            .and_then(|content| content.raw.as_text())
            .map(|text| text.text.to_string())
            .unwrap_or_else(|| format!("{result:?}")),
        Err(error) => error.to_string(),
    };

    assert!(
        text.contains("ERR_CONSOLIDATE_MANAGED"),
        "background-managed serve posture rejects foreground consolidation: {text}"
    );

    client.cancel().await?;
    server_handle.await??;
    Ok(())
}

//! Protocol-level MCP resource and subscription handlers.

use aionforge_domain::contracts::Embedder;
use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, GetPromptResult, Implementation,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
    ServerInfo, SubscribeRequestMethod, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::RequestContext;
use rmcp::{ServerHandler, prompt_handler, tool_handler};
use tracing::Instrument;

use crate::{AionforgeMcp, message, resources, room, server, validated_principal_from_extensions};

#[tool_handler]
#[prompt_handler]
impl<E: Embedder + 'static> ServerHandler for AionforgeMcp<E> {
    /// One tracing span per MCP tool call with only bounded, non-sensitive fields.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool = request.name.clone();
        let authenticated = validated_principal_from_extensions(&context.extensions).is_some();
        let span = tracing::info_span!(
            "aionforge.mcp.tool",
            tool = %tool,
            authenticated,
            outcome = tracing::field::Empty,
            error = tracing::field::Empty,
            latency_ms = tracing::field::Empty,
        );
        let started = std::time::Instant::now();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).instrument(span.clone()).await;
        span.record("latency_ms", started.elapsed().as_millis() as u64);
        let (outcome, error) = tool_span_outcome(&result);
        span.record("outcome", outcome);
        span.record("error", error);
        result
    }

    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources();
        if self.heartbeats_enabled {
            capabilities = capabilities.enable_resources_subscribe();
        }
        ServerInfo::new(capabilities.build())
            .with_server_info(Implementation::new(
                crate::surface::SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(server::SERVER_INSTRUCTIONS.to_string())
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListResourcesResult::with_all_items(
            resources::list_static_resources(),
        )))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_
    {
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(vec![
            room::resource_template(),
        ])))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri;
        if !uri.starts_with(room::ROOM_URI_PREFIX) {
            return resources::read_static_resource(&uri)
                .map(|resource| ReadResourceResult::new(vec![resource]))
                .ok_or_else(|| resource_not_found(&uri));
        }
        let room = room::parse_room_uri(&uri).map_err(|_| resource_not_found(&uri))?;
        let extension = validated_principal_from_extensions(&context.extensions);
        match room::read_room_resource(&self.memory, &room, extension, self.auth_enabled()) {
            Ok(Some(text)) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("text/plain"),
            ])),
            Ok(None) => {
                if self.auth_enabled().is_enabled() {
                    self.room_subs.unsubscribe(
                        &room.room_id.to_string(),
                        &uri,
                        &self.session_marker,
                    );
                }
                Err(resource_not_found(&uri))
            }
            Err(_) => Err(resource_not_found(&uri)),
        }
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        if !self.heartbeats_enabled {
            return Err(McpError::method_not_found::<SubscribeRequestMethod>());
        }
        let uri = request.uri;
        let room = room::parse_room_uri(&uri)
            .map_err(|_| McpError::invalid_params("not a room resource URI", None))?;
        let extension = validated_principal_from_extensions(&context.extensions);
        let principal = room::resolve_room_reader(&room, extension, self.auth_enabled())
            .map_err(|_| McpError::method_not_found::<SubscribeRequestMethod>())?;
        self.room_subs
            .subscribe(
                &room.room_id.to_string(),
                uri,
                &self.session_marker,
                context.peer.clone(),
                message::visible_recipients(&principal),
                self.room_bounds,
            )
            .map_err(|_| McpError::internal_error("room subscription limit reached", None))
    }

    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        let uri = request.uri;
        if let Ok(room) = room::parse_room_uri(&uri) {
            self.room_subs
                .unsubscribe(&room.room_id.to_string(), &uri, &self.session_marker);
        }
        std::future::ready(Ok(()))
    }
}

fn resource_not_found(uri: &str) -> McpError {
    McpError::resource_not_found(
        "resource not found",
        Some(serde_json::json!({ "uri": uri })),
    )
}

fn tool_span_outcome(result: &Result<CallToolResult, McpError>) -> (&'static str, &'static str) {
    match result {
        Ok(call) if call.is_error == Some(true) => ("error", "tool_error"),
        Ok(_) => ("success", "none"),
        Err(_) => ("error", "dispatch_error"),
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolRequestMethod, CallToolResult};

    use super::{McpError, tool_span_outcome};

    #[test]
    fn tool_span_outcome_classifies_success_tool_error_and_dispatch_error() {
        assert_eq!(
            tool_span_outcome(&Ok(CallToolResult::success(vec![]))),
            ("success", "none"),
        );
        assert_eq!(
            tool_span_outcome(&Ok(CallToolResult::error(vec![]))),
            ("error", "tool_error"),
        );
        let dispatch: Result<CallToolResult, McpError> =
            Err(McpError::method_not_found::<CallToolRequestMethod>());
        assert_eq!(tool_span_outcome(&dispatch), ("error", "dispatch_error"));
    }
}

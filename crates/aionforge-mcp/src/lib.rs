//! Optional Model Context Protocol server surface for Aionforge Memory.
//!
//! The server exposes capture, recall, inspection, lifecycle, work-tracking, and
//! addressed-message tools over stdio or Streamable HTTP, backed by the [`Memory`]
//! facade. Output is compact by default, every request is resolved against a validated
//! or explicitly supplied principal, and recalled content is rendered as untrusted
//! third-party data. The server never requests sampling from the caller's model.
//! Prompts and resources publish the matching security guidance, client configuration,
//! tool manifest, and approval posture.

mod auth_validator;
mod census;
mod handler;
mod http_body_limit;
mod http_transport;
mod inspect;
mod lifecycle;
mod lifecycle_output;
mod mapper;
mod message;
#[cfg(test)]
mod message_tests;
mod notify;
mod principal;
mod prompt;
mod render;
mod resources;
mod room;
mod room_subs;
mod server;
mod status;
mod stdio;
mod structured;
mod surface;
mod telemetry;
mod tools;
mod traffic;
mod validated;
mod work;

pub use auth_validator::{AuthValidators, AuthValidatorsError};
pub use census::{MemoryCensusCursorToolParam, MemoryCensusToolParams, memory_census_tool};
pub use http_body_limit::{DEFAULT_MAX_REQUEST_BODY_BYTES, RequestBodyLimitService};
pub use http_transport::{
    AionforgeStreamableHttpService, OAUTH_PROTECTED_RESOURCE_WELL_KNOWN_PREFIX,
    OAuthProtectedResourceMetadata, STREAMABLE_HTTP_ENDPOINT, StreamableHttpConfigError,
    StreamableHttpOptions, oauth_protected_resource_well_known_path, streamable_http_config,
    streamable_http_service, streamable_http_service_with_consolidation,
    streamable_http_service_with_consolidation_and_message_wait,
};
pub use inspect::{
    ReadMemoryToolParams, SessionManifestCursorToolParam, SessionManifestToolParams,
    read_memory_tool, session_manifest_tool,
};
pub use lifecycle::{
    AuditCursorToolParam, AuditHistoryToolParams, ConsolidationRunToolParams,
    ConsolidationStatusToolParams, MemoryLifecycleToolParams, audit_history_tool, consolidate_tool,
    consolidation_status_tool, forget_tool, pin_tool, unforget_tool, unpin_tool,
};
pub use mapper::{MapError, TokenClass, WritePosture, map_verified_claims_to_principal};
pub use message::{
    MessageAckToolParams, MessagePollCursorToolParam, MessagePollToolParams, MessageSendToolParams,
    MessageWaitToolParams, message_ack_tool, message_poll_tool, message_send_tool,
    message_wait_tool,
};
pub use notify::MessageWaitBounds;
pub use principal::{AuthEnabled, HostPrincipalToolParam};
pub use prompt::{
    RECALL_UNTRUSTED_DATA_PROMPT, RECALL_UNTRUSTED_DATA_PROMPT_NAME,
    RECALL_UNTRUSTED_DATA_PROMPT_RESOURCE_URI, RECALL_WRAPPER_TAG,
};
pub use resources::{
    CLAUDE_CODE_CONFIG_RESOURCE_URI, CLIENT_OAUTH_GUIDE_RESOURCE_URI, CODEX_CONFIG_RESOURCE_URI,
    CURSOR_CONFIG_RESOURCE_URI, MCP_SURFACE_GUIDE_RESOURCE_URI, OPENCODE_CONFIG_RESOURCE_URI,
    PLUGIN_PACKAGE_GUIDE_RESOURCE_URI, TOOL_APPROVAL_POLICY_RESOURCE_URI,
    TOOL_MANIFEST_RESOURCE_URI,
};
pub use room_subs::RoomSubscribeBounds;
pub use status::{
    AuthPosture, ServerStatusToolParams, build_sha, build_status, build_timestamp,
    server_status_tool,
};
pub use stdio::{
    serve_stdio, serve_stdio_with_consolidation, serve_stdio_with_consolidation_and_message_wait,
};
pub use tools::{
    BatchCaptureItem, BatchCaptureToolParams, CaptureToolParams, MAX_BATCH_ITEMS, SearchToolParams,
    batch_capture_tool, capture_tool, search_tool,
};
pub use traffic::{
    DEFAULT_HEARTBEAT_INTERVAL as DEFAULT_TRAFFIC_HEARTBEAT_INTERVAL,
    log_totals as log_traffic_totals, run_heartbeat as run_traffic_heartbeat,
};
pub use validated::{ValidatedPrincipal, validated_principal_from_extensions};
pub use work::{
    WorkAdvanceToolParams, WorkCreateToolParams, WorkLinkToolParams, WorkQueryToolParams,
    WorkTreeToolParams, work_advance_tool, work_create_tool, work_link_tool, work_query_tool,
    work_tree_tool,
};

use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_engine::Memory;
use rmcp::RoleServer;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router};

/// The MCP server handler over a shared [`Memory`].
pub struct AionforgeMcp<E> {
    memory: Arc<Memory<E>>,
    // The OAuth resource-server posture. Its `enabled` flag is threaded into every identity
    // resolver (`false`, the default via [`AionforgeMcp::new`], reproduces today's body-only
    // behavior; `true`, via [`AionforgeMcp::new_with_auth`], requires a validated request
    // extension), and the issuer origins ride `server_status` for posture reporting (never a
    // secret). PR4 shipped dark — no caller set it enabled — so runtime behavior was unchanged
    // until PR5's validator layer flips it on.
    auth: AuthPosture,
    background_managed: bool,
    consolidation_lock: Arc<tokio::sync::Mutex<()>>,
    notifier: Arc<notify::MessageNotifier>,
    wait_bounds: MessageWaitBounds,
    room_subs: Arc<room_subs::RoomSubscriptions>,
    room_bounds: RoomSubscribeBounds,
    session_marker: Arc<room_subs::SessionMarker>,
    heartbeats_enabled: bool,
    // Used by the rmcp-generated `#[tool_handler]` impl; the macro expansion hides the
    // read from the dead-code analyzer.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    // Used by the rmcp-generated `#[prompt_handler]` impl; the macro expansion hides the
    // read from the dead-code analyzer.
    #[allow(dead_code)]
    prompt_router: PromptRouter<Self>,
}

#[tool_router]
impl<E: Embedder + 'static> AionforgeMcp<E> {
    #[tool(
        description = "Report version, counts, transports, auth/sampling posture, tool classes, and resources; read-only diagnostic.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn server_status(
        &self,
        params: Parameters<ServerStatusToolParams>,
    ) -> Result<CallToolResult, String> {
        let counts = self
            .memory
            .memory_counts()
            .map_err(|e| format!("ERR_SERVER_STATUS {e}"))?;
        let work_counts = self
            .memory
            .store()
            .work_counts()
            .map_err(|e| format!("ERR_SERVER_STATUS {e}"))?;
        Ok(structured::call_tool_result(
            status::server_status_tool_output(
                resources::static_resource_count(),
                counts,
                work_counts,
                params.0,
                &self.auth,
            ),
        ))
    }

    #[tool(
        description = "Persist one event after filtering, dedupe, and embedding; team target needs asserted teams; errors ERR_CAPTURE/ERR_INVALID_*.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn capture(
        &self,
        params: Parameters<CaptureToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        capture_tool(&self.memory, params.0, &now, extension, self.auth_enabled()).await
    }

    #[tool(
        description = "Persist 1..=64 events under one writer; per-item failures return ERR_ITEM[i], with ERR_EMPTY_BATCH/ERR_BATCH_TOO_LARGE call errors.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn batch_capture(
        &self,
        params: Parameters<BatchCaptureToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let params = params.0;
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        batch_capture_tool(&self.memory, params, &now, extension, self.auth_enabled()).await
    }

    #[tool(
        description = "Search visible memories for viewer/principal; returns capped snippets in recalled-memory-context; validates fanout/min_relevance.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn search(
        &self,
        params: Parameters<SearchToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        // The host boundary owns the wall clock, mirroring `capture`: stamping the recall
        // instant here keeps the substrate free of an ambient clock while making the
        // importance and recency re-ranks available to every MCP search — each query class
        // still decides whether it weights them; the quote class keeps both off (05 §2,
        // M5.T01).
        let params = params.0;
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        tools::search_tool_output(&self.memory, params, &now, extension, self.auth_enabled())
            .await
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "Read 1..=16 visible ids; assert teams for team ids; full=true is untruncated, verbose is wider; ERR_TOO_MANY_IDS.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn read_memory(
        &self,
        params: Parameters<ReadMemoryToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        inspect::read_memory_tool_output(&self.memory, params.0, extension, self.auth_enabled())
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "List visible captures for one session with after/next pagination; sessionless captures excluded; max 200.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn session_manifest(
        &self,
        params: Parameters<SessionManifestToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        inspect::session_manifest_tool_output(
            &self.memory,
            params.0,
            extension,
            self.auth_enabled(),
        )
        .map(structured::call_tool_result)
    }

    #[tool(
        description = "Send a recall-excluded message to agent:<uuid> or an authorized team:<name>; sender identity is authenticated; errors ERR_INVALID_RECIPIENT/ERR_NOT_AUTHORIZED.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn message_send(
        &self,
        params: Parameters<MessageSendToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        let (output, emit) = message::message_send_tool_output(
            &self.memory,
            &self.notifier,
            params.0,
            &now,
            extension,
            self.auth_enabled(),
        )?;
        let result = structured::call_tool_result(output);
        if let Some(emit) = emit {
            let room_subs = Arc::clone(&self.room_subs);
            tokio::spawn(async move {
                room_subs
                    .notify_room(&emit.room_id.to_string(), &emit.recipient)
                    .await;
            });
        }
        Ok(result)
    }

    #[tool(
        description = "Poll visible private/team message inboxes in ascending keyset order; pure read, optional room/unread filters, max 200.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn message_poll(
        &self,
        params: Parameters<MessagePollToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        message::message_poll_tool_output(&self.memory, params.0, extension, self.auth_enabled())
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "Bounded wait; recipient over-cap polls once with timed_out=true, returns pending mail, never parks or errors.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn message_wait(
        &self,
        params: Parameters<MessageWaitToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        let heartbeat = notify::message_wait_heartbeat_sink(
            &context,
            self.wait_bounds,
            self.heartbeats_enabled,
        );
        message::message_wait_tool_output(
            &self.memory,
            &self.notifier,
            params.0,
            extension,
            self.auth_enabled(),
            self.wait_bounds,
            heartbeat,
        )
        .await
        .map(structured::call_tool_result)
    }

    #[tool(
        description = "Advance 1..=64 visible messages to read or acked with guarded per-id CAS outcomes and audit.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn message_ack(
        &self,
        params: Parameters<MessageAckToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        message::message_ack_tool_output(
            &self.memory,
            params.0,
            &now,
            extension,
            self.auth_enabled(),
        )
        .map(structured::call_tool_result)
    }

    #[tool(
        description = "Report consolidation backlog counts, oldest pending age, graph generation, and optional hint; errors ERR_CONSOLIDATION_STATUS.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn consolidation_status(
        &self,
        params: Parameters<ConsolidationStatusToolParams>,
    ) -> Result<CallToolResult, String> {
        let now = jiff::Zoned::now();
        lifecycle::consolidation_status_tool_output(&self.memory, params.0, &now)
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "Run bounded foreground consolidation (max_ticks<=5); ERR_CONSOLIDATE_MANAGED if background-owned, ERR_CONSOLIDATE_BUSY if locked.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn consolidate(
        &self,
        params: Parameters<ConsolidationRunToolParams>,
    ) -> Result<String, String> {
        if self.background_managed {
            return Err(
                "ERR_CONSOLIDATE_MANAGED: consolidation is managed by the background loop"
                    .to_string(),
            );
        }
        let Ok(_guard) = self.consolidation_lock.try_lock() else {
            return Err(
                "ERR_CONSOLIDATE_BUSY: another foreground consolidation run is active".to_string(),
            );
        };
        consolidate_tool(&self.memory, params.0).await
    }

    #[tool(
        description = "Soft-forget one writable memory id for the viewer/principal; may return ERR_NOT_FOUND or disabled-forgetting outcome.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn forget(
        &self,
        params: Parameters<MemoryLifecycleToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let params = params.0;
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        forget_tool(&self.memory, params, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Restore one soft-forgotten writable memory id for the viewer/principal; same team gate as forget, ERR_NOT_FOUND on miss.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn unforget(
        &self,
        params: Parameters<MemoryLifecycleToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let params = params.0;
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        unforget_tool(&self.memory, params, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Pin one writable memory id so decay and forgetting spare it; same viewer/team gate as forget, ERR_NOT_FOUND on miss.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pin(
        &self,
        params: Parameters<MemoryLifecycleToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let params = params.0;
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        pin_tool(&self.memory, params, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Unpin one writable memory id so decay and forgetting resume; same viewer/team gate as pin, ERR_NOT_FOUND on miss.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn unpin(
        &self,
        params: Parameters<MemoryLifecycleToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let params = params.0;
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        unpin_tool(&self.memory, params, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Read visible audit rows by subject_id, snake_case kind, or both; max 50; ERR_INVALID_AUDIT_QUERY if neither is supplied.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn audit_history(
        &self,
        params: Parameters<AuditHistoryToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let params = params.0;
        let extension = validated_principal_from_extensions(&context.extensions);
        lifecycle::audit_history_tool_output(&self.memory, params, extension, self.auth_enabled())
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "Create a work item in private or authorized team namespace; parent must share namespace; errors ERR_NOT_AUTHORIZED/ERR_WORK_PARENT_*.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn work_create(
        &self,
        params: Parameters<WorkCreateToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        work_create_tool(&self.memory, params.0, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Advance a work item's status with optional expected_from CAS; audited; errors ERR_WORK_STATE_CONFLICT or ERR_NOT_FOUND.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn work_advance(
        &self,
        params: Parameters<WorkAdvanceToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        work_advance_tool(&self.memory, params.0, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Attach a HAS_TAG classification to a writable work item, minting the tag on first use; ERR_INVALID_SLUG/ERR_NOT_FOUND.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn work_link(
        &self,
        params: Parameters<WorkLinkToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let now = jiff::Zoned::now();
        let extension = validated_principal_from_extensions(&context.extensions);
        work_link_tool(&self.memory, params.0, &now, extension, self.auth_enabled())
    }

    #[tool(
        description = "Read a visible work item's subtree as recalled-memory-context; depth defaults to 3 and caps at 8; ERR_NOT_FOUND on miss.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn work_tree(
        &self,
        params: Parameters<WorkTreeToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        work::work_tree_tool_output(&self.memory, params.0, extension, self.auth_enabled())
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "Query visible work items by work_status and/or level; max 200; ERR_WORK_QUERY if no filter is supplied.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn work_query(
        &self,
        params: Parameters<WorkQueryToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        work::work_query_tool_output(&self.memory, params.0, extension, self.auth_enabled())
            .map(structured::call_tool_result)
    }

    #[tool(
        description = "Count or list visible memories by namespace; list mode is paginated.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_census(
        &self,
        params: Parameters<MemoryCensusToolParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, String> {
        let extension = validated_principal_from_extensions(&context.extensions);
        census::memory_census_tool_output(&self.memory, params.0, extension, self.auth_enabled())
            .map(structured::call_tool_result)
    }
}

//! Stdio transport entry points.

use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_engine::Memory;
use rmcp::ServiceExt;

use crate::notify::MessageNotifier;
use crate::room_subs::RoomSubscriptions;
use crate::{AionforgeMcp, MessageWaitBounds, RoomSubscribeBounds};

/// Serve the MCP surface over stdio until the peer disconnects.
///
/// `auth_enabled` selects the OAuth resource-server posture: `false` reproduces body-only local
/// identity, while `true` requires a validated request extension on every identity-resolving tool.
/// Stdio has no HTTP validator, so auth-enabled stdio fails closed with `ERR_PRINCIPAL_REQUIRED`
/// until a stdio-side validated-principal producer exists.
///
/// # Errors
/// Returns an error if the transport cannot be established or the service fails while running.
pub async fn serve_stdio<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    auth_enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_stdio_with_consolidation(memory, auth_enabled, false).await
}

/// Serve over stdio with auth and background-consolidation posture.
///
/// `background_managed` must match the host's serve-owned background consolidation loop. When
/// true, the foreground `consolidate` tool returns `ERR_CONSOLIDATE_MANAGED` so it cannot race the
/// background cursor writer.
///
/// # Errors
/// Returns an error if the transport cannot be established or the service fails while running.
pub async fn serve_stdio_with_consolidation<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    auth_enabled: bool,
    background_managed: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_stdio_with_consolidation_and_message_wait(
        memory,
        auth_enabled,
        background_managed,
        MessageWaitBounds::default(),
        RoomSubscribeBounds::default(),
    )
    .await
}

/// Serve over stdio with configured message-wait and room-subscription bounds.
///
/// Stdio owns one handler, so its constructor-owned notifier is shared by every request.
///
/// # Errors
/// Returns an error if the transport cannot be established or the service fails while running.
pub async fn serve_stdio_with_consolidation_and_message_wait<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
    auth_enabled: bool,
    background_managed: bool,
    wait_bounds: MessageWaitBounds,
    room_bounds: RoomSubscribeBounds,
) -> Result<(), Box<dyn std::error::Error>> {
    let service =
        AionforgeMcp::new_with_auth_consolidation_notifier_and_message_wait_and_room_subscriptions(
            memory,
            auth_enabled,
            background_managed,
            Arc::new(MessageNotifier::default()),
            wait_bounds,
            Arc::new(RoomSubscriptions::default()),
            room_bounds,
        )
        .serve(rmcp::transport::io::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

//! Optional Model Context Protocol server surface for Aionforge Memory.
//!
//! The M1 smoke exposes two Tools over stdio — `capture` and `search` — backed by the
//! [`Memory`] facade. Output is compact by default to keep an agent's context small,
//! captures are confined to the writer's private namespace, and searches are
//! authorized against a caller-supplied viewer namespace. The server is a pure tool
//! provider: it never requests sampling from the caller's model. Resources and Prompts
//! round out the surface in a later milestone.
//!
//! The tool logic lives in a private module, exposed as [`capture_tool`] and
//! [`search_tool`] so it can be tested without the transport; this module is the rmcp
//! wiring on top.

mod tools;

pub use tools::{CaptureToolParams, SearchToolParams, capture_tool, search_tool};

use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_engine::Memory;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};

/// The MCP server handler over a shared [`Memory`].
pub struct AionforgeMcp<E> {
    memory: Arc<Memory<E>>,
    // Used by the rmcp-generated `#[tool_handler]` impl; the macro expansion hides the
    // read from the dead-code analyzer.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

// A manual `Clone` so the handler does not require `E: Clone` (the memory is shared
// behind an `Arc`).
impl<E> Clone for AionforgeMcp<E> {
    fn clone(&self) -> Self {
        Self {
            memory: Arc::clone(&self.memory),
            tool_router: self.tool_router.clone(),
        }
    }
}

#[tool_router]
impl<E: Embedder + 'static> AionforgeMcp<E> {
    /// Build a handler over a shared memory.
    #[must_use]
    pub fn new(memory: Arc<Memory<E>>) -> Self {
        Self {
            memory,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Capture a memory: filter, deduplicate, embed, and commit one event. Returns a compact receipt line."
    )]
    async fn capture(&self, params: Parameters<CaptureToolParams>) -> Result<String, String> {
        let now = jiff::Zoned::now();
        capture_tool(&self.memory, params.0, &now).await
    }

    #[tool(
        description = "Search memories. Returns compact one-line hits (id, score, snippet); pass verbose for per-hit detail."
    )]
    async fn search(&self, params: Parameters<SearchToolParams>) -> Result<String, String> {
        // The host boundary owns the wall clock, mirroring `capture`: stamping the recall
        // instant here keeps the substrate free of an ambient clock while making the
        // importance and recency re-ranks available to every MCP search — each query class
        // still decides whether it weights them; the quote class keeps both off (05 §2,
        // M5.T01).
        let now = jiff::Zoned::now();
        search_tool(&self.memory, params.0, &now).await
    }
}

#[tool_handler]
impl<E: Embedder + 'static> ServerHandler for AionforgeMcp<E> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Aionforge Memory MCP server. capture writes a memory; search recalls memories as \
             compact, third-party-data-tagged results. The server never requests sampling from \
             your model."
                .to_string(),
        )
    }
}

/// Serve the MCP surface over stdio until the peer disconnects.
///
/// # Errors
/// Returns an error if the transport cannot be established or the service fails while
/// running.
pub async fn serve_stdio<E: Embedder + 'static>(
    memory: Arc<Memory<E>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = AionforgeMcp::new(memory)
        .serve(rmcp::transport::io::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

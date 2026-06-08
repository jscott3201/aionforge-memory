//! Procedural memory: skills stored as data, with versioning, reliability, and
//! reliability-weighted retrieval (02 §4.4, 05; M3.T04).
//!
//! [`ProceduralMemoryService`] is the layer-2 engine over the L0 versioned-skill surface. It
//! owns the policies the store leaves to its caller: it assigns each save a monotonic version
//! per name, deprecates the prior active version (deprecate-never-delete), constructs the
//! version-diff audit trail, and — on retrieval — fuses the problem-embedding and description
//! signals and re-weights them by a Beta-posterior reliability so a proven skill outranks an
//! unproven one of equal problem match. It is generic over the [`Embedder`](aionforge_domain::Embedder)
//! seam, so it names the contract, not the concrete HTTP client.
//!
//! Two write-path policies keep the history clean and honest:
//!
//! - **Change detection.** Re-saving a skill whose body and frozen contract surface
//!   (capabilities, params, pre/post-conditions, language) all match the active version is a
//!   no-op, so an agent re-registering its skill set never churns the version history. A change
//!   to any of those — including the capabilities frozen per version — does cut a new, audited
//!   version.
//! - **Reliability is earned.** A new version starts from zero outcomes; it never inherits the
//!   prior version's success record, because a changed body is a different procedure whose
//!   reliability must be proven again.

mod clock;
mod config;
mod error;
mod memory;
mod ranking;

pub use clock::{Clock, SystemClock};
pub use config::ProceduralConfig;
pub use error::ProceduralError;
pub use memory::ProceduralMemoryService;

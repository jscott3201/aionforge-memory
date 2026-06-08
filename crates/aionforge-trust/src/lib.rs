//! Namespaces, CRDT merge, Ed25519 provenance, attestation/quorum promotion, trust scoring, and the audit subgraph.

pub mod gate;
pub mod resolver;
pub mod signing;

pub use gate::{SignedWriteGate, SystemWallClock};
pub use resolver::StoreKeyResolver;
pub use signing::Ed25519Verifier;

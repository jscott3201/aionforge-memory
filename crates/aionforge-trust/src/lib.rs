//! Namespaces, CRDT merge, Ed25519 provenance, attestation/quorum promotion, trust scoring, and the audit subgraph.

pub mod attest_gate;
pub mod gate;
pub mod promoter;
pub mod resolver;
pub mod signing;

pub use attest_gate::{AttestError, AttestRejection, AttestationGate};
pub use gate::{SignedWriteGate, SystemWallClock};
pub use promoter::{
    AttestReceipt, AttestRequest, CategoryRule, DemotionOutcome, Promoter, PromotionError,
    PromotionOutcome, PromotionPolicy,
};
pub use resolver::StoreKeyResolver;
pub use signing::Ed25519Verifier;

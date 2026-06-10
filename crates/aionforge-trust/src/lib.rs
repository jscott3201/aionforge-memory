//! Namespaces, CRDT merge, Ed25519 provenance, attestation/quorum promotion, trust scoring, and the audit subgraph.

pub mod attest_gate;
pub mod audit_custody;
pub mod audit_keyring;
pub mod audit_provision;
mod audit_rotation;
pub mod audit_signer;
pub mod audit_verifier;
pub mod gate;
pub mod promoter;
pub mod reliability;
pub mod reliability_scorer;
pub mod resolver;
pub mod signing;
mod system_audit;

pub use attest_gate::{AttestError, AttestRejection, AttestationGate};
pub use audit_custody::{
    CustodyError, SeedSource, ensure_audit_dir, load_audit_seed, resolve_audit_signer,
};
pub use audit_keyring::{AuditKeyring, KeyringEntry, KeyringError, keyring_path};
pub use audit_provision::{AuditProvision, ProvisionError, provision_audit_signing};
pub use audit_rotation::{genesis_rotation, rotate_key};
pub use audit_signer::{AuditSigner, KeyError, SecretSeed};
pub use audit_verifier::{AuditStatus, AuditVerifier};
pub use gate::{SignedWriteGate, SystemWallClock};
pub use promoter::{
    AttestReceipt, AttestRequest, CategoryRule, DemotionOutcome, Promoter, PromotionError,
    PromotionOutcome, PromotionPolicy,
};
pub use reliability::{ReliabilityEvent, ReliabilityFold, ReliabilityOutcome, ReliabilityPolicy};
pub use reliability_scorer::{ReliabilityError, ReliabilityScorer};
pub use resolver::StoreKeyResolver;
pub use signing::Ed25519Verifier;

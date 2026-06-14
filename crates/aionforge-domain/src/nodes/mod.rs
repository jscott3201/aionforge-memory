//! The memory-kind node types (02 §4).
//!
//! Each kind composes the shared [`crate::blocks::Identity`] block; retrievable
//! memory kinds additionally compose [`crate::blocks::Stats`]. Forensic and
//! control kinds carry only identity. Every kind exposes a `LABEL` constant naming
//! its selene-db node label. Kinds are grouped by memory tier, one module per
//! family, to respect the per-file line cap.

pub mod agent;
pub mod anchors;
pub mod associative;
pub mod control;
pub mod core;
pub mod episodic;
pub mod forensic;
pub mod procedural;
pub mod semantic;
pub mod work;

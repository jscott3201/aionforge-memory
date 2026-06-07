//! Asynchronous consolidation: a durable cursor and a bounded background scheduler over
//! the commit stream (write-and-consolidation spec §2–§3, M2.T03).
//!
//! This crate is the *engine* of slow consolidation, not its rules. It drives the
//! lifecycle — discover pending episodes, run each registered [`ConsolidationPass`],
//! flip the episode and advance the durable cursor in one atomic commit, and report
//! lag — while the rules themselves (fact extraction, entity resolution, supersession,
//! summarization) land in later milestones and plug into the [`ConsolidationPass`] seam
//! without touching the scheduler.
//!
//! The durable position lives in the store's [`aionforge_store::ConsolidationCursor`]
//! singleton; the per-episode `consolidation_state` is the work queue. Together they
//! give resume-not-reprocess across restart and an idempotency that no crash can turn
//! into a double-apply: a pass runs against a read snapshot only, and the scheduler
//! commits its result atomically with the state-flip, so an interrupted pass leaves the
//! episode `raw` to be re-run, never half-applied.
//!
//! [`Consolidator::tick_once`] is the deterministic unit of work; [`Consolidator::start`]
//! runs it on a timer and hands back a [`ConsolidationHandle`] for shutdown.

mod audit;
mod clock;
mod config;
mod detect;
mod error;
mod fact_extraction;
mod lag;
mod pass;
mod resolve;
mod rule_extractor;
mod rule_summarizer;
mod scheduler;
mod summarize;

pub use clock::{Clock, SystemClock};
pub use config::{
    ConsolidationConfig, DetectionConfig, PassConfig, PredicateRule, ResolutionConfig,
    SummarizationConfig,
};
pub use error::ConsolidationError;
pub use fact_extraction::FactExtractionPass;
pub use lag::ConsolidationLag;
pub use pass::{ConsolidationPass, NoopPass, PassContext, PassError, PassOutput};
pub use rule_extractor::{ObjectRule, Rule, RuleExtractor};
pub use rule_summarizer::RuleSummarizer;
pub use scheduler::{ConsolidationHandle, Consolidator, TickReport};

// Re-exported from the store so callers can build a pass payload / read derived facts
// without naming the L0 crate directly.
pub use aionforge_store::MaterializedFact;

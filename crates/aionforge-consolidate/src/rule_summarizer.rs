//! A deterministic, rule-based summarizer (write-and-consolidation §2, M2.T06).
//!
//! M2 ships this in place of the model-backed production summarizer (deferred to M4) so
//! the consolidation pass and its idempotency are testable with no network. It condenses a
//! cluster of facts about one subject into a single templated [`Note`](aionforge_domain::nodes::associative::Note)
//! body, naming every predicate and entity the cluster touches — so the roll-up is a pure,
//! reproducible function of the cluster, and the detail-retention guard it feeds passes by
//! construction (the deterministic summary drops no entity). The conservative size gate and
//! the guard live in the pass ([`crate::summarize`]); this seam only renders prose.

use std::convert::Infallible;
use std::future::Future;

use aionforge_domain::contracts::{
    SummarizationCluster, Summarizer, SummarizerIdentity, SummaryOutput,
};

/// A deterministic [`Summarizer`] that renders a cluster into a templated note.
#[derive(Debug, Clone)]
pub struct RuleSummarizer {
    identity: SummarizerIdentity,
}

impl RuleSummarizer {
    /// Build a summarizer with an explicit rule-set version.
    #[must_use]
    pub fn new(rule_version: impl Into<String>) -> Self {
        Self {
            identity: SummarizerIdentity {
                model_family: None,
                model_version: None,
                rule_version: rule_version.into(),
            },
        }
    }

    /// Build a summarizer with the default rule set (`summarize-v1`).
    #[must_use]
    pub fn with_default_rules() -> Self {
        Self::new("summarize-v1")
    }

    /// Render synchronously — the whole of the work; the async seam just wraps this.
    fn summarize_sync(&self, cluster: &SummarizationCluster) -> Option<SummaryOutput> {
        if cluster.facts.is_empty() {
            return None;
        }

        // Distinct predicates and entity names, sorted, so the prose and keywords are a
        // reproducible function of the cluster (the content-addressed note id depends on it).
        let mut predicates: Vec<String> =
            cluster.facts.iter().map(|f| f.predicate.clone()).collect();
        predicates.sort();
        predicates.dedup();
        let mut entities = cluster.entity_names.clone();
        entities.sort();
        entities.dedup();

        let content = format!(
            "{} — {} facts across {}. Entities: {}.",
            cluster.subject_name,
            cluster.facts.len(),
            predicates.join(", "),
            entities.join(", "),
        );

        // Keywords carry every entity and predicate, so the detail-retention guard's
        // entity-preservation check passes by construction for this deterministic summary.
        let mut keywords = entities;
        keywords.extend(predicates);
        keywords.sort();
        keywords.dedup();

        Some(SummaryOutput {
            content,
            keywords,
            context: None,
        })
    }
}

impl Summarizer for RuleSummarizer {
    type Error = Infallible;

    fn summarize(
        &self,
        cluster: &SummarizationCluster,
    ) -> impl Future<Output = Result<Option<SummaryOutput>, Self::Error>> + Send {
        let out = self.summarize_sync(cluster);
        async move { Ok(out) }
    }

    fn identity(&self) -> &SummarizerIdentity {
        &self.identity
    }
}

//! Scheduler tuning (write-and-consolidation §3).

use std::collections::BTreeMap;
use std::time::Duration;

use aionforge_domain::value::ObjectValue;

/// How the background consolidator paces and bounds itself.
///
/// Every field is a bound: how often to look for work, how much to take at once, how
/// long a single pass may run, how many times to retry a transient failure before
/// giving up on an episode, and the lag above which the scheduler warns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationConfig {
    /// How often the spawned loop wakes to drain work.
    pub tick_interval: Duration,
    /// The most episodes a single tick will take (the per-tick concurrency bound).
    pub batch_size: usize,
    /// The wall-clock budget for one pass over one episode.
    pub apply_timeout: Duration,
    /// How many transient failures an episode may accrue before it is marked failed.
    pub max_retries: u32,
    /// The steady-state lag ceiling; the scheduler warns when the oldest pending
    /// episode is older than this.
    pub lag_ceiling: Duration,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            batch_size: 32,
            apply_timeout: Duration::from_secs(30),
            max_retries: 5,
            lag_ceiling: Duration::from_secs(5),
        }
    }
}

/// How the fact-extraction pass resolves surface forms to canonical entities
/// (write-and-consolidation §2). Pass-level tuning, kept separate from the scheduler's
/// [`ConsolidationConfig`]: it carries a float threshold (so it derives `PartialEq`, not
/// `Eq`), and only the extraction pass reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolutionConfig {
    /// How many candidate entities each lexical/vector probe pulls before filtering.
    pub candidate_k: usize,
    /// The cosine-distance ceiling under which an embedding neighbor is judged the same
    /// entity (lower is nearer). Above it, the surface forms a new entity (conservative).
    pub merge_threshold: f64,
}

impl Default for ResolutionConfig {
    fn default() -> Self {
        Self {
            candidate_k: 8,
            merge_threshold: 0.12,
        }
    }
}

/// How one predicate behaves under supersession and contradiction (write-and-consolidation
/// §2).
///
/// A predicate is **multi-valued by default** (the conservative choice — a wrong
/// "functional" silently retires additive facts by status). A functional predicate holds
/// at most one current object per subject, so a newer different object supersedes the
/// prior. `contradicts` declares object-value pairs that are mutually exclusive for this
/// predicate, on top of the always-on boolean inversion rule.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PredicateRule {
    /// Whether `(subject, predicate)` holds at most one current object (newer supersedes).
    pub functional: bool,
    /// Mutually-exclusive object-value pairs for this predicate (order-insensitive).
    pub contradicts: Vec<(ObjectValue, ObjectValue)>,
}

/// How the supersession/contradiction detector decides (write-and-consolidation §2).
///
/// Conservative by construction: predicates are multi-valued unless the registry marks
/// them functional, and only a genuinely high-trust incumbent (`>= high_trust_threshold`)
/// causes a contradicting new fact to be quarantined rather than just recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectionConfig {
    /// Per-predicate behavior; a predicate absent from the map is multi-valued, no antonyms.
    pub predicates: BTreeMap<String, PredicateRule>,
    /// The incumbent trust at or above which a contradicting new fact is quarantined.
    pub high_trust_threshold: f64,
    /// Whether detection runs at all (off → extraction-only, the T04a behavior).
    pub enabled: bool,
}

impl DetectionConfig {
    /// A small, conservative default ruleset (`based_in`/`located_in` functional; boolean
    /// inversion is always on regardless of the registry).
    #[must_use]
    pub fn with_default_rules() -> Self {
        let functional = |contradicts| PredicateRule {
            functional: true,
            contradicts,
        };
        let mut predicates = BTreeMap::new();
        predicates.insert("based_in".to_string(), functional(Vec::new()));
        predicates.insert("located_in".to_string(), functional(Vec::new()));
        Self {
            predicates,
            high_trust_threshold: 0.7,
            enabled: true,
        }
    }

    /// The rule for `predicate` (the conservative default when unregistered).
    #[must_use]
    pub fn rule(&self, predicate: &str) -> PredicateRule {
        self.predicates.get(predicate).cloned().unwrap_or_default()
    }
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self::with_default_rules()
    }
}

/// How the fact-extraction pass condenses fact clusters into summary notes
/// (write-and-consolidation §2, M2.T06).
///
/// Conservative by default: summarization is on, but it only fires on a cluster large
/// enough to be worth condensing, and a detail-retention guard skips any summary that would
/// drop too much of the cluster's specificity rather than write a lossy note. Carries
/// floats, so it derives `PartialEq`, not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct SummarizationConfig {
    /// Whether summarization runs at all (off → extraction/detection only, the T05 behavior).
    pub enabled: bool,
    /// The fewest facts a subject must have before its cluster is worth summarizing.
    pub min_facts: usize,
    /// The fewest distinct entities a cluster must reference before it is summarized.
    pub min_entities: usize,
    /// The fraction of a cluster's distinct entities the summary must preserve (in its
    /// content or keywords) to clear the detail-retention guard. High = conservative: a
    /// summary that drops more specificity than this is skipped, not written.
    pub entity_retention_threshold: f64,
    /// The mean source-fact confidence a cluster must clear before it is summarized.
    pub confidence_floor: f64,
}

impl Default for SummarizationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_facts: 3,
            min_entities: 2,
            entity_retention_threshold: 0.9,
            confidence_floor: 0.6,
        }
    }
}

/// How the skill-induction pass decides whether a recurring episode becomes an induced skill
/// (05 §1, M3.T06).
///
/// Conservative by construction and **off by default**: nothing is induced unless `enabled` is
/// set, and even then only a procedural-role episode whose exact content has recurred at least
/// `repetition_threshold` times (the reuse evidence) within a bounded window, and which clears
/// the lexical-structure floor, is induced. Carries no floats, so it derives `Eq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InductionConfig {
    /// Whether induction runs at all. **Default `false`** — the binding off-by-default gate.
    pub enabled: bool,
    /// The most recent same-content episodes the recurrence probe counts (also the query
    /// `LIMIT`, so it bounds the work a high-volume agent can ask of one pass).
    pub recurrence_window: usize,
    /// The fewest exact-content recurrences (reuse evidence) before an episode is induced.
    pub repetition_threshold: usize,
    /// The fewest distinct whitespace tokens the content must carry to be worth inducing
    /// (rejects a repeated short utterance or a one-line error log).
    pub min_distinct_tokens: usize,
    /// The fewest characters the content must carry to be induced.
    pub min_body_chars: usize,
    /// The most characters an induced body may carry (bounds the stored procedure size; a
    /// longer episode is conservatively not induced rather than truncated).
    pub max_body_chars: usize,
    /// The prefix on every induced skill name (the rest is a content hash), so induced skills
    /// are visibly namespaced apart from authored ones.
    pub name_prefix: String,
}

impl Default for InductionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            recurrence_window: 50,
            repetition_threshold: 3,
            min_distinct_tokens: 5,
            min_body_chars: 16,
            max_body_chars: 4096,
            name_prefix: "induced/".to_string(),
        }
    }
}

/// The tuning the consolidation passes need: entity resolution, supersession detection,
/// summarization, and skill induction. Bundled so the facade and the passes take one config,
/// not a widening list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PassConfig {
    /// Entity-resolution tuning.
    pub resolution: ResolutionConfig,
    /// Supersession/contradiction detection tuning.
    pub detection: DetectionConfig,
    /// Summary-note tuning.
    pub summarization: SummarizationConfig,
    /// Skill-induction tuning (off by default).
    pub induction: InductionConfig,
}

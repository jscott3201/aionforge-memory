//! The mandatory query-class router (03 §3).
//!
//! A lightweight heuristic classifier sorts a query into one of five classes and
//! hands back the retrieval profile for it: the per-signal mode weights plus the
//! behavior flags (graph expansion, the bi-temporal filter, exact rerank, …). The
//! router is required, not optional — indiscriminate graph expansion measurably hurts
//! simple single-hop precision while helping multi-hop recall, so the class has to
//! gate it (03 §3).
//!
//! The classifier is heuristic in v1.0 — quoted phrases, temporal markers, bare
//! proper-noun lookups, and associative cue words — with a documented upgrade path to
//! a learned classifier (00 foundations). Misclassification degrades gracefully: a
//! wrong class still returns a useful ordering, just a less optimal one, and the
//! chosen class is reported so a caller can see it in the retrieval explanation.

use std::sync::LazyLock;

use regex::Regex;

use crate::signals::Signal;

/// The class a query is routed to (03 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryClass {
    /// A simple factual lookup. Graph expansion is suppressed; lexical and dense over
    /// current facts, exact-reranked.
    SingleHopFactual,
    /// A multi-hop or associative question. Graph expansion is enabled.
    MultiHop,
    /// A time-scoped question. The bi-temporal filter applies.
    Temporal,
    /// A bare entity lookup. Graph is seeded on the entity.
    Entity,
    /// An exact-phrase / quote lookup. Lexical only.
    Quote,
}

/// The per-signal mode weights a profile assigns (03 §3). Each is non-negative; zero
/// elides the signal from fusion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalWeights {
    /// Lexical (BM25) weight.
    pub lexical: f64,
    /// Lexical-anchor weight. This is a factual-query guard over the highest BM25 hits,
    /// not a separate search path.
    pub lexical_anchor: f64,
    /// Dense (vector) weight.
    pub dense: f64,
    /// Support-expansion weight (03 §4, M3.T02): the graph-guided dense scoring over a
    /// query entity's supporting evidence. Non-zero only for the graph-expansion classes;
    /// additive to `dense`, so it lifts recovered evidence without diluting dense precision.
    pub support: f64,
    /// Associative graph weight.
    pub graph: f64,
    /// Recency weight.
    pub recency: f64,
    /// Effective-importance weight (05 §2, M5.T01).
    pub importance: f64,
    /// Trust weight.
    pub trust: f64,
}

impl SignalWeights {
    /// The weight assigned to `signal`.
    #[must_use]
    pub fn weight(&self, signal: Signal) -> f64 {
        match signal {
            Signal::Lexical => self.lexical,
            Signal::LexicalAnchor => self.lexical_anchor,
            Signal::Dense => self.dense,
            Signal::Support => self.support,
            Signal::Graph => self.graph,
            Signal::Recency => self.recency,
            Signal::Importance => self.importance,
            Signal::Trust => self.trust,
        }
    }
}

/// A retrieval profile: the class, its mode weights, and the behavior flags the rest
/// of the pipeline reads (03 §3–§5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetrievalProfile {
    /// The class this profile serves.
    pub class: QueryClass,
    /// The per-signal mode weights.
    pub weights: SignalWeights,
    /// Whether to run associative graph expansion (PageRank prior + `SUPPORTS`
    /// expansion). Suppressed for single-hop and quote classes (03 §3).
    pub graph_expansion: bool,
    /// Whether to apply the bi-temporal validity filter (03 §5).
    pub bitemporal_filter: bool,
    /// Whether to exact-vector-rerank the candidate set — the high-precision default
    /// for the factual and temporal classes (03 §4).
    pub exact_rerank: bool,
    /// Whether to prefer the exact phrase on the lexical signal (quote class).
    pub quote_phrase: bool,
    /// Whether to default the candidate kinds to facts (the factual class, 03 §3).
    pub restrict_to_fact_kinds: bool,
    /// The per-class default absolute dense-relevance floor in `[0, 1]`.
    ///
    /// A hit whose dense cosine similarity is below the floor — or which has no dense
    /// score at all (a lexical-only hit) — is dropped, so an off-topic query can
    /// legitimately return empty. `0.0` is OFF (the floor never fires). The factual class
    /// floors at `0.60` (the `FACTUAL_FLOOR` const — an off-topic-rejection win measured on
    /// the eval harness); the other classes are still OFF pending their own calibration. The
    /// dense-weight-zero classes (e.g. `Quote`) stay exempt at `0.0`, since a dense floor
    /// is meaningless where the dense signal itself is off. A per-query
    /// `RecallOptions::min_relevance` overrides this, which in turn overrides the
    /// deployment-wide `RetrieverConfig::min_relevance`.
    pub min_relevance: f64,
}

/// Weight levels the mode profiles are built from (03 §3 "heavy/moderate/light").
const HEAVY: f64 = 1.0;
const MODERATE: f64 = 0.6;
const LIGHT: f64 = 0.3;
const OFF: f64 = 0.0;

/// The per-class dense-relevance floor for a class that has not been calibrated yet — OFF
/// (the floor never fires). Named so a non-zero value reads as a deliberate flip.
const FLOOR_OFF: f64 = 0.0;

/// The single-hop-factual dense-relevance floor: off-topic queries on the factual class
/// return empty rather than surfacing a confident-but-unrelated hit.
///
/// `0.60` is the center of the clean separation window measured on the `aionforge-eval`
/// off-topic-rejection harness against the production gemini-3072 embedder: across an
/// everyday corpus and a harder project/domain-adjacent corpus, off-topic queries peaked
/// at ~0.55 dense similarity while on-topic gold bottomed at ~0.64, and a floor at the
/// midpoint rejected 100% of off-topic queries at zero false-rejection (recall@5 held at
/// 1.0). The midpoint maximizes the margin on both sides, so it is robust to embedder
/// drift. Re-measure with `cargo test -p aionforge-eval --test floor_sweep -- --ignored`
/// after any embedder or dimension change (the value is gemini-cosine-calibrated).
const FACTUAL_FLOOR: f64 = 0.60;

/// The default retrieval profile for a class (03 §3 mode-weight profiles).
#[must_use]
pub fn profile_for(class: QueryClass) -> RetrievalProfile {
    match class {
        // factual: heavy lexical + lexical anchor + dense, light graph, heavy trust,
        // light recency.
        QueryClass::SingleHopFactual => RetrievalProfile {
            class,
            min_relevance: FACTUAL_FLOOR,
            weights: SignalWeights {
                lexical: HEAVY,
                lexical_anchor: HEAVY,
                dense: HEAVY,
                support: OFF,
                graph: LIGHT,
                recency: LIGHT,
                importance: LIGHT,
                trust: HEAVY,
            },
            graph_expansion: false,
            bitemporal_filter: false,
            exact_rerank: true,
            quote_phrase: false,
            restrict_to_fact_kinds: true,
        },
        // associative: heavy dense + graph, light lexical, moderate trust, light recency.
        QueryClass::MultiHop => RetrievalProfile {
            class,
            min_relevance: FLOOR_OFF,
            weights: SignalWeights {
                lexical: LIGHT,
                lexical_anchor: OFF,
                dense: HEAVY,
                support: MODERATE,
                graph: HEAVY,
                recency: LIGHT,
                importance: LIGHT,
                trust: MODERATE,
            },
            graph_expansion: true,
            bitemporal_filter: false,
            exact_rerank: false,
            quote_phrase: false,
            restrict_to_fact_kinds: false,
        },
        // recall: heavy recency + dense, moderate lexical, no graph, moderate trust.
        QueryClass::Temporal => RetrievalProfile {
            class,
            min_relevance: FLOOR_OFF,
            weights: SignalWeights {
                lexical: MODERATE,
                lexical_anchor: OFF,
                dense: HEAVY,
                support: OFF,
                graph: OFF,
                recency: HEAVY,
                importance: LIGHT,
                trust: MODERATE,
            },
            graph_expansion: false,
            bitemporal_filter: true,
            exact_rerank: true,
            quote_phrase: false,
            restrict_to_fact_kinds: false,
        },
        // entity: heavy graph + moderate dense, lexical over aliases, no recency.
        QueryClass::Entity => RetrievalProfile {
            class,
            min_relevance: FLOOR_OFF,
            weights: SignalWeights {
                lexical: MODERATE,
                lexical_anchor: OFF,
                dense: MODERATE,
                support: MODERATE,
                graph: HEAVY,
                recency: OFF,
                importance: LIGHT,
                trust: MODERATE,
            },
            graph_expansion: true,
            bitemporal_filter: false,
            exact_rerank: false,
            quote_phrase: false,
            restrict_to_fact_kinds: false,
        },
        // quote: lexical only, exact-phrase preference.
        QueryClass::Quote => RetrievalProfile {
            class,
            min_relevance: FLOOR_OFF,
            weights: SignalWeights {
                lexical: HEAVY,
                lexical_anchor: HEAVY,
                dense: OFF,
                support: OFF,
                graph: OFF,
                recency: OFF,
                importance: OFF,
                trust: OFF,
            },
            graph_expansion: false,
            bitemporal_filter: false,
            exact_rerank: false,
            quote_phrase: true,
            restrict_to_fact_kinds: false,
        },
    }
}

/// Classify a query, then return its retrieval profile (03 §3). The main entry point.
#[must_use]
pub fn route(query: &str) -> RetrievalProfile {
    profile_for(classify(query))
}

/// A double-quoted (straight or curly) phrase with at least one character inside.
static QUOTED_PHRASE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]+"|“[^”]+”"#).expect("valid static pattern"));

/// Temporal markers: when/before/after/since, relative-time spans, and 4-digit years.
static TEMPORAL_MARKERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(when|before|after|during|since|until|yesterday|today|tomorrow|earlier|recently|ago|previously|originally)\b|\bas of\b|\b(last|past) (week|month|year|decade|century|night|time)\b|\b(19|20)\d{2}\b",
    )
    .expect("valid static pattern")
});

/// Associative / multi-hop cue words, plus a few strong causal phrases (kept as
/// phrases, not bare words, so common verbs do not over-trigger graph expansion).
static MULTIHOP_MARKERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(why|how|because|relationship|related|relate|connection|connect|between|cause|caused|influence|influenced|impact|associated|associate|compare|versus|vs)\b|\b(leads?|led) to\b|\bresults? in\b|\bdepends? on\b|\bdue to\b",
    )
    .expect("valid static pattern")
});

/// Question / command words. Their presence rules out the bare-entity heuristic.
static INTERROGATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(what|who|whom|whose|where|which|is|are|was|were|do|does|did|list|find|show|tell|give|name|define|explain)\b",
    )
    .expect("valid static pattern")
});

/// Source/file anchors: paths, common source/doc extensions, or multiple hyphen/underscore
/// anchor tokens. These are exact lookup cues even when they contain dates.
static SOURCE_PATH_MARKERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|\s)(?:[./~]?[\w.-]+/[\w./-]+|[\w./-]+\.(?:rs|md|toml|json|jsonl|yaml|yml|lock|txt|sh|ts|tsx|js|jsx|py|go|java|rb|sql|proto|drawio))(?:\s|$)",
    )
    .expect("valid static pattern")
});

static SOURCE_ANCHOR_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Za-z0-9]+[-_][A-Za-z0-9][A-Za-z0-9_-]*\b").expect("valid static pattern")
});

/// Sort a query into a [`QueryClass`] by heuristic (03 §3).
///
/// Precedence runs from most specific to least: an explicit quoted phrase, source/file
/// anchors, temporal markers, a bare proper-noun entity, associative cue words, and
/// finally the single-hop factual default. The order matters — a temporal phrase that
/// also reads like a multi-hop question is routed temporal so the bi-temporal filter
/// applies, unless the temporal-looking token is inside a source path such as
/// `docs/2026-plan.md`.
#[must_use]
pub fn classify(query: &str) -> QueryClass {
    let query = query.trim();
    if QUOTED_PHRASE.is_match(query) || looks_like_source_anchor(query) {
        QueryClass::Quote
    } else if TEMPORAL_MARKERS.is_match(query) {
        QueryClass::Temporal
    } else if looks_like_entity(query) {
        QueryClass::Entity
    } else if MULTIHOP_MARKERS.is_match(query) {
        QueryClass::MultiHop
    } else {
        QueryClass::SingleHopFactual
    }
}

pub(crate) fn looks_like_source_anchor(query: &str) -> bool {
    SOURCE_PATH_MARKERS.is_match(query) || SOURCE_ANCHOR_TOKEN.find_iter(query).take(2).count() >= 2
}

/// A bare entity lookup: a one- or two-token query with no question words whose
/// alphabetic tokens all read as proper nouns (each starts uppercase), like
/// `Ada Lovelace` or `France`.
///
/// Deliberately conservative. Capitalization alone cannot tell a proper noun from a
/// title-cased common phrase (`Climate Change`), and the costly error is the
/// false positive — entity routing turns on graph expansion, which the spec says
/// hurts single-hop precision (03 §3). So the cap is two tokens, which keeps the
/// common 1–2 word entity lookups while sending longer title-cased phrases
/// (`Quantum Entanglement Breakthrough`) to the safe single-hop default. The
/// residual two-word ambiguity is a known v1 limitation that degrades gracefully;
/// the upgrade path is a learned classifier or a store-backed entity check.
fn looks_like_entity(query: &str) -> bool {
    if INTERROGATIVE.is_match(query) {
        return false;
    }
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() > 2 {
        return false;
    }
    let mut alphabetic = tokens
        .iter()
        .filter(|token| token.chars().any(char::is_alphabetic))
        .peekable();
    if alphabetic.peek().is_none() {
        return false;
    }
    alphabetic.all(|token| token.chars().next().is_some_and(char::is_uppercase))
}

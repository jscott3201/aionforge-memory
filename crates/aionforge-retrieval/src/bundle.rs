//! The recall bundle: the two coordinated views plus the explanation (03 §6).
//!
//! The **structured** view is the memories in fused score order, each with the
//! metadata a caller needs to reason about provenance and the per-signal
//! contributions that ranked it. The **rendered** view is the same set rendered for
//! prompt injection, ordered by a stable serialization id so the same recalled set
//! produces byte-identical text across calls — the inference-server prefix-cache
//! contract. Recalled content is wrapped in structural tags that mark it as
//! third-party data, not instructions (a security requirement, 07).
//!
//! [`RecallBundle::render_compact`] is a third view for token-thrifty callers (the MCP
//! surface): a one-line summary plus one short, snippet-capped line per memory. It is
//! held to the same security contract as [`render`] — the same `recalled-memory-context`
//! wrapper and the same `tag_escape` on every snippet — so a compact result is no less
//! safe to splice into a prompt than the full rendered view.

use aionforge_domain::ids::{Id, SerializationId};
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::core::BlockKind;
use aionforge_domain::nodes::episodic::Role;
use aionforge_domain::nodes::semantic::FactStatus;
use aionforge_domain::time::Timestamp;
use aionforge_domain::{RECALLED_MEMORY_CONTEXT_CLOSE, RECALLED_MEMORY_CONTEXT_OPEN};

use crate::fusion::Contribution;
use crate::router::{QueryClass, SignalWeights};
use crate::signals::Signal;

/// One memory in the structured view (03 §6): a captured episode or a derived fact.
///
/// Episodes and facts coexist in one bundle so a recall can return raw turns and the
/// semantic assertions distilled from them together (03 §5–§6). The variants share the
/// ranking fields a caller reasons about — serialization id, namespace, trust, fused
/// score, contributions, and the rendered text — exposed through accessors so the
/// fusion, ordering, and rendering code treats an entry uniformly without unpacking the
/// variant; the kind-specific metadata (an episode's role, a fact's predicate/status
/// and bi-temporal window) is read by matching the variant.
#[derive(Debug, Clone, PartialEq)]
pub enum StructuredEntry {
    /// A captured episode (a raw turn).
    Episode(EpisodeEntry),
    /// A derived semantic fact, with its bi-temporal validity window.
    Fact(FactEntry),
    /// An identity-tier core block (05 §4): always included by the recall pre-pass,
    /// never ranked — identity is context, not a search hit.
    CoreBlock(CoreBlockEntry),
}

/// A captured episode in the structured view.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeEntry {
    /// The episode's stable domain id.
    pub id: Id,
    /// The content-derived serialization id that orders the rendered view.
    pub serialization_id: SerializationId,
    /// The episode's namespace.
    pub namespace: Namespace,
    /// The producing role.
    pub role: Role,
    /// Transaction-time creation instant.
    pub ingested_at: Timestamp,
    /// Soft-expiry instant, if any (only present on history queries).
    pub expired_at: Option<Timestamp>,
    /// Writer trust.
    pub trust: f64,
    /// The fused RRF score.
    pub score: f64,
    /// The per-signal contributions that ranked it.
    pub contributions: Vec<Contribution>,
    /// The episode content.
    pub content: String,
}

/// A derived fact in the structured view, carrying its bi-temporal validity window.
#[derive(Debug, Clone, PartialEq)]
pub struct FactEntry {
    /// The fact's stable domain id.
    pub id: Id,
    /// The content-derived serialization id that orders the rendered view.
    pub serialization_id: SerializationId,
    /// The fact's namespace.
    pub namespace: Namespace,
    /// The canonical subject `Entity.id` the fact is about.
    pub subject_id: Id,
    /// The relation.
    pub predicate: String,
    /// Extraction/assertion confidence in `[0, 1]`.
    pub confidence: f64,
    /// The assertion lifecycle status (active / quarantined / superseded).
    pub status: FactStatus,
    /// Derivation trust.
    pub trust: f64,
    /// The fused RRF score.
    pub score: f64,
    /// The per-signal contributions that ranked it.
    pub contributions: Vec<Contribution>,
    /// The canonical natural-language rendering — the searchable, rendered text.
    pub statement: String,
    /// Transaction-time lower bound of the `ABOUT` window: when the substrate recorded it.
    pub ingested_at: Timestamp,
    /// Transaction-time upper bound: when the record was expired; `None` while live.
    pub expired_at: Option<Timestamp>,
    /// Event-time lower bound: when the fact became true.
    pub valid_from: Timestamp,
    /// Event-time upper bound: when it stopped being true; `None` while current.
    pub valid_to: Option<Timestamp>,
}

/// An identity-tier core block in the structured view (05 §4).
///
/// Core blocks reach the bundle through the always-include pre-pass, not the ranked
/// signals: they bypass fusion and the session-diversity cap, carry no score and no
/// contributions, and are gated only by the reader's visible set. Identity is the
/// standing context every recall is read against, not a hit that competes on
/// relevance.
#[derive(Debug, Clone, PartialEq)]
pub struct CoreBlockEntry {
    /// The block's one stable domain id.
    pub id: Id,
    /// The content-derived serialization id that orders the rendered view.
    pub serialization_id: SerializationId,
    /// The block's namespace.
    pub namespace: Namespace,
    /// The block's category (persona / commitment / redline).
    pub block_kind: BlockKind,
    /// Sensitivity classification, if any.
    pub sensitivity: Option<String>,
    /// Writer trust.
    pub trust: f64,
    /// The block body.
    pub content: String,
}

impl StructuredEntry {
    /// The content-derived serialization id that orders the rendered view.
    #[must_use]
    pub fn serialization_id(&self) -> &SerializationId {
        match self {
            StructuredEntry::Episode(e) => &e.serialization_id,
            StructuredEntry::Fact(f) => &f.serialization_id,
            StructuredEntry::CoreBlock(c) => &c.serialization_id,
        }
    }

    /// The entry's stable domain id.
    #[must_use]
    pub fn id(&self) -> &Id {
        match self {
            StructuredEntry::Episode(e) => &e.id,
            StructuredEntry::Fact(f) => &f.id,
            StructuredEntry::CoreBlock(c) => &c.id,
        }
    }

    /// The entry's namespace.
    #[must_use]
    pub fn namespace(&self) -> &Namespace {
        match self {
            StructuredEntry::Episode(e) => &e.namespace,
            StructuredEntry::Fact(f) => &f.namespace,
            StructuredEntry::CoreBlock(c) => &c.namespace,
        }
    }

    /// Writer/derivation trust.
    #[must_use]
    pub fn trust(&self) -> f64 {
        match self {
            StructuredEntry::Episode(e) => e.trust,
            StructuredEntry::Fact(f) => f.trust,
            StructuredEntry::CoreBlock(c) => c.trust,
        }
    }

    /// The fused RRF score. A core block reaches the bundle through the
    /// always-include pre-pass, never the ranked signals, so it has no score — `0.0`,
    /// the additive identity, keeps the accessor total without inventing a rank.
    #[must_use]
    pub fn score(&self) -> f64 {
        match self {
            StructuredEntry::Episode(e) => e.score,
            StructuredEntry::Fact(f) => f.score,
            StructuredEntry::CoreBlock(_) => 0.0,
        }
    }

    /// The per-signal contributions that ranked the entry. Empty for a core block —
    /// nothing ranked it.
    #[must_use]
    pub fn contributions(&self) -> &[Contribution] {
        match self {
            StructuredEntry::Episode(e) => &e.contributions,
            StructuredEntry::Fact(f) => &f.contributions,
            StructuredEntry::CoreBlock(_) => &[],
        }
    }

    /// The entry's rendered/searchable text — an episode's content, a fact's
    /// statement, or a core block's body. The body of the rendered `memory` tag and
    /// the rendered-order tie-break (03 §6).
    #[must_use]
    pub fn content(&self) -> &str {
        match self {
            StructuredEntry::Episode(e) => &e.content,
            StructuredEntry::Fact(f) => &f.statement,
            StructuredEntry::CoreBlock(c) => &c.content,
        }
    }
}

/// Why the retrieval ranked the way it did (03 §6). Not part of the deterministic
/// rendered text — timings vary run to run.
#[derive(Debug, Clone)]
pub struct RecallExplanation {
    /// The query class the router chose.
    pub class: QueryClass,
    /// The mode weights applied.
    pub weights: SignalWeights,
    /// Which signals actually ran (a signal with zero weight, or dense when the
    /// embedder was down, does not).
    pub signals_run: Vec<Signal>,
    /// Whether the embedder was reachable; false means the dense signal was dropped
    /// and retrieval degraded to the rest (03 §6, §8.1).
    pub embedder_available: bool,
    /// Distinct candidates that passed authorization and filtering.
    pub candidates_considered: usize,
    /// How many memories the bundle returned.
    pub returned: usize,
    /// Coarse per-stage wall-clock timings, in milliseconds.
    pub timings_ms: StageTimings,
}

/// Coarse per-stage timings for the retrieval explanation, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StageTimings {
    /// Classifying the query.
    pub classify: u128,
    /// Running the signals.
    pub signals: u128,
    /// Fusing and assembling the bundle.
    pub assemble: u128,
}

/// A recall bundle: the structured and rendered views and the explanation (03 §6).
#[derive(Debug, Clone)]
pub struct RecallBundle {
    /// The memories in fused score order, with metadata and contributions.
    pub structured: Vec<StructuredEntry>,
    /// The same memories rendered for prompt injection, serialization-id ordered and
    /// wrapped as third-party data.
    pub rendered: String,
    /// The retrieval explanation.
    pub explanation: RecallExplanation,
}

/// The longest snippet (in characters) shown per memory in the compact view.
const COMPACT_SNIPPET_CHARS: usize = 160;

impl RecallBundle {
    /// Render a token-thrifty view: a one-line summary, then one line per memory
    /// (serialization id, role, score, score band, snippet). `verbose` adds a trusted route/signal
    /// explanation plus the namespace, trust, and per-signal contributions as attributes
    /// on each memory line.
    ///
    /// Memories are listed in fused score order (most relevant first) — the order a
    /// caller wants for a ranked result, with the score shown so the ranking is
    /// transparent. The summary line is trusted metadata and sits outside the wrapper;
    /// every snippet is `tag_escape`d and the block carries the same
    /// `recalled-memory-context` framing as [`render`], so the compact view is held to
    /// the recall security contract (07 §4) just like the full rendered view.
    #[must_use]
    pub fn render_compact(&self, verbose: bool) -> String {
        let explanation = &self.explanation;
        let more = explanation
            .candidates_considered
            .saturating_sub(explanation.returned);

        let mut out = format!(
            "hits: {returned} of {considered} considered | class={class} | embedder={embedder}",
            returned = explanation.returned,
            considered = explanation.candidates_considered,
            class = class_tag(explanation.class),
            embedder = if explanation.embedder_available {
                "up"
            } else {
                "down"
            },
        );
        if more > 0 {
            out.push_str(&format!(" | +{more} more"));
        }
        out.push('\n');
        if verbose {
            out.push_str(&format!(
                "explain: route={route} embedder={embedder} signals={signals} weights={weights}\n",
                route = class_tag(explanation.class),
                embedder = if explanation.embedder_available {
                    "up"
                } else {
                    "down(dense skipped)"
                },
                signals = signal_list(&explanation.signals_run),
                weights = weight_list(&explanation.weights, &explanation.signals_run),
            ));
        }

        out.push_str(RECALLED_MEMORY_CONTEXT_OPEN);
        out.push('\n');
        let max_ranked_score = self
            .structured
            .iter()
            .filter(|entry| !matches!(entry, StructuredEntry::CoreBlock(_)))
            .map(StructuredEntry::score)
            .fold(0.0_f64, f64::max);
        for entry in &self.structured {
            let id = entry.id();
            let sid = entry.serialization_id();
            // The kind-specific head: an episode carries its role, a fact its predicate
            // and lifecycle status. The actionable domain id is exposed as `id` so compact
            // MCP results can feed lifecycle follow-up tools; the content-derived
            // serialization id stays available as `sid`. The predicate is `attr_escape`d so an
            // extracted value cannot break out of its attribute quotes (07 §4). The fused score
            // is common to both kinds, so it is read through the accessor and appended once.
            match entry {
                StructuredEntry::Episode(e) => out.push_str(&format!(
                    "<memory id=\"{id}\" sid=\"{sid}\" kind=\"episode\" role=\"{role}\"",
                    role = role_tag(e.role),
                )),
                StructuredEntry::Fact(f) => out.push_str(&format!(
                    "<memory id=\"{id}\" sid=\"{sid}\" kind=\"fact\" predicate=\"{predicate}\" status=\"{status}\"",
                    predicate = attr_escape(&f.predicate),
                    status = status_tag(f.status),
                )),
                StructuredEntry::CoreBlock(c) => out.push_str(&format!(
                    "<memory id=\"{id}\" sid=\"{sid}\" kind=\"core\" block_kind=\"{kind}\"",
                    kind = block_kind_tag(c.block_kind),
                )),
            }
            // A core block was never ranked: it carries the always-include marker
            // instead of a score, so a 0.0000 cannot read as "barely relevant".
            match entry {
                StructuredEntry::CoreBlock(_) => out.push_str(" always=\"true\""),
                _ => out.push_str(&format!(
                    " score=\"{:.4}\" score_band=\"{}\"",
                    entry.score(),
                    score_band(entry.score(), max_ranked_score),
                )),
            }
            if verbose {
                let via = entry
                    .contributions()
                    .iter()
                    .map(|c| format!("{}#{}", signal_tag(c.signal), c.rank))
                    .collect::<Vec<_>>()
                    .join(" ");
                // The namespace is `attr_escape`d like the predicate: an agent/team id is
                // a plain string, so a hostile id cannot break out of the attribute (07 §4).
                out.push_str(&format!(
                    " ns=\"{ns}\" trust=\"{trust:.2}\" via=\"{via}\"",
                    ns = attr_escape(&entry.namespace().to_string()),
                    trust = entry.trust(),
                ));
            }
            out.push('>');
            out.push_str(&tag_escape(&snippet(
                entry.content(),
                COMPACT_SNIPPET_CHARS,
            )));
            out.push_str("</memory>\n");
        }
        out.push_str(RECALLED_MEMORY_CONTEXT_CLOSE);
        out.push('\n');
        out
    }
}

/// A whitespace-collapsed, length-capped snippet of content, counted in characters so
/// the cap never splits a multi-byte character.
fn snippet(content: &str, max: usize) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let head: String = collapsed.chars().take(max).collect();
        format!("{head}…")
    }
}

/// The compact tag for a query class.
fn class_tag(class: QueryClass) -> &'static str {
    match class {
        QueryClass::SingleHopFactual => "single_hop_factual",
        QueryClass::MultiHop => "multi_hop",
        QueryClass::Temporal => "temporal",
        QueryClass::Entity => "entity",
        QueryClass::Quote => "quote",
    }
}

/// The compact tag for a signal.
fn signal_tag(signal: Signal) -> &'static str {
    match signal {
        Signal::Lexical => "lexical",
        Signal::LexicalAnchor => "lexical_anchor",
        Signal::Dense => "dense",
        Signal::Support => "support",
        Signal::Graph => "graph",
        Signal::Recency => "recency",
        Signal::Importance => "importance",
        Signal::Trust => "trust",
    }
}

/// A stable compact list of the signals that actually ran.
fn signal_list(signals: &[Signal]) -> String {
    if signals.is_empty() {
        return "none".to_string();
    }

    signals
        .iter()
        .map(|signal| signal_tag(*signal))
        .collect::<Vec<_>>()
        .join(",")
}

/// A stable compact list of non-zero weights for the signals that ran.
fn weight_list(weights: &SignalWeights, signals_run: &[Signal]) -> String {
    let rendered = signals_run
        .iter()
        .map(|signal| (*signal, weights.weight(*signal)))
        .filter(|(_, weight)| *weight > 0.0)
        .map(|(signal, weight)| format!("{}:{weight:.2}", signal_tag(signal)))
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        "none".to_string()
    } else {
        rendered.join(",")
    }
}

fn score_band(score: f64, max_score: f64) -> &'static str {
    if max_score <= 0.0 || score <= 0.0 {
        return "low";
    }
    let ratio = score / max_score;
    if ratio >= 0.85 {
        "high"
    } else if ratio >= 0.50 {
        "medium"
    } else {
        "low"
    }
}

/// The spec string for a role in the rendered view.
fn role_tag(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
        Role::Event => "event",
    }
}

/// The spec string for a fact's lifecycle status in the rendered view.
fn status_tag(status: FactStatus) -> &'static str {
    match status {
        FactStatus::Active => "active",
        FactStatus::Quarantined => "quarantined",
        FactStatus::Superseded => "superseded",
    }
}

/// The spec string for a core block's category in the rendered view — also a field of
/// the core serialization-id key, so the rendered attributes and the rendered order
/// derive from the same bytes.
pub(crate) fn block_kind_tag(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Persona => "persona",
        BlockKind::Commitment => "commitment",
        BlockKind::Redline => "redline",
    }
}

/// Render entries (already serialization-id ordered) into the prompt-injection view.
///
/// The output is a pure function of the entries' serialization ids, roles, and
/// content — no clock, no run-varying state — so the same recalled set renders
/// byte-identically every time. Each memory sits inside a `memory` tag, and the whole
/// block is marked as untrusted third-party data (07). Content is tag-escaped so it
/// cannot break out of its `memory` wrapper and pose as instructions or as another
/// memory; semantic injection-marker hardening is the capture filter's job (07 §2).
#[must_use]
pub fn render(entries: &[StructuredEntry]) -> String {
    let mut out = String::new();
    out.push_str(RECALLED_MEMORY_CONTEXT_OPEN);
    out.push('\n');
    for entry in entries {
        let sid = entry.serialization_id();
        // The opening tag carries kind-specific, trusted metadata as attributes; an
        // extracted predicate is `attr_escape`d so it cannot break out of its quotes.
        // The body — the episode content or the fact statement — is `tag_escape`d so it
        // cannot forge or close a `memory` tag (07).
        match entry {
            StructuredEntry::Episode(e) => out.push_str(&format!(
                "<memory id=\"{sid}\" kind=\"episode\" role=\"{}\">\n",
                role_tag(e.role),
            )),
            StructuredEntry::Fact(f) => out.push_str(&format!(
                "<memory id=\"{sid}\" kind=\"fact\" predicate=\"{}\" status=\"{}\">\n",
                attr_escape(&f.predicate),
                status_tag(f.status),
            )),
            // The sensitivity is a free string from the block's author, so it is
            // `attr_escape`d like a fact predicate; the kind tag is a closed enum.
            StructuredEntry::CoreBlock(c) => {
                out.push_str(&format!(
                    "<memory id=\"{sid}\" kind=\"core\" block_kind=\"{}\"",
                    block_kind_tag(c.block_kind),
                ));
                if let Some(sensitivity) = &c.sensitivity {
                    out.push_str(&format!(" sensitivity=\"{}\"", attr_escape(sensitivity)));
                }
                out.push_str(">\n");
            }
        }
        out.push_str(&tag_escape(entry.content()));
        out.push('\n');
        out.push_str("</memory>\n");
    }
    out.push_str(RECALLED_MEMORY_CONTEXT_CLOSE);
    out.push('\n');
    out
}

/// Escape the characters that delimit the structural tags, so recalled content cannot
/// forge or close a tag. `&` is escaped first so the replacements do not compound.
fn tag_escape(content: &str) -> String {
    content
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape an attribute value: the tag delimiters plus the double quote that bounds the
/// value, so an extracted value (a fact predicate) cannot break out of its attribute
/// and forge another. `&` is escaped first so the replacements do not compound (07 §4).
fn attr_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact_test_bundle() -> RecallBundle {
        let id = Id::generate();
        let entry = EpisodeEntry {
            id,
            serialization_id: SerializationId::derive("episode", id.to_string().as_bytes()),
            namespace: Namespace::Agent("agent-a".to_string()),
            role: Role::User,
            ingested_at: ts("2026-06-06T09:30:00-05:00[America/Chicago]"),
            expired_at: None,
            trust: 0.8,
            score: 1.0,
            contributions: vec![
                Contribution {
                    signal: Signal::Lexical,
                    rank: 0,
                    weight: 1.0,
                },
                Contribution {
                    signal: Signal::Dense,
                    rank: 1,
                    weight: 1.0,
                },
            ],
            content: "ranked compact memory".to_string(),
        };
        RecallBundle {
            structured: vec![StructuredEntry::Episode(entry)],
            rendered: String::new(),
            explanation: RecallExplanation {
                class: QueryClass::SingleHopFactual,
                weights: SignalWeights {
                    lexical: 1.0,
                    lexical_anchor: 1.0,
                    dense: 1.0,
                    support: 0.0,
                    graph: 0.3,
                    recency: 0.3,
                    importance: 0.3,
                    trust: 1.0,
                },
                signals_run: vec![Signal::Lexical, Signal::Dense],
                embedder_available: true,
                candidates_considered: 1,
                returned: 1,
                timings_ms: StageTimings::default(),
            },
        }
    }

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid test timestamp")
    }

    #[test]
    fn compact_verbose_explains_route_signals_and_active_weights() {
        let bundle = compact_test_bundle();
        let plain = bundle.render_compact(false);
        let verbose = bundle.render_compact(true);

        assert!(
            !plain.contains("explain:"),
            "non-verbose compact view stays terse: {plain}"
        );
        assert!(
            verbose.starts_with(
                "hits: 1 of 1 considered | class=single_hop_factual | embedder=up\n\
                 explain: route=single_hop_factual embedder=up signals=lexical,dense \
                 weights=lexical:1.00,dense:1.00\n"
            ),
            "verbose compact view explains the route and active signals: {verbose}"
        );
        assert!(
            verbose.contains("via=\"lexical#0 dense#1\""),
            "verbose memory lines keep per-hit contributions: {verbose}"
        );
        assert!(
            verbose.contains("score=\"1.0000\" score_band=\"high\""),
            "ranked hits expose a coarse score band next to the raw RRF score: {verbose}"
        );
    }

    #[test]
    fn score_bands_are_relative_to_the_top_ranked_hit() {
        assert_eq!(score_band(0.85, 1.0), "high");
        assert_eq!(score_band(0.50, 1.0), "medium");
        assert_eq!(score_band(0.49, 1.0), "low");
        assert_eq!(score_band(0.0, 1.0), "low");
        assert_eq!(score_band(1.0, 0.0), "low");
    }
}

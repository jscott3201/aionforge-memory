//! A deterministic, pattern-based fact extractor (write-and-consolidation §2).
//!
//! M2 ships this in place of the model-backed production extractor (deferred to M4) so
//! the consolidation pass and its idempotency are testable with no network. It scans
//! episode text for a small, configurable set of verb-marker relations and emits one
//! triple per match, recording the matched sentence's byte range as the source span.
//! Because it is a pure function of `(content, ruleset)`, the same episode always yields
//! the same facts — which, with the content-derived fact id, makes re-extraction a no-op.

use std::convert::Infallible;
use std::future::Future;

use aionforge_domain::contracts::{
    EntitySurface, ExtractedFact, ExtractedObject, ExtractorIdentity, FactExtractor,
};
use aionforge_domain::nodes::episodic::Episode;
use aionforge_domain::nodes::semantic::SourceSpan;
use aionforge_domain::value::ObjectValue;

use crate::config::ExtractionConfig;

/// Leading connectives that disqualify a surface from being a fact subject: a clause that
/// opens with a subordinating/coordinating conjunction or relative connective is a dependent
/// fragment (`"because the build failed"`, `"which she uses daily"`), not a standalone
/// subject. Matched as a lowercased EXACT first token only (a proper noun that merely
/// *contains* one of these as a substring is untouched), mirroring the closed-list
/// discipline of [`RELATIONSHIP_VOCABULARY`](crate::RELATIONSHIP_VOCABULARY). Conservative
/// by construction: the list is short and unambiguous so a legitimate short proper-noun
/// subject is never rejected.
pub const SUBJECT_LEADING_STOPWORDS: &[&str] = &[
    "because", "so", "and", "but", "or", "nor", "yet", "which", "that", "when", "while", "if",
    "although", "though", "since", "as",
];

/// Bare pronouns/deictics that disqualify a surface when they are the WHOLE subject: an
/// unresolved `"it"`/`"they"`/`"this"` carries no referent the resolver can canonicalize, so
/// it would mint a junk entity. Matched against the entire lowercased surface (a multi-word
/// subject that merely begins with one of these is untouched), mirroring the closed-list
/// discipline of [`RELATIONSHIP_VOCABULARY`](crate::RELATIONSHIP_VOCABULARY).
pub const SUBJECT_BARE_PRONOUNS: &[&str] = &[
    "this", "that", "it", "they", "these", "those", "here", "there",
];

/// The most whitespace tokens a plausible subject (or entity-object) surface may carry. A
/// surface longer than this is a clause, not a name, so it is rejected before it can mint an
/// entity. Conservative: real subjects are short (`"Alice"`, `"Alice Smith"`, an org name),
/// and this ceiling leaves ample room for the longest legitimate proper-noun phrase.
pub const MAX_SUBJECT_TOKENS: usize = 6;

/// The most characters a plausible subject (or entity-object) surface may carry — a second,
/// length-based guard so a few very long tokens cannot slip past [`MAX_SUBJECT_TOKENS`].
pub const MAX_SUBJECT_CHARS: usize = 80;

/// What a matched rule produces for the object position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectRule {
    /// The object is another entity of this type (resolved to a canonical entity).
    Entity {
        /// The provisional entity type for the object surface.
        entity_type: String,
    },
    /// The object is a text literal (stored as-is, never resolved).
    Text,
}

/// One verb-marker relation the extractor recognizes.
///
/// A sentence containing `" <marker> "` becomes `(subject, predicate, object)` where the
/// subject is the text before the marker and the object the text after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// The space-delimited verb phrase that triggers the rule (e.g. `"works on"`).
    pub marker: String,
    /// The canonical predicate emitted on a match (e.g. `"works_on"`).
    pub predicate: String,
    /// The provisional entity type for the subject surface.
    pub subject_type: String,
    /// How to read the object position.
    pub object: ObjectRule,
    /// The confidence assigned to facts from this rule, in `[0, 1]`.
    pub confidence: f64,
}

/// A deterministic [`FactExtractor`] driven by a fixed ruleset.
#[derive(Debug, Clone)]
pub struct RuleExtractor {
    identity: ExtractorIdentity,
    rules: Vec<Rule>,
    config: ExtractionConfig,
}

impl RuleExtractor {
    /// Build an extractor from an explicit ruleset and rule-set version, with the default
    /// precision gates ([`ExtractionConfig::default`]).
    #[must_use]
    pub fn new(rule_version: impl Into<String>, rules: Vec<Rule>) -> Self {
        Self::with_config(rule_version, rules, ExtractionConfig::default())
    }

    /// Build an extractor from an explicit ruleset, rule-set version, and precision-gate
    /// config (the `min_confidence` floor that filters low-confidence rules).
    #[must_use]
    pub fn with_config(
        rule_version: impl Into<String>,
        rules: Vec<Rule>,
        config: ExtractionConfig,
    ) -> Self {
        Self {
            identity: ExtractorIdentity {
                model_family: None,
                model_version: None,
                rule_version: rule_version.into(),
            },
            rules,
            config,
        }
    }

    /// Build an extractor with a small general-purpose ruleset (`rule-v2`).
    ///
    /// Only the four **typed entity** rules ship: `works_on`/`based_in`/`prefers`/`uses`,
    /// each resolving its object to a canonical entity. The earlier free-text `is a`→`is_a`
    /// catch-all was removed (M-precision): it matched any `"X is a Y"` clause and so turned
    /// dependent fragments and bare pronouns into junk facts. The subject (and entity-object)
    /// of every match is now screened by [`is_plausible_subject`], and a rule whose confidence
    /// falls below [`ExtractionConfig::min_confidence`] is skipped entirely.
    #[must_use]
    pub fn with_default_rules() -> Self {
        Self::with_default_rules_and_config(ExtractionConfig::default())
    }

    /// [`with_default_rules`](Self::with_default_rules) with an explicit precision-gate config.
    #[must_use]
    pub fn with_default_rules_and_config(config: ExtractionConfig) -> Self {
        let entity = |entity_type: &str| ObjectRule::Entity {
            entity_type: entity_type.to_string(),
        };
        let rule = |marker: &str, predicate: &str, subject_type: &str, object, confidence| Rule {
            marker: marker.to_string(),
            predicate: predicate.to_string(),
            subject_type: subject_type.to_string(),
            object,
            confidence,
        };
        Self::with_config(
            "rule-v2",
            vec![
                rule("works on", "works_on", "Person", entity("Project"), 0.9),
                rule("is based in", "based_in", "Person", entity("Place"), 0.85),
                rule("prefers", "prefers", "Person", entity("Technology"), 0.85),
                rule("uses", "uses", "Person", entity("Tool"), 0.8),
            ],
            config,
        )
    }

    /// Extract synchronously — the whole of the work; the async seam just wraps this.
    fn extract_sync(&self, episode: &Episode) -> Vec<ExtractedFact> {
        let mut facts = Vec::new();
        for (offset, sentence) in sentences(&episode.content) {
            for rule in &self.rules {
                // Confidence floor: a rule that does not clear `min_confidence` never fires, so
                // a deployment can dial out low-confidence (noisier) rules without recompiling.
                if rule.confidence < self.config.min_confidence {
                    continue;
                }
                let pattern = format!(" {} ", rule.marker);
                let Some(pos) = sentence.find(&pattern) else {
                    continue;
                };
                let subject = sentence[..pos].trim();
                let object = sentence[pos + pattern.len()..]
                    .trim()
                    .trim_end_matches(['.', '!', '?', ',', ';'])
                    .trim();
                if subject.is_empty() || object.is_empty() {
                    continue;
                }
                if looks_like_code_fragment(subject) || looks_like_code_fragment(object) {
                    continue;
                }
                // Precision gate: a leading connective, a bare pronoun, or an over-long clause
                // is not a subject — reject before it can mint a junk entity (the root cause the
                // removed `is a` catch-all exposed). An entity-typed object is held to the same
                // bar; a text-literal object is content stored as-is, so it is exempt.
                if !is_plausible_subject(subject) {
                    continue;
                }
                if let ObjectRule::Entity { .. } = &rule.object
                    && !is_plausible_subject(object)
                {
                    continue;
                }
                facts.push(ExtractedFact {
                    subject: EntitySurface {
                        surface: subject.to_string(),
                        entity_type: rule.subject_type.clone(),
                    },
                    predicate: rule.predicate.clone(),
                    object: match &rule.object {
                        ObjectRule::Entity { entity_type } => {
                            ExtractedObject::Entity(EntitySurface {
                                surface: object.to_string(),
                                entity_type: entity_type.clone(),
                            })
                        }
                        ObjectRule::Text => {
                            ExtractedObject::Literal(ObjectValue::Text(object.to_string()))
                        }
                    },
                    confidence: rule.confidence,
                    statement: sentence.trim().to_string(),
                    source_spans: vec![SourceSpan {
                        episode_id: episode.identity.id,
                        start: offset,
                        end: offset + sentence.len(),
                    }],
                });
            }
        }
        facts
    }
}

fn looks_like_code_fragment(value: &str) -> bool {
    value.contains('`')
        || value.contains("::")
        || value.contains("=>")
        || value.contains("->")
        || value.contains('{')
        || value.contains('}')
        || value.contains('(')
        || value.contains(')')
}

/// Whether `surface` is plausibly a fact subject (or entity-typed object), and not a clause
/// fragment the marker accidentally split off.
///
/// Deterministic and conservative — it rejects three unambiguous non-subject shapes and
/// passes everything else, so a legitimate short proper-noun subject is never turned away:
/// - a leading subordinating/coordinating conjunction or relative connective
///   ([`SUBJECT_LEADING_STOPWORDS`], lowercased EXACT first-token match) — a dependent clause;
/// - a bare unresolved pronoun/deictic as the WHOLE surface
///   ([`SUBJECT_BARE_PRONOUNS`], lowercased full-surface match) — no canonicalizable referent;
/// - a surface over the [`MAX_SUBJECT_TOKENS`] / [`MAX_SUBJECT_CHARS`] budget — a clause, not
///   a name.
///
/// An empty surface is not plausible (the caller already trims and skips empties, so this is
/// belt-and-suspenders).
#[must_use]
pub fn is_plausible_subject(surface: &str) -> bool {
    let trimmed = surface.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Length budget (chars then tokens): a clause is not a subject.
    if trimmed.chars().count() > MAX_SUBJECT_CHARS {
        return false;
    }
    let mut tokens = trimmed.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    let token_count = 1 + tokens.count();
    if token_count > MAX_SUBJECT_TOKENS {
        return false;
    }
    // A leading connective marks a dependent clause (exact lowercased first-token match, so a
    // proper noun that merely contains a stopword as a substring is untouched).
    let first_lower = first.to_lowercase();
    if SUBJECT_LEADING_STOPWORDS.contains(&first_lower.as_str()) {
        return false;
    }
    // A bare pronoun/deictic as the WHOLE surface has no referent to canonicalize.
    let whole_lower = trimmed.to_lowercase();
    if SUBJECT_BARE_PRONOUNS.contains(&whole_lower.as_str()) {
        return false;
    }
    true
}

impl FactExtractor for RuleExtractor {
    type Error = Infallible;

    fn extract(
        &self,
        episode: &Episode,
    ) -> impl Future<Output = Result<Vec<ExtractedFact>, Self::Error>> + Send {
        let facts = self.extract_sync(episode);
        async move { Ok(facts) }
    }

    fn identity(&self) -> &ExtractorIdentity {
        &self.identity
    }
}

/// Split content into `(byte_offset, sentence)` pairs on terminal punctuation and
/// newlines. Offsets index into the original content (the delimiters are all ASCII, so
/// every split point is a char boundary), so they back the [`SourceSpan`] byte ranges.
fn sentences(content: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        if matches!(byte, b'.' | b'\n' | b';' | b'!' | b'?') {
            let segment = &content[start..i];
            if !segment.trim().is_empty() {
                out.push((start, segment));
            }
            start = i + 1;
        }
    }
    if start < content.len() {
        let segment = &content[start..];
        if !segment.trim().is_empty() {
            out.push((start, segment));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::ids::{ContentHash, Id};
    use aionforge_domain::namespace::Namespace;
    use aionforge_domain::nodes::episodic::{ConsolidationState, Episode, Role};

    use super::*;

    #[test]
    fn skips_code_shaped_rule_matches() {
        let episode =
            episode("`capture.agent_id` uses raw UUID while `search.viewer` uses `agent:<uuid>`");
        let facts = RuleExtractor::with_default_rules().extract_sync(&episode);
        assert!(
            facts.is_empty(),
            "code-heavy fragments are not facts: {facts:?}"
        );
    }

    #[test]
    fn still_extracts_plain_prose_matches() {
        let episode = episode("Alice uses Aionforge Memory");
        let facts = RuleExtractor::with_default_rules().extract_sync(&episode);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].predicate, "uses");
        assert_eq!(facts[0].subject.surface, "Alice");
    }

    #[test]
    fn rejects_the_is_a_dependent_clause_exemplar() {
        // The removed `is a`→`is_a` catch-all used to turn this dependent fragment into a
        // junk fact ("which means it is a feature ..."). With that rule gone AND the subject
        // gate live, no typed rule matches and nothing is extracted.
        let episode =
            episode("The release shipped, which means it is a feature users have wanted.");
        let facts = RuleExtractor::with_default_rules().extract_sync(&episode);
        assert!(
            facts.is_empty(),
            "the is_a dependent-clause exemplar yields no fact: {facts:?}"
        );
    }

    #[test]
    fn rejects_a_conjunction_led_subject() {
        // A typed marker still matches, but the subject opens with a connective ("because the
        // build failed"), so the subject gate drops it as a dependent clause.
        let episode = episode("because the build failed she uses a workaround");
        let facts = RuleExtractor::with_default_rules().extract_sync(&episode);
        assert!(
            facts.is_empty(),
            "a conjunction-led subject is rejected: {facts:?}"
        );
    }

    #[test]
    fn rejects_a_bare_pronoun_subject() {
        // "It" alone is an unresolved deictic with no canonicalizable referent, so even a
        // clean typed match is dropped.
        let pronoun = episode("It uses Rust.");
        let facts = RuleExtractor::with_default_rules().extract_sync(&pronoun);
        assert!(
            facts.is_empty(),
            "a bare-pronoun subject is rejected: {facts:?}"
        );
        // But a real name that merely STARTS with pronoun-like text is untouched.
        let proper = episode("Italo uses Rust.");
        let kept = RuleExtractor::with_default_rules().extract_sync(&proper);
        assert_eq!(kept.len(), 1, "a real proper-noun subject still fires");
        assert_eq!(kept[0].subject.surface, "Italo");
    }

    #[test]
    fn rejects_an_over_long_clause_subject() {
        // A subject longer than the token budget is a clause, not a name.
        let episode = episode("Alice Bob Carol Dave Eve Frank Grace uses Rust.");
        let facts = RuleExtractor::with_default_rules().extract_sync(&episode);
        assert!(
            facts.is_empty(),
            "an over-long clause subject is rejected: {facts:?}"
        );
    }

    #[test]
    fn the_default_min_confidence_keeps_every_typed_rule_firing() {
        // The four shipped typed rules carry confidences 0.9/0.85/0.85/0.8; the default
        // `min_confidence` (0.8) is at-or-below the lowest, so all four still fire. (The
        // confidence floor is `<`, so the 0.8 `uses` rule is kept.)
        let cfg = ExtractionConfig::default();
        for rule in RuleExtractor::with_default_rules_and_config(cfg.clone()).rules {
            assert!(
                rule.confidence >= cfg.min_confidence,
                "the default min_confidence ({}) must not silence the typed `{}` rule (conf {})",
                cfg.min_confidence,
                rule.predicate,
                rule.confidence
            );
        }
    }

    #[test]
    fn a_raised_min_confidence_floor_skips_lower_rules() {
        // Raising the floor above `uses` (0.8) silences it but keeps the 0.9 `works_on` rule.
        let cfg = ExtractionConfig {
            min_confidence: 0.81,
            ..ExtractionConfig::default()
        };
        let extractor = RuleExtractor::with_default_rules_and_config(cfg);
        assert!(
            extractor
                .extract_sync(&episode("Alice uses Rust."))
                .is_empty(),
            "the 0.8 `uses` rule is below the raised floor"
        );
        let kept = extractor.extract_sync(&episode("Alice works on Aionforge."));
        assert_eq!(kept.len(), 1, "the 0.9 `works_on` rule still clears 0.81");
        assert_eq!(kept[0].predicate, "works_on");
    }

    #[test]
    fn is_plausible_subject_passes_short_proper_nouns() {
        for ok in ["Alice", "Alice Smith", "Aionforge Memory", "Mary Jane"] {
            assert!(is_plausible_subject(ok), "`{ok}` is a plausible subject");
        }
        for bad in [
            "because the build failed",
            "which she uses",
            "it",
            "this",
            "There",
        ] {
            assert!(
                !is_plausible_subject(bad),
                "`{bad}` is not a plausible subject"
            );
        }
    }

    fn episode(content: &str) -> Episode {
        let now: aionforge_domain::time::Timestamp = "2026-06-06T09:30:00-05:00[America/Chicago]"
            .parse()
            .expect("valid timestamp");
        Episode {
            identity: Identity {
                id: Id::generate(),
                ingested_at: now.clone(),
                namespace: Namespace::Agent("agent".to_string()),
                expired_at: None,
            },
            stats: Stats {
                importance: 0.5,
                trust: 0.8,
                last_access: now.clone(),
                access_count_recent: 0,
                referenced_count: 0,
                surprise: 0.0,
                is_pinned: false,
            },
            content: content.to_string(),
            role: Role::User,
            captured_at: now,
            agent_id: Id::generate(),
            session_id: None,
            content_hash: ContentHash::of(content.as_bytes()),
            embedding: None,
            embedder_model: None,
            consolidation_state: ConsolidationState::Raw,
            origin: None,
        }
    }
}

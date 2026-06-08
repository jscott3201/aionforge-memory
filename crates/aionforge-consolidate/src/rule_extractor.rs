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
}

impl RuleExtractor {
    /// Build an extractor from an explicit ruleset and rule-set version.
    #[must_use]
    pub fn new(rule_version: impl Into<String>, rules: Vec<Rule>) -> Self {
        Self {
            identity: ExtractorIdentity {
                model_family: None,
                model_version: None,
                rule_version: rule_version.into(),
            },
            rules,
        }
    }

    /// Build an extractor with a small general-purpose ruleset (`rule-v1`).
    #[must_use]
    pub fn with_default_rules() -> Self {
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
        Self::new(
            "rule-v1",
            vec![
                rule("works on", "works_on", "Person", entity("Project"), 0.9),
                rule("is based in", "based_in", "Person", entity("Place"), 0.85),
                rule("prefers", "prefers", "Person", entity("Technology"), 0.85),
                rule("uses", "uses", "Person", entity("Tool"), 0.8),
                rule("is a", "is_a", "Entity", ObjectRule::Text, 0.7),
            ],
        )
    }

    /// Extract synchronously — the whole of the work; the async seam just wraps this.
    fn extract_sync(&self, episode: &Episode) -> Vec<ExtractedFact> {
        let mut facts = Vec::new();
        for (offset, sentence) in sentences(&episode.content) {
            for rule in &self.rules {
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

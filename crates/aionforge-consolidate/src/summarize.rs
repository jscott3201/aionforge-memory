//! Clustering and the detail-retention guard for conservative summarization (M2.T06).
//!
//! Pure, store-free building blocks the fact-extraction pass calls after detection: group a
//! subject's facts into a cluster worth condensing, derive the cluster's content-addressed
//! note id, and check that a produced summary preserves enough of the cluster's specificity
//! before it is written. Being store-free, every branch is unit-testable.

use std::collections::BTreeMap;

use aionforge_domain::contracts::{SummarizationCluster, SummaryOutput};
use aionforge_domain::ids::Id;
use aionforge_domain::namespace::Namespace;
use aionforge_domain::nodes::semantic::Fact;
use aionforge_domain::value::ObjectValue;

use crate::config::SummarizationConfig;

/// Group facts by subject into clusters worth summarizing.
///
/// `name_of` resolves an entity id to its canonical name (the subject and entity-typed
/// objects); an id with no name falls back to its string form. Only clusters meeting the
/// size gates (`>= min_facts` and `>= min_entities` distinct entities) are returned — the
/// conservative "worth condensing" filter. Output order and per-cluster fact order are
/// deterministic (sorted by id), so the downstream note id is reproducible.
pub(crate) fn build_clusters(
    facts: &[Fact],
    name_of: &BTreeMap<Id, String>,
    cfg: &SummarizationConfig,
) -> Vec<SummarizationCluster> {
    let mut by_subject: BTreeMap<Id, Vec<Fact>> = BTreeMap::new();
    for fact in facts {
        by_subject
            .entry(fact.subject_id)
            .or_default()
            .push(fact.clone());
    }

    let mut clusters = Vec::new();
    for (subject_id, mut subject_facts) in by_subject {
        subject_facts.sort_by_key(|a| a.identity.id);
        subject_facts.dedup_by(|a, b| a.identity.id == b.identity.id);
        let entity_names = distinct_entity_names(&subject_id, &subject_facts, name_of);
        if subject_facts.len() < cfg.min_facts || entity_names.len() < cfg.min_entities {
            continue;
        }
        let subject_name = name_of
            .get(&subject_id)
            .cloned()
            .unwrap_or_else(|| subject_id.to_string());
        clusters.push(SummarizationCluster {
            subject_id,
            subject_name,
            facts: subject_facts,
            entity_names,
        });
    }
    clusters
}

/// The distinct entity names a cluster references: the subject plus every entity-typed
/// object, named via `name_of` (the id string as a fallback). Sorted, deduped.
fn distinct_entity_names(
    subject_id: &Id,
    facts: &[Fact],
    name_of: &BTreeMap<Id, String>,
) -> Vec<String> {
    let name = |id: &Id| name_of.get(id).cloned().unwrap_or_else(|| id.to_string());
    let mut names = vec![name(subject_id)];
    for fact in facts {
        if let ObjectValue::Entity(id) = &fact.object {
            names.push(name(id));
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The content-addressed id of a cluster's summary note: a hash over the namespace, the
/// subject, the sorted source fact-id set, and the summarizer rule version. Re-running the
/// same episode yields the same set and so the same id (a replay is a no-op); adding a fact
/// later is a different set and so a different note (the old one is kept — non-lossy).
pub(crate) fn note_id(
    namespace: &Namespace,
    cluster: &SummarizationCluster,
    rule_version: &str,
) -> Id {
    let mut ids: Vec<String> = cluster
        .facts
        .iter()
        .map(|f| f.identity.id.to_string())
        .collect();
    ids.sort_unstable();
    let key = format!(
        "{}|{}|{}|{}",
        namespace,
        cluster.subject_id,
        ids.join(","),
        rule_version,
    );
    Id::from_content_hash(key.as_bytes())
}

/// The outcome of the detail-retention guard, with the metrics for the audit trail.
pub(crate) struct RetentionOutcome {
    /// Whether the summary preserves enough specificity to be written.
    pub passed: bool,
    /// The fraction of the cluster's distinct entities the summary names.
    pub entity_retention: f64,
    /// The mean confidence of the cluster's source facts.
    pub mean_confidence: f64,
}

/// Check that a produced summary preserves enough of the cluster's specificity to be worth
/// writing — M2.T06's over-summarization guard, the safety net for any summarizer (the M2
/// rule summarizer passes by construction; a future model summarizer may not).
///
/// Two independent, deterministic checks, both required: the fraction of distinct source
/// entities named in the summary (content or keywords) must clear `entity_retention_threshold`,
/// and the cluster's mean source-fact confidence must clear `confidence_floor`. A summary
/// that fails either is skipped, not written, so no lossy artifact lands.
pub(crate) fn check_detail_retention(
    cluster: &SummarizationCluster,
    output: &SummaryOutput,
    cfg: &SummarizationConfig,
) -> RetentionOutcome {
    let haystack = format!("{} {}", output.content, output.keywords.join(" ")).to_lowercase();
    let total = cluster.entity_names.len().max(1);
    let preserved = cluster
        .entity_names
        .iter()
        .filter(|name| contains_word(&haystack, &name.to_lowercase()))
        .count();
    let entity_retention = preserved as f64 / total as f64;
    let mean_confidence = if cluster.facts.is_empty() {
        0.0
    } else {
        cluster.facts.iter().map(|f| f.confidence).sum::<f64>() / cluster.facts.len() as f64
    };
    let passed = entity_retention >= cfg.entity_retention_threshold
        && mean_confidence >= cfg.confidence_floor;
    RetentionOutcome {
        passed,
        entity_retention,
        mean_confidence,
    }
}

/// Whole-word containment: does `needle` occur in `haystack` bounded on each side by a
/// non-alphanumeric character (or a string edge)? Both arguments are expected pre-lowercased.
///
/// A plain substring test would count an entity as "retained" merely because its name is a
/// fragment of a longer, unrelated word in the summary — `"Bo"` inside `"Bobby"`. That is a
/// false positive on the guard's one job (catching dropped entities), inflating retention and
/// letting a lossy summary through, so the test must respect word boundaries. Internal content
/// of a multi-word `needle` (`"new york"`) is matched literally; only the two outer edges are
/// checked, so multi-word names work without tokenizing.
///
/// Shared across the summarizers behind the [`Summarizer`](aionforge_domain::contracts::Summarizer)
/// seam: a summarizer derives a note's keywords as the entity names actually present in the
/// generated prose — by this same whole-word rule, so the keywords it adds are a subset of what the
/// guard already finds in the content and cannot inflate retention past an honest reading of the
/// summary.
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        // Advance one char past this occurrence's start to find a later one.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge_domain::blocks::{Identity, Stats};
    use aionforge_domain::nodes::semantic::{Fact, FactStatus};
    use aionforge_domain::time::Timestamp;

    fn ts() -> Timestamp {
        "2026-06-06T09:00:00Z[UTC]"
            .parse()
            .expect("valid zoned datetime")
    }

    fn ns() -> Namespace {
        Namespace::Agent("tester".to_string())
    }

    fn stats(confidence: f64) -> Stats {
        let _ = confidence;
        Stats {
            importance: 0.5,
            trust: 0.9,
            last_access: ts(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.0,
            is_pinned: false,
        }
    }

    fn fact(id: &str, subject: &Id, predicate: &str, object: ObjectValue, confidence: f64) -> Fact {
        Fact {
            identity: Identity {
                id: Id::from_content_hash(id.as_bytes()),
                ingested_at: ts(),
                namespace: ns(),
                expired_at: None,
            },
            stats: stats(confidence),
            subject_id: *subject,
            predicate: predicate.to_string(),
            object,
            confidence,
            status: FactStatus::Active,
            statement: String::new(),
            embedding: None,
            embedder_model: None,
            extraction: None,
            cooled_until: None,
        }
    }

    fn names(pairs: &[(&Id, &str)]) -> BTreeMap<Id, String> {
        pairs
            .iter()
            .map(|(id, name)| (*(*id), (*name).to_string()))
            .collect()
    }

    #[test]
    fn a_cluster_forms_only_when_it_clears_the_size_gates() {
        let cfg = SummarizationConfig::default(); // min_facts 3, min_entities 2
        let alice = Id::from_content_hash(b"alice");
        let nyc = Id::from_content_hash(b"nyc");
        let aionforge = Id::from_content_hash(b"aionforge");
        let name_of = names(&[(&alice, "Alice"), (&nyc, "NYC"), (&aionforge, "Aionforge")]);
        let facts = vec![
            fact("f1", &alice, "based_in", ObjectValue::Entity(nyc), 0.9),
            fact(
                "f2",
                &alice,
                "works_on",
                ObjectValue::Entity(aionforge),
                0.9,
            ),
            fact(
                "f3",
                &alice,
                "prefers",
                ObjectValue::Text("Rust".to_string()),
                0.9,
            ),
        ];

        let clusters = build_clusters(&facts, &name_of, &cfg);
        assert_eq!(
            clusters.len(),
            1,
            "three facts, three entities -> one cluster"
        );
        assert_eq!(clusters[0].facts.len(), 3);
        assert!(clusters[0].entity_names.contains(&"Alice".to_string()));
        assert!(clusters[0].entity_names.contains(&"NYC".to_string()));

        // Two facts is below min_facts -> no cluster.
        let small = build_clusters(&facts[..2], &name_of, &cfg);
        assert!(
            small.is_empty(),
            "below min_facts: nothing worth condensing"
        );
    }

    #[test]
    fn the_note_id_is_stable_for_a_fact_set_and_changes_when_it_grows() {
        let cfg = SummarizationConfig {
            min_facts: 1,
            min_entities: 1,
            ..SummarizationConfig::default()
        };
        let alice = Id::from_content_hash(b"alice");
        let nyc = Id::from_content_hash(b"nyc");
        let name_of = names(&[(&alice, "Alice"), (&nyc, "NYC")]);
        let f1 = fact("f1", &alice, "based_in", ObjectValue::Entity(nyc), 0.9);
        let f2 = fact(
            "f2",
            &alice,
            "prefers",
            ObjectValue::Text("Rust".to_string()),
            0.9,
        );

        let one = build_clusters(std::slice::from_ref(&f1), &name_of, &cfg);
        let one_again = build_clusters(std::slice::from_ref(&f1), &name_of, &cfg);
        assert_eq!(
            note_id(&ns(), &one[0], "summarize-v1"),
            note_id(&ns(), &one_again[0], "summarize-v1"),
            "same fact set -> same id (replay is a no-op)"
        );

        let two = build_clusters(&[f1, f2], &name_of, &cfg);
        assert_ne!(
            note_id(&ns(), &one[0], "summarize-v1"),
            note_id(&ns(), &two[0], "summarize-v1"),
            "a grown fact set -> a different id (a new note, the old one kept)"
        );
    }

    #[test]
    fn the_guard_passes_a_faithful_summary_and_blocks_a_lossy_one() {
        let cfg = SummarizationConfig::default(); // entity threshold 0.9, confidence floor 0.6
        let alice = Id::from_content_hash(b"alice");
        let nyc = Id::from_content_hash(b"nyc");
        let aionforge = Id::from_content_hash(b"aionforge");
        let name_of = names(&[(&alice, "Alice"), (&nyc, "NYC"), (&aionforge, "Aionforge")]);
        let facts = vec![
            fact("f1", &alice, "based_in", ObjectValue::Entity(nyc), 0.9),
            fact(
                "f2",
                &alice,
                "works_on",
                ObjectValue::Entity(aionforge),
                0.9,
            ),
            fact(
                "f3",
                &alice,
                "prefers",
                ObjectValue::Text("Rust".to_string()),
                0.9,
            ),
        ];
        let cluster = build_clusters(&facts, &name_of, &cfg)
            .pop()
            .expect("one cluster");

        // A faithful summary names every entity -> retention 1.0 -> passes.
        let faithful = SummaryOutput {
            content: "Alice — 3 facts. Entities: Aionforge, Alice, NYC.".to_string(),
            keywords: vec![
                "Aionforge".to_string(),
                "Alice".to_string(),
                "NYC".to_string(),
            ],
            context: None,
        };
        assert!(check_detail_retention(&cluster, &faithful, &cfg).passed);

        // A lossy summary that drops two of three entities -> retention 1/3 -> blocked.
        let lossy = SummaryOutput {
            content: "Alice did some things.".to_string(),
            keywords: vec!["Alice".to_string()],
            context: None,
        };
        let outcome = check_detail_retention(&cluster, &lossy, &cfg);
        assert!(!outcome.passed, "an over-summarized note is blocked");
        assert!(outcome.entity_retention < cfg.entity_retention_threshold);
    }

    #[test]
    fn contains_word_respects_boundaries() {
        // Substring of a longer word is not a match.
        assert!(!contains_word("bobby went home", "bo"));
        // Standalone occurrence is a match, even alongside a substring occurrence.
        assert!(contains_word("bobby and bo", "bo"));
        // Multi-word names match literally, bounded by edges/punctuation.
        assert!(contains_word("alice lives in new york.", "new york"));
        assert!(!contains_word("newyork is not the city", "new york"));
        // String edges count as boundaries.
        assert!(contains_word("bo", "bo"));
        assert!(contains_word("rust, go, bo", "bo"));
        assert!(!contains_word("", "bo"));
        assert!(!contains_word("anything", ""));
    }

    #[test]
    fn the_guard_does_not_credit_a_substring_only_mention() {
        let cfg = SummarizationConfig::default(); // entity threshold 0.9, confidence floor 0.6
        let bo = Id::from_content_hash(b"bo");
        let nyc = Id::from_content_hash(b"nyc");
        let aionforge = Id::from_content_hash(b"aionforge");
        let name_of = names(&[(&bo, "Bo"), (&nyc, "NYC"), (&aionforge, "Aionforge")]);
        let facts = vec![
            fact("f1", &bo, "based_in", ObjectValue::Entity(nyc), 0.9),
            fact("f2", &bo, "works_on", ObjectValue::Entity(aionforge), 0.9),
            fact("f3", &bo, "prefers", ObjectValue::Text("Rust".into()), 0.9),
        ];
        let cluster = build_clusters(&facts, &name_of, &cfg)
            .pop()
            .expect("one cluster");

        // "Bobby" mentions the subject only as a substring; a plain `contains` would have
        // counted "Bo" as retained (3/3) and passed this lossy summary. Word boundaries drop
        // it to 2/3, below the threshold, so the over-summarized note is blocked.
        let lossy = SummaryOutput {
            content: "Bobby is connected to Aionforge and NYC.".to_string(),
            keywords: vec!["Aionforge".to_string(), "NYC".to_string()],
            context: None,
        };
        let outcome = check_detail_retention(&cluster, &lossy, &cfg);
        assert!(!outcome.passed, "a substring-only mention must not pass");
        assert!(outcome.entity_retention < cfg.entity_retention_threshold);
    }

    #[test]
    fn the_guard_blocks_a_low_confidence_cluster() {
        let cfg = SummarizationConfig::default(); // confidence floor 0.6
        let alice = Id::from_content_hash(b"alice");
        let nyc = Id::from_content_hash(b"nyc");
        let aionforge = Id::from_content_hash(b"aionforge");
        let name_of = names(&[(&alice, "Alice"), (&nyc, "NYC"), (&aionforge, "Aionforge")]);
        let facts = vec![
            fact("f1", &alice, "based_in", ObjectValue::Entity(nyc), 0.4),
            fact(
                "f2",
                &alice,
                "works_on",
                ObjectValue::Entity(aionforge),
                0.4,
            ),
            fact(
                "f3",
                &alice,
                "prefers",
                ObjectValue::Text("Rust".to_string()),
                0.4,
            ),
        ];
        let cluster = build_clusters(&facts, &name_of, &cfg)
            .pop()
            .expect("one cluster");
        let faithful = SummaryOutput {
            content: "Alice — 3 facts. Entities: Aionforge, Alice, NYC.".to_string(),
            keywords: vec![
                "Aionforge".to_string(),
                "Alice".to_string(),
                "NYC".to_string(),
            ],
            context: None,
        };
        let outcome = check_detail_retention(&cluster, &faithful, &cfg);
        assert!(
            !outcome.passed,
            "a thin, low-confidence cluster is not summarized"
        );
        assert!(outcome.mean_confidence < cfg.confidence_floor);
    }
}

//! Measures the capture-side marker filter against a published injection corpus
//! (`deepset/prompt-injections`) and a benign trigger-word corpus (`leolee99/NotInject`)
//! — the M6.T03 acceptance basis (07 §2, §5): block rate on the injection corpus and
//! false-positive rate on benign dialogue. Corpus provenance, licensing, curation, and
//! the secret scrub are recorded in `corpus/PROVENANCE.md`.
//!
//! This PR introduces the corpus and the measurement harness and asserts the
//! **false-positive ceiling** (the marker set fires on no benign row, 07 §5) plus the
//! curation invariants. It deliberately does NOT yet assert the block-rate **floor**:
//! that gate is pinned in a later PR against the hardened marker set's observed number,
//! measure-first, so the "thresholds are never relaxed to pass" discipline (07 §5) is
//! visible in the diff that introduces it rather than buried in the corpus import.
//!
//! Block definition: a `label==1` (injection) row is **blocked** iff the filter returns
//! a non-empty `injection_flags` (at least one marker fired). False positive: a benign
//! row whose `injection_flags` is non-empty. The block rate is computed over the FULL
//! injection set — rows whose only injection signal is semantic, with no string-matchable
//! override phrase, fall in a "no-phrase" bucket but are never subtracted from the
//! denominator.

use std::collections::BTreeMap;

use aionforge_domain::PrivacyFilter;
use aionforge_security::CaptureFilter;
use serde_json::Value;

const DEEPSET: &str = include_str!("corpus/deepset_injections.jsonl");
const NOTINJECT: &str = include_str!("corpus/notinject_benign.jsonl");

struct Row {
    text: String,
    label: Option<i64>,
}

/// Parse one JSONL fixture. `expect_label` requires an integer `label` per row (the
/// deepset block corpus); the benign corpus carries only `text`.
fn parse(jsonl: &str, expect_label: bool) -> Vec<Row> {
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let v: Value = serde_json::from_str(line).expect("corpus row is valid JSON");
            // A missing/null `text` here is exactly the NotInject `prompt`->`text`
            // curation gotcha; failing loudly is the point.
            let text = v
                .get("text")
                .and_then(Value::as_str)
                .expect("corpus row has a non-null string `text`")
                .to_string();
            let label = expect_label.then(|| {
                v.get("label")
                    .and_then(Value::as_i64)
                    .expect("injection row has an integer `label`")
            });
            Row { text, label }
        })
        .collect()
}

/// Assert a fixture trips none of the five `check-no-secrets.sh` patterns, so a future
/// corpus refresh that pulled in a secret-shaped row cannot silently red the
/// whole-workspace no-secret gate (the scrub is also applied at curation time).
fn assert_secret_free(name: &str, rows: &[Row]) {
    let patterns = [
        r"sk-[A-Za-z0-9_-]{20,}",
        r"AKIA[0-9A-Z]{16}",
        r"xox[abpr]-[A-Za-z0-9-]{10,}",
        r"gh[pousr]_[A-Za-z0-9]{36,}",
        r"-----BEGIN (RSA|EC|OPENSSH|PGP) PRIVATE KEY-----",
    ];
    for pattern in patterns {
        let re = regex::Regex::new(pattern).expect("secret pattern compiles");
        for row in rows {
            assert!(
                !re.is_match(&row.text),
                "{name}: a secret-shaped substring matched /{pattern}/"
            );
        }
    }
}

/// The block/false-positive measurement over an injection set and a benign set. Returned
/// so the (later) threshold gate can assert on the same numbers this harness computes.
struct Report {
    n_injection: usize,
    blocked: usize,
    n_benign: usize,
    false_positives: usize,
    per_marker: BTreeMap<String, u64>,
}

fn measure(filter: &CaptureFilter, injection: &[&str], benign: &[&str]) -> Report {
    let mut blocked = 0;
    let mut per_marker: BTreeMap<String, u64> = BTreeMap::new();
    for text in injection {
        let out = filter.filter(text).expect("filter");
        if !out.injection_flags.is_empty() {
            blocked += 1;
        }
        for (id, count) in out.marker_hits {
            *per_marker.entry(id).or_default() += u64::from(count);
        }
    }
    let false_positives = benign
        .iter()
        .filter(|text| {
            !filter
                .filter(text)
                .expect("filter")
                .injection_flags
                .is_empty()
        })
        .count();
    Report {
        n_injection: injection.len(),
        blocked,
        n_benign: benign.len(),
        false_positives,
        per_marker,
    }
}

#[test]
fn corpus_curates_clean_and_holds_the_false_positive_ceiling() {
    let deepset = parse(DEEPSET, true);
    let notinject = parse(NOTINJECT, false);

    // Curation invariants — catch corpus corruption and the `prompt`->`text` gotcha
    // that would otherwise make a benign-FP measurement vacuously zero.
    assert_eq!(
        deepset.len(),
        662,
        "deepset row count drifted from the snapshot"
    );
    assert_eq!(
        notinject.len(),
        339,
        "notinject row count drifted from the snapshot"
    );
    assert!(
        deepset.iter().any(|r| r.label == Some(1)),
        "deepset has injection rows"
    );
    assert!(
        deepset.iter().any(|r| r.label == Some(0)),
        "deepset has benign rows"
    );
    assert!(
        notinject.iter().all(|r| !r.text.is_empty()),
        "every benign row carries non-empty text"
    );
    assert_secret_free("deepset", &deepset);
    assert_secret_free("notinject", &notinject);

    let injection: Vec<&str> = deepset
        .iter()
        .filter(|r| r.label == Some(1))
        .map(|r| r.text.as_str())
        .collect();
    let deepset_benign: Vec<&str> = deepset
        .iter()
        .filter(|r| r.label == Some(0))
        .map(|r| r.text.as_str())
        .collect();
    let notinject_text: Vec<&str> = notinject.iter().map(|r| r.text.as_str()).collect();
    let all_benign: Vec<&str> = deepset_benign
        .iter()
        .chain(notinject_text.iter())
        .copied()
        .collect();

    let filter = CaptureFilter::with_defaults().expect("default patterns compile");
    let report = measure(&filter, &injection, &all_benign);

    assert_eq!(
        report.n_injection, 263,
        "full injection denominator — never reduced to make a rate pass (07 §5)"
    );
    assert_eq!(
        report.n_benign, 738,
        "benign set = deepset label==0 (399) + all NotInject (339)"
    );

    // FALSE-POSITIVE CEILING (07 §5): the marker set fires on NO benign row. This holds
    // from corpus introduction and the hardened marker set (next PR) must preserve it.
    // NotInject is a subset of `all_benign`, so zero here is zero on NotInject too.
    assert_eq!(
        report.false_positives, 0,
        "a benign row tripped a marker — false positive"
    );

    // The filter is not inert — it blocks some injections and the per-marker tally is
    // populated (the M6.T03 hit-count plumbing). The block-rate FLOOR is pinned in the
    // gate PR against the hardened set's observed number: measure-first, never relaxed to
    // pass. Observed here with the default (pre-M6.T03) markers: blocked 5/263 = 1.9%,
    // per-marker {ignore_previous: 4, you_are_now: 1}. The remaining ~98% is the no-phrase
    // bucket a capture-time string filter cannot reach (semantic / role-play injection,
    // handled recall-side in M6.T02 and by the M6.T04 probes).
    assert!(
        report.blocked > 0,
        "the marker filter blocks at least some injections"
    );
    assert!(
        !report.per_marker.is_empty(),
        "per-marker hit tally is populated"
    );
}

//! Measures the capture-side marker filter against a published injection corpus
//! (`deepset/prompt-injections`) and a benign trigger-word corpus (`leolee99/NotInject`)
//! — the M6.T03 acceptance basis (07 §2, §5): block rate on the injection corpus and
//! false-positive rate on benign dialogue. Corpus provenance, licensing, curation, and
//! the secret scrub are recorded in `corpus/PROVENANCE.md`.
//!
//! This is the binding release gate (07 §5: a failing probe blocks release; thresholds
//! are never relaxed to pass). It asserts the curation invariants, the benign
//! false-positive **ceiling**, and the injection block-rate **floor** — the floor pinned
//! against the hardened marker set's observed number (measure-first; the observed numbers
//! are recorded next to the constants so any later downward relaxation is visible in the
//! diff).
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

/// Binding thresholds (07 §5 — never relaxed to pass). Observed with the hardened M6.T03
/// marker set: block 31/263 = 0.118, benign false positives 0/738.
///
/// The floor is pinned strictly inside the observed headroom, so an unrelated regression
/// is caught with a little margin. If a future change drops block-rate below the floor,
/// the fix is to harden the markers — NOT to lower `MIN_BLOCK_RATE`. A capture-time phrase
/// filter cannot reach the ~88% of injections that carry no override phrase, so the floor
/// is an honest statement of known-phrase coverage over the full injection set, not a
/// claim of injection immunity. If the zero-FP ceiling and the block floor ever conflict,
/// the FP ceiling wins (drop the offending broad marker).
const MIN_BLOCK_RATE: f64 = 0.10;
const MAX_FP_RATE: f64 = 0.0;

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
fn corpus_meets_block_floor_and_false_positive_ceiling() {
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

    // Denominator integrity: the full corpus, never trimmed to make a rate pass (07 §5).
    assert_eq!(report.n_injection, 263, "full injection denominator");
    assert_eq!(
        report.n_benign, 738,
        "benign set = deepset label==0 (399) + all NotInject (339)"
    );

    let block_rate = report.blocked as f64 / report.n_injection as f64;
    let fp_rate = report.false_positives as f64 / report.n_benign as f64;

    // FALSE-POSITIVE CEILING: the marker set fires on no benign row (NotInject is a subset
    // of the benign set, so zero here is zero on NotInject too).
    assert!(
        fp_rate <= MAX_FP_RATE,
        "benign false-positive rate {:.4} ({}/{}) exceeded the ceiling {} (07 §5)",
        fp_rate,
        report.false_positives,
        report.n_benign,
        MAX_FP_RATE
    );
    // BLOCK-RATE FLOOR over the full injection set. Observed 31/263 = 0.118; per-marker
    // {forget_everything: 15, ignore_or_forget_context: 14, reveal_system_prompt: 4,
    // role_override_persona: 1}. The remaining ~88% is the no-phrase bucket a capture-time
    // string filter cannot reach. If this regresses, harden the markers — never lower the
    // floor to pass (07 §5).
    assert!(
        block_rate >= MIN_BLOCK_RATE,
        "block rate {:.4} ({}/{}) fell below the floor {}; harden markers, do not relax it (07 §5)",
        block_rate,
        report.blocked,
        report.n_injection,
        MIN_BLOCK_RATE
    );
    assert!(
        !report.per_marker.is_empty(),
        "per-marker hit tally is populated"
    );
}

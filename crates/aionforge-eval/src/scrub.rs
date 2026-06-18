//! A secret/PII scrub gate over fixture text.
//!
//! Any corpus that becomes an evaluation baseline must be free of secrets, credentials,
//! personal data, and machine-local provenance. This runs a small regex table over every
//! fixture string and reports matches, so a hand-authored fixture is gated before it is
//! trusted. The table mirrors the retrieval crate's project-corpus scrub rules.

use std::sync::LazyLock;

use regex::Regex;

/// The scrub patterns, compiled once. Each is a `(name, regex)` pair; a match in any
/// fixture string is a violation.
static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("api-key", r"sk-[A-Za-z0-9_-]{20,}"),
        ("aws-key", r"AKIA[0-9A-Z]{16}"),
        ("slack-token", r"xox[abpr]-[A-Za-z0-9-]{10,}"),
        ("github-token", r"gh[pousr]_[A-Za-z0-9]{36,}"),
        (
            "private-key",
            r"-----BEGIN (RSA|EC|OPENSSH|PGP) PRIVATE KEY-----",
        ),
        ("email", r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b"),
        (
            "uuid",
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        ),
        (
            "machine-path",
            r"(/Users/|/private/var/folders|/Volumes/|~/|[A-Za-z]:\\)",
        ),
        (
            "planning-note",
            r"(?i)\b(_briefs|codex-handoffs|handoff|brief-[0-9]+|stage[ -]?[0-9]+)\b",
        ),
    ]
    .into_iter()
    .map(|(name, pattern)| (name, Regex::new(pattern).expect("scrub regex compiles")))
    .collect()
});

/// Scan `(id, text)` items and return one message per scrub violation.
///
/// An empty result means the fixture is clean. Each violation reads `"<id>: matched
/// <pattern-name>"`.
pub fn scrub_violations<'a>(items: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let mut violations = Vec::new();
    for (id, text) in items {
        for (name, regex) in PATTERNS.iter() {
            if regex.is_match(text) {
                violations.push(format!("{id}: matched {name}"));
            }
        }
    }
    violations
}

/// Return `text` with scrub-pattern matches replaced by stable placeholders.
///
/// Use this only for external benchmark corpora that must pass through the same
/// adapter scrub gate as hand-authored fixtures. The placeholders preserve enough
/// local context for retrieval while ensuring raw secrets, identifiers, local paths,
/// or process notes do not enter the seeded store.
#[must_use]
pub fn redact_scrub_patterns(text: &str) -> String {
    let mut redacted = text.to_string();
    for (name, regex) in PATTERNS.iter() {
        let replacement = match *name {
            "email" => "[email]",
            "uuid" => "[uuid]",
            "machine-path" => "[path]",
            "planning-note" => "[planning-note]",
            _ => "[secret]",
        };
        redacted = regex.replace_all(&redacted, replacement).into_owned();
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_has_no_violations() {
        let items = [("m-1", "turn the compost pile every two weeks")];
        assert!(scrub_violations(items).is_empty());
    }

    #[test]
    fn an_email_is_flagged() {
        let items = [("m-1", "ping me at someone@example.com about it")];
        let violations = scrub_violations(items);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("email"), "{violations:?}");
    }

    #[test]
    fn a_machine_path_is_flagged() {
        let items = [("q-1", "the file at /Users/someone/notes.md")];
        assert!(
            scrub_violations(items)
                .iter()
                .any(|v| v.contains("machine-path"))
        );
    }

    #[test]
    fn redaction_removes_scrub_matches() {
        let text = "image 568ba110-4d9e-4023-a2bd-5ffd7528e72a from /Users/alice";
        let redacted = redact_scrub_patterns(text);
        assert!(redacted.contains("[uuid]"));
        assert!(redacted.contains("[path]"));
        assert!(scrub_violations([("m-1", redacted.as_str())]).is_empty());
    }
}

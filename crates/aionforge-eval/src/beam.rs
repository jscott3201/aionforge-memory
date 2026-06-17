//! BEAM benchmark adapter: a loader for a normalized slice of the BEAM long-term-memory
//! benchmark, used by the on-demand source-recall-under-floor runner.
//!
//! BEAM ([github.com/mohammadtavakoli78/BEAM](https://github.com/mohammadtavakoli78/BEAM),
//! dataset `Mohammadta/BEAM`) is **CC BY-SA 4.0** and is **never vendored into this
//! repository**. The `tools/prepare_beam.py` step reads it from the local HuggingFace
//! cache and writes a normalized JSONL to an external path under `~/.aionforge/beam-data`;
//! the `#[ignore]` runner reads *that* path. This module only parses the normalized form —
//! it carries no BEAM data itself.
//!
//! Each JSONL line is one conversation: its messages (the episodes to seed) and its probing
//! questions (the queries). A probe references its evidence messages by id in `source_ids`
//! — the retrieval *gold*. A probe with no resolvable source (the BEAM `abstention` ability)
//! is a negative whose correct recall is empty, so a healthy dense floor must still reject
//! it.

use serde::Deserialize;

/// One conversation message — seeded into the store as an episode.
#[derive(Debug, Clone, Deserialize)]
pub struct BeamMessage {
    /// Stable per-conversation message id (e.g. `msg-28`), joined to the seeded domain id.
    pub id: String,
    /// The message text, embedded by the real embedder and stored verbatim.
    pub text: String,
    /// The speaker role (`user` / `assistant`); informational.
    #[serde(default)]
    pub role: String,
    /// The BEAM time anchor for the message, if any (informational; not parsed as a clock).
    #[serde(default)]
    pub time_anchor: Option<String>,
}

/// One probing question — a recall query with its evidence-message gold.
#[derive(Debug, Clone, Deserialize)]
pub struct BeamProbe {
    /// Stable probe id (`<conversation>::<ability>::<index>`).
    pub id: String,
    /// The BEAM memory ability this probe exercises (e.g. `information_extraction`).
    pub ability: String,
    /// The natural-language question.
    pub question: String,
    /// The evidence message ids that answer the probe (the recall gold), in `BeamMessage.id`
    /// space. Empty for a negative.
    #[serde(default)]
    pub source_ids: Vec<String>,
    /// `true` when the probe has no resolvable evidence (BEAM `abstention`): a negative whose
    /// correct recall is empty.
    #[serde(default)]
    pub expected_empty: bool,
}

impl BeamProbe {
    /// Whether this probe is a negative (no gold; correct recall is empty).
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.expected_empty || self.source_ids.is_empty()
    }
}

/// One BEAM conversation: the messages to seed and the probes to ask against them. BEAM
/// builds a retriever once per conversation, so the measurement seeds and queries each
/// conversation in isolation and aggregates across them.
#[derive(Debug, Clone, Deserialize)]
pub struct BeamConversation {
    /// The BEAM conversation id.
    pub conversation_id: String,
    /// The conversation title (informational).
    #[serde(default)]
    pub title: String,
    /// The conversation's messages, in order.
    pub messages: Vec<BeamMessage>,
    /// The conversation's probing questions.
    pub probes: Vec<BeamProbe>,
}

/// Parse normalized BEAM conversations from a JSONL string (one conversation per line).
///
/// # Errors
/// Returns the first JSON parse error encountered.
pub fn parse_conversations(jsonl: &str) -> serde_json::Result<Vec<BeamConversation>> {
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        r#"{"conversation_id":"1","title":"Demo","messages":[{"id":"msg-0","text":"hi","role":"user","time_anchor":"March-15-2024"},{"id":"msg-1","text":"reply","role":"assistant"}],"#,
        r#""probes":[{"id":"1::information_extraction::0","ability":"information_extraction","question":"q?","source_ids":["msg-1"]},"#,
        r#"{"id":"1::abstention::0","ability":"abstention","question":"unknowable?","source_ids":[],"expected_empty":true}]}"#,
        "\n"
    );

    #[test]
    fn parses_messages_and_probes() {
        let convs = parse_conversations(SAMPLE).expect("parse");
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.conversation_id, "1");
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].time_anchor.as_deref(), Some("March-15-2024"));
        assert_eq!(c.probes.len(), 2);
    }

    #[test]
    fn a_probe_with_source_ids_is_positive_and_abstention_is_negative() {
        let c = &parse_conversations(SAMPLE).expect("parse")[0];
        let positive = &c.probes[0];
        assert!(!positive.is_negative());
        assert_eq!(positive.source_ids, vec!["msg-1"]);
        let negative = &c.probes[1];
        assert!(negative.is_negative());
        assert!(negative.source_ids.is_empty());
    }

    #[test]
    fn a_blank_line_is_skipped() {
        assert!(parse_conversations("\n\n").expect("parse").is_empty());
    }
}

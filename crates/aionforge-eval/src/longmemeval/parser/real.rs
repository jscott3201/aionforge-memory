//! Parser for the cleaned public LongMemEval release shape.

use aionforge_domain::time::Timestamp;
use serde_json::{Map, Value};

use super::{
    array_field, bool_field, fallback_time, labels_from_fields, number_field, offset_time,
    parse_time, role_from_str, session_id, string_field, strings_array_field, turn_content,
};
use crate::scrub::redact_scrub_patterns;
use crate::{
    GoldGranularity, IngestSession, IngestTurn, LongMemEvalCorpus, LongMemEvalError,
    LongMemEvalGold, LongMemEvalQuestion,
};

pub(super) fn is_real_question_record(map: &Map<String, Value>) -> bool {
    map.get("haystack_session_ids").is_some_and(Value::is_array)
        && map.get("haystack_sessions").is_some_and(Value::is_array)
        && map.get("question").is_some()
}

pub(super) fn parse_real_question_case(
    map: &Map<String, Value>,
    idx: usize,
) -> Result<LongMemEvalCorpus, LongMemEvalError> {
    let question_id = string_field(map, &["question_id", "questionId", "qid", "id"])
        .unwrap_or_else(|| format!("q-{idx}"));
    let question = string_field(map, &["question", "query", "text"])
        .ok_or_else(|| LongMemEvalError::Parse("question text missing".to_string()))?;
    let session_labels = strings_array_field(map, &["haystack_session_ids"]).ok_or_else(|| {
        LongMemEvalError::Parse(format!(
            "question {question_id:?} has no haystack_session_ids"
        ))
    })?;
    let haystack = array_field(map, &["haystack_sessions"]).ok_or_else(|| {
        LongMemEvalError::Parse(format!("question {question_id:?} has no haystack_sessions"))
    })?;
    if session_labels.len() != haystack.len() {
        return Err(LongMemEvalError::Parse(format!(
            "question {question_id:?} has {} session ids but {} haystack sessions",
            session_labels.len(),
            haystack.len()
        )));
    }

    let haystack_dates = strings_array_field(map, &["haystack_dates"]).unwrap_or_default();
    let question_time = string_field(map, &["question_date", "date"])
        .and_then(|raw| parse_time(&raw))
        .unwrap_or_else(|| fallback_time(idx));
    let mut sessions = Vec::new();
    let mut session_id_map = std::collections::HashMap::new();
    let mut turn_gold = Vec::new();

    for (session_idx, turns_value) in haystack.iter().enumerate() {
        let label = session_labels
            .get(session_idx)
            .cloned()
            .unwrap_or_else(|| format!("{question_id}::session-{session_idx}"));
        let base_time = haystack_dates
            .get(session_idx)
            .and_then(|raw| parse_time(raw))
            .unwrap_or_else(|| offset_time(&question_time, session_idx));
        let Value::Array(turn_values) = turns_value else {
            return Err(LongMemEvalError::Parse(format!(
                "question {question_id:?} session {label:?} is not an array"
            )));
        };
        let session_id = session_id(&label);
        session_id_map.insert(label.clone(), session_id);
        let turns = parse_real_turns(
            &question_id,
            session_idx,
            &label,
            &base_time,
            turn_values,
            &mut turn_gold,
        );
        if !turns.is_empty() {
            sessions.push(IngestSession { session_id, turns });
        }
    }

    let (gold, granularity) = if !turn_gold.is_empty() {
        (turn_gold, GoldGranularity::Turn)
    } else {
        let session_gold = labels_from_fields(
            map,
            &[
                "answer_session_ids",
                "evidence_session_ids",
                "relevant_session_ids",
                "gold_session_ids",
                "session_ids",
            ],
        );
        if session_gold.is_empty() {
            return Err(LongMemEvalError::Parse(format!(
                "question {question_id:?} has no turn- or session-level gold labels"
            )));
        }
        (session_gold, GoldGranularity::Session)
    };

    Ok(LongMemEvalCorpus::new(
        sessions,
        vec![LongMemEvalQuestion {
            id: question_id,
            question,
            gold,
            granularity,
        }],
        session_id_map,
    ))
}

fn parse_real_turns(
    question_id: &str,
    session_idx: usize,
    session_label: &str,
    base_time: &Timestamp,
    turn_values: &[Value],
    turn_gold: &mut Vec<LongMemEvalGold>,
) -> Vec<IngestTurn> {
    let mut turns = Vec::new();
    for (turn_idx, turn_value) in turn_values.iter().enumerate() {
        let Value::Object(turn_map) = turn_value else {
            continue;
        };
        let Some(content) = turn_content(turn_map).map(|content| redact_scrub_patterns(&content))
        else {
            continue;
        };
        let fixture_id = string_field(
            turn_map,
            &[
                "fixture_id",
                "turn_id",
                "turnId",
                "message_id",
                "messageId",
                "id",
            ],
        )
        .unwrap_or_else(|| format!("{question_id}::{session_idx}::{session_label}::{turn_idx}"));
        if bool_field(turn_map, &["has_answer", "is_answer", "answer_evidence"]) {
            turn_gold.push(LongMemEvalGold {
                id: fixture_id.clone(),
                grade: 1,
            });
        }
        turns.push(IngestTurn {
            fixture_id,
            content,
            role: role_from_str(string_field(turn_map, &["role", "speaker"]).as_deref()),
            captured_at: string_field(turn_map, &["date", "created_at", "timestamp", "time"])
                .and_then(|raw| parse_time(&raw))
                .unwrap_or_else(|| offset_time(base_time, turn_idx)),
            importance: number_field(turn_map, &["importance"]).unwrap_or(0.5),
            trust: number_field(turn_map, &["trust"]).unwrap_or(0.5),
        });
    }
    turns
}

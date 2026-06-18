//! Flexible LongMemEval JSON/JSONL normalization.

use std::collections::{HashMap, HashSet};

use aionforge_domain::{
    ids::Id,
    nodes::episodic::Role,
    time::{Timestamp, instant_after},
};
use serde_json::{Map, Value};

use super::{
    GoldGranularity, LongMemEvalCorpus, LongMemEvalError, LongMemEvalGold, LongMemEvalQuestion,
    session_id,
};
use crate::{IngestSession, IngestTurn};

/// Parse LongMemEval JSON or JSONL into adapter-ready sessions plus retrieval labels.
///
/// The loader accepts the normalized shape this repository's tests use and several
/// common dataset aliases: `sessions`/`haystack_sessions`, `turns`/`messages`,
/// `questions`/`queries`, turn-level evidence ids, and session-level evidence ids.
/// If both turn and session labels exist for a question, turn labels win because
/// they are the finer retrieval target.
///
/// # Errors
/// Returns [`LongMemEvalError`] if the JSON cannot be parsed or lacks sessions/questions.
pub fn parse_longmemeval(input: &str) -> Result<LongMemEvalCorpus, LongMemEvalError> {
    let values = parse_values(input)?;
    let mut builder = CorpusBuilder::default();
    for value in &values {
        parse_record(value, &mut builder)?;
    }
    builder.finish()
}

#[derive(Default)]
struct CorpusBuilder {
    sessions: Vec<IngestSession>,
    session_id_map: HashMap<String, Id>,
    seen_turns: HashSet<String>,
    questions: Vec<LongMemEvalQuestion>,
}

impl CorpusBuilder {
    fn add_session(&mut self, label: String, mut session: IngestSession) {
        self.session_id_map.insert(label, session.session_id);
        session
            .turns
            .retain(|turn| self.seen_turns.insert(turn.fixture_id.clone()));
        if !session.turns.is_empty() {
            self.sessions.push(session);
        }
    }

    fn finish(self) -> Result<LongMemEvalCorpus, LongMemEvalError> {
        if self.sessions.is_empty() {
            return Err(LongMemEvalError::Parse(
                "no LongMemEval sessions found".to_string(),
            ));
        }
        if self.questions.is_empty() {
            return Err(LongMemEvalError::Parse(
                "no LongMemEval questions found".to_string(),
            ));
        }
        Ok(LongMemEvalCorpus::new(
            self.sessions,
            self.questions,
            self.session_id_map,
        ))
    }
}

struct ParsedSession {
    label: String,
    session: IngestSession,
    answer_turn_ids: Vec<String>,
}

fn parse_values(input: &str) -> Result<Vec<Value>, LongMemEvalError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(LongMemEvalError::Parse("input is empty".to_string()));
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed)
            .map(|value| vec![value])
            .map_err(|error| LongMemEvalError::Parse(error.to_string()));
    }
    trimmed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| LongMemEvalError::Parse(error.to_string()))
        })
        .collect()
}

fn parse_record(value: &Value, builder: &mut CorpusBuilder) -> Result<(), LongMemEvalError> {
    match value {
        Value::Array(items) => {
            for item in items {
                parse_record(item, builder)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            if let Some(items) = array_field(map, &["data", "examples", "records"]) {
                for item in items {
                    parse_record(item, builder)?;
                }
                return Ok(());
            }

            let parsed_sessions = parse_sessions(map)?;
            let answer_turn_ids: Vec<String> = parsed_sessions
                .iter()
                .flat_map(|session| session.answer_turn_ids.iter().cloned())
                .collect();
            for parsed in parsed_sessions {
                builder.add_session(parsed.label, parsed.session);
            }
            parse_questions(map, &answer_turn_ids, builder)
        }
        _ => Err(LongMemEvalError::Parse(
            "record must be an object or array".to_string(),
        )),
    }
}

fn parse_sessions(map: &Map<String, Value>) -> Result<Vec<ParsedSession>, LongMemEvalError> {
    if let Some(items) = array_field(
        map,
        &[
            "haystack_sessions",
            "sessions",
            "source_sessions",
            "conversation_sessions",
        ],
    ) {
        return items.iter().enumerate().map(parse_session).collect();
    }
    if has_any_array(map, &["turns", "messages", "chat", "conversation"]) {
        return parse_session((0, &Value::Object(map.clone()))).map(|session| vec![session]);
    }
    Err(LongMemEvalError::Parse(
        "record has no sessions/haystack_sessions".to_string(),
    ))
}

fn parse_session((idx, value): (usize, &Value)) -> Result<ParsedSession, LongMemEvalError> {
    let Value::Object(map) = value else {
        return Err(LongMemEvalError::Parse(
            "session must be an object".to_string(),
        ));
    };
    let label = string_field(
        map,
        &[
            "session_id",
            "sessionId",
            "conversation_id",
            "conversationId",
            "id",
        ],
    )
    .unwrap_or_else(|| format!("session-{idx}"));
    let base_time = string_field(map, &["date", "session_date", "created_at", "timestamp"])
        .and_then(|raw| parse_time(&raw))
        .unwrap_or_else(|| fallback_time(idx));
    let messages = array_field(map, &["turns", "messages", "chat", "conversation"])
        .ok_or_else(|| LongMemEvalError::Parse(format!("session {label:?} has no turns")))?;
    let mut turns = Vec::new();
    let mut answer_turn_ids = Vec::new();
    for (turn_idx, message) in messages.iter().enumerate() {
        let Value::Object(turn_map) = message else {
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
        .unwrap_or_else(|| format!("{label}::turn-{turn_idx}"));
        let Some(content) = turn_content(turn_map) else {
            continue;
        };
        if bool_field(turn_map, &["has_answer", "is_answer", "answer_evidence"]) {
            answer_turn_ids.push(fixture_id.clone());
        }
        turns.push(IngestTurn {
            fixture_id,
            content,
            role: role_from_str(string_field(turn_map, &["role", "speaker"]).as_deref()),
            captured_at: string_field(turn_map, &["date", "created_at", "timestamp", "time"])
                .and_then(|raw| parse_time(&raw))
                .unwrap_or_else(|| offset_time(&base_time, turn_idx)),
            importance: number_field(turn_map, &["importance"]).unwrap_or(0.5),
            trust: number_field(turn_map, &["trust"]).unwrap_or(0.5),
        });
    }
    Ok(ParsedSession {
        session: IngestSession {
            session_id: session_id(&label),
            turns,
        },
        label,
        answer_turn_ids,
    })
}

fn parse_questions(
    map: &Map<String, Value>,
    answer_turn_ids: &[String],
    builder: &mut CorpusBuilder,
) -> Result<(), LongMemEvalError> {
    if let Some(items) = array_field(map, &["questions", "queries", "qa", "qas"]) {
        for item in items {
            builder.questions.push(parse_question(
                item,
                answer_turn_ids,
                builder.questions.len(),
            )?);
        }
        return Ok(());
    }
    if map.contains_key("question") || map.contains_key("query") {
        builder.questions.push(parse_question(
            &Value::Object(map.clone()),
            answer_turn_ids,
            builder.questions.len(),
        )?);
    }
    Ok(())
}

fn parse_question(
    value: &Value,
    answer_turn_ids: &[String],
    idx: usize,
) -> Result<LongMemEvalQuestion, LongMemEvalError> {
    let Value::Object(map) = value else {
        return Err(LongMemEvalError::Parse(
            "question must be an object".to_string(),
        ));
    };
    let question = string_field(map, &["question", "query", "text"])
        .ok_or_else(|| LongMemEvalError::Parse("question text missing".to_string()))?;
    let id = string_field(map, &["question_id", "questionId", "qid", "id"])
        .unwrap_or_else(|| format!("q-{idx}"));

    let mut gold = labels_from_fields(
        map,
        &[
            "evidence_turn_ids",
            "answer_turn_ids",
            "relevant_turn_ids",
            "gold_turn_ids",
            "evidence_ids",
            "answer_evidence_ids",
            "source_ids",
        ],
    );
    let mut granularity = GoldGranularity::Turn;
    if gold.is_empty() {
        gold = answer_turn_ids
            .iter()
            .map(|id| LongMemEvalGold {
                id: id.clone(),
                grade: 1,
            })
            .collect();
    }
    if gold.is_empty() {
        gold = labels_from_fields(
            map,
            &[
                "answer_session_ids",
                "evidence_session_ids",
                "relevant_session_ids",
                "gold_session_ids",
                "session_ids",
            ],
        );
        granularity = GoldGranularity::Session;
    }
    if gold.is_empty() {
        return Err(LongMemEvalError::Parse(format!(
            "question {id:?} has no turn- or session-level gold labels"
        )));
    }
    Ok(LongMemEvalQuestion {
        id,
        question,
        gold,
        granularity,
    })
}

fn labels_from_fields(map: &Map<String, Value>, names: &[&str]) -> Vec<LongMemEvalGold> {
    for name in names {
        if let Some(value) = map.get(*name) {
            let labels = labels_from_value(value);
            if !labels.is_empty() {
                return labels;
            }
        }
    }
    if let Some(Value::Array(items)) = map.get("evidence") {
        let labels: Vec<_> = items.iter().flat_map(labels_from_value).collect();
        if !labels.is_empty() {
            return labels;
        }
    }
    Vec::new()
}

fn labels_from_value(value: &Value) -> Vec<LongMemEvalGold> {
    match value {
        Value::Array(items) => items.iter().flat_map(labels_from_value).collect(),
        Value::Object(map) => {
            if let Some(id) = string_field(
                map,
                &[
                    "fixture_id",
                    "turn_id",
                    "message_id",
                    "session_id",
                    "id",
                    "source_id",
                ],
            ) {
                vec![LongMemEvalGold {
                    id,
                    grade: number_field(map, &["grade", "score", "relevance"]).unwrap_or(1.0) as u8,
                }]
            } else {
                map.iter()
                    .filter_map(|(id, grade)| {
                        grade.as_u64().map(|grade| LongMemEvalGold {
                            id: id.clone(),
                            grade: grade as u8,
                        })
                    })
                    .collect()
            }
        }
        Value::String(id) => vec![LongMemEvalGold {
            id: id.clone(),
            grade: 1,
        }],
        Value::Number(number) => vec![LongMemEvalGold {
            id: number.to_string(),
            grade: 1,
        }],
        _ => Vec::new(),
    }
}

fn array_field<'a>(map: &'a Map<String, Value>, names: &[&str]) -> Option<&'a [Value]> {
    names
        .iter()
        .find_map(|name| map.get(*name)?.as_array().map(Vec::as_slice))
}

fn has_any_array(map: &Map<String, Value>, names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| map.get(*name).is_some_and(Value::is_array))
}

fn string_field(map: &Map<String, Value>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| match map.get(*name)? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn number_field(map: &Map<String, Value>, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| map.get(*name)?.as_f64())
}

fn bool_field(map: &Map<String, Value>, names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| map.get(*name).and_then(Value::as_bool).unwrap_or(false))
}

fn turn_content(map: &Map<String, Value>) -> Option<String> {
    string_field(map, &["content", "text", "message", "value"]).or_else(|| {
        let user = string_field(map, &["user"]);
        let assistant = string_field(map, &["assistant"]);
        match (user, assistant) {
            (Some(user), Some(assistant)) => Some(format!("User: {user}\nAssistant: {assistant}")),
            (Some(user), None) => Some(format!("User: {user}")),
            (None, Some(assistant)) => Some(format!("Assistant: {assistant}")),
            (None, None) => None,
        }
    })
}

fn role_from_str(role: Option<&str>) -> Role {
    match role
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        "event" => Role::Event,
        _ => Role::User,
    }
}

fn parse_time(raw: &str) -> Option<Timestamp> {
    let raw = raw.trim();
    raw.parse()
        .ok()
        .or_else(|| {
            (raw.len() == 10)
                .then(|| format!("{raw}T00:00:00Z[UTC]").parse().ok())
                .flatten()
        })
        .or_else(|| {
            raw.strip_suffix('Z')
                .filter(|value| !value.contains('['))
                .and_then(|value| format!("{value}+00:00[UTC]").parse().ok())
        })
}

fn fallback_time(index: usize) -> Timestamp {
    let seconds = index % 60;
    let minutes = (index / 60) % 60;
    let hours = (index / 3_600) % 24;
    let day = 1 + ((index / 86_400) % 28);
    parse_static_time(&format!(
        "2026-01-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z[UTC]"
    ))
}

fn offset_time(base: &Timestamp, index: usize) -> Timestamp {
    instant_after(base, index as u64)
}

fn parse_static_time(input: &str) -> Timestamp {
    input.parse().expect("static eval timestamp is valid")
}

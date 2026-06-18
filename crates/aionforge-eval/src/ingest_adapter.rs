//! Session-aware ingest adapter for evaluation corpora.
//!
//! Evaluation runners use this module to turn benchmark conversations into the
//! same `Episode` shape the product stores, without each runner rebuilding its
//! own seed loop. The adapter preserves the benchmark fixture id for scoring,
//! threads session ids and event times into every episode, and can optionally
//! drain the in-process consolidation pipeline so runner arms can compare raw
//! turn recall against derived-memory recall.

use std::{collections::HashMap, error::Error, fmt, sync::Arc};

use aionforge_consolidate::{
    ConsolidationConfig, ConsolidationError, Consolidator, FactExtractionPass, PassConfig,
    RuleExtractor, RuleSummarizer,
};
use aionforge_domain::{
    blocks::{Identity, Stats},
    contracts::Embedder,
    embedding::{EmbedderModel, Embedding},
    ids::{ContentHash, Id},
    namespace::Namespace,
    nodes::episodic::{ConsolidationState, Episode, Role},
    time::Timestamp,
};
use aionforge_store::{Store, StoreConfig, StoreError};

use crate::{BeamConversation, MemoryRow, scrub_violations};

const DEFAULT_TIME: &str = "2026-01-01T00:00:00Z[UTC]";

/// A benchmark corpus normalized into conversation sessions.
pub type IngestCorpus = Vec<IngestSession>;

/// One benchmark turn that should become an eval-seeded episode.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestTurn {
    /// The benchmark's original message or memory id, preserved for gold-label mapping.
    pub fixture_id: String,
    /// Text stored as the raw episode body.
    pub content: String,
    /// Producer role for the episode.
    pub role: Role,
    /// Event time to persist as `Episode.captured_at`.
    pub captured_at: Timestamp,
    /// Effective importance in `[0, 1]`.
    pub importance: f64,
    /// Writer trust in `[0, 1]`.
    pub trust: f64,
}

/// One conversation session. Every turn becomes an episode with this `session_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestSession {
    /// Durable session id written into `Episode.session_id`.
    pub session_id: Id,
    /// Ordered turns in this session.
    pub turns: Vec<IngestTurn>,
}

/// Which memory surfaces to prepare for a runner arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestMode {
    /// Seed raw turns only and leave consolidation untouched.
    RawTurns,
    /// Seed turns and drain deterministic consolidation so derived facts/notes exist.
    Consolidated,
    /// Seed turns and drain consolidation while retaining the source episodes for raw recall.
    ///
    /// The current store always retains source episodes for lineage, so this mode has
    /// the same storage effect as [`Self::Consolidated`]. It is still an explicit arm
    /// because runners need to name the measurement posture they intend.
    Both,
}

impl IngestMode {
    fn runs_consolidation(self) -> bool {
        matches!(self, Self::Consolidated | Self::Both)
    }
}

/// Result of seeding an eval corpus.
#[derive(Debug, Clone)]
pub struct IngestOutcome {
    /// Store containing the seeded episodes and any derived consolidation output.
    pub store: Arc<Store>,
    /// Benchmark fixture id to persisted episode id.
    pub id_map: HashMap<String, Id>,
}

/// Errors produced while turning an eval corpus into a seeded store.
#[derive(Debug)]
pub enum IngestError {
    /// The corpus failed the fixture scrub gate.
    Scrub(Vec<String>),
    /// Two turns carried the same fixture id.
    DuplicateFixtureId(String),
    /// A precomputed embedding was missing for the turn content.
    MissingEmbedding(String),
    /// The embedder returned a vector count that did not match its input count.
    EmbeddingCount {
        /// Number of texts sent to the embedder.
        expected: usize,
        /// Number of vectors returned by the embedder.
        actual: usize,
    },
    /// Consolidation was requested without an embedder for derived fact statements.
    MissingConsolidationEmbedder,
    /// Store operation failed.
    Store(StoreError),
    /// Consolidation operation failed.
    Consolidation(ConsolidationError),
    /// Embedder operation failed.
    Embedder(String),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scrub(violations) => write!(f, "fixture scrub violations: {violations:?}"),
            Self::DuplicateFixtureId(id) => write!(f, "duplicate fixture id {id:?}"),
            Self::MissingEmbedding(content) => {
                write!(f, "missing precomputed embedding for {content:?}")
            }
            Self::EmbeddingCount { expected, actual } => {
                write!(
                    f,
                    "embedder returned {actual} vectors for {expected} inputs"
                )
            }
            Self::MissingConsolidationEmbedder => {
                f.write_str("consolidation mode requires a consolidation embedder")
            }
            Self::Store(error) => write!(f, "store error: {error}"),
            Self::Consolidation(error) => write!(f, "consolidation error: {error}"),
            Self::Embedder(error) => write!(f, "embedder error: {error}"),
        }
    }
}

impl Error for IngestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Consolidation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StoreError> for IngestError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ConsolidationError> for IngestError {
    fn from(error: ConsolidationError) -> Self {
        Self::Consolidation(error)
    }
}

/// Seed sessions by embedding every turn with the supplied embedder.
///
/// # Errors
/// Returns [`IngestError`] if scrubbing, embedding, storage, or consolidation fails.
pub async fn seed_sessions<E>(
    sessions: &[IngestSession],
    mode: IngestMode,
    embedder: Arc<E>,
    dim: u32,
) -> Result<IngestOutcome, IngestError>
where
    E: Embedder + 'static,
{
    let texts: Vec<String> = sessions
        .iter()
        .flat_map(|session| session.turns.iter().map(|turn| turn.content.clone()))
        .collect();
    let vectors = embedder
        .embed(&texts)
        .await
        .map_err(|error| IngestError::Embedder(error.to_string()))?;
    if vectors.len() != texts.len() {
        return Err(IngestError::EmbeddingCount {
            expected: texts.len(),
            actual: vectors.len(),
        });
    }
    let embeddings: HashMap<String, Embedding> = texts.into_iter().zip(vectors).collect();
    seed_sessions_with_embeddings(sessions, mode, &embeddings, Some(embedder), dim).await
}

/// Seed sessions from an embed-once cache keyed by turn content.
///
/// When `mode` runs consolidation, `consolidation_embedder` is required because the
/// fact-extraction pass embeds derived fact statements that are not necessarily present
/// in the turn-content cache.
///
/// # Errors
/// Returns [`IngestError`] if scrubbing, embedding lookup, storage, or consolidation fails.
pub async fn seed_sessions_with_embeddings<E>(
    sessions: &[IngestSession],
    mode: IngestMode,
    embeddings: &HashMap<String, Embedding>,
    consolidation_embedder: Option<Arc<E>>,
    dim: u32,
) -> Result<IngestOutcome, IngestError>
where
    E: Embedder + 'static,
{
    scrub_sessions(sessions)?;
    if mode.runs_consolidation() && consolidation_embedder.is_none() {
        return Err(IngestError::MissingConsolidationEmbedder);
    }

    let store = Arc::new(Store::open_with_config(StoreConfig {
        embedding_dimension: dim,
    })?);
    store.migrate(&migration_time(sessions))?;

    let embedder_model = consolidation_embedder
        .as_ref()
        .map(|embedder| embedder.model().clone());
    let mut id_map = HashMap::new();
    for session in sessions {
        for turn in &session.turns {
            let id = Id::generate();
            if id_map.insert(turn.fixture_id.clone(), id).is_some() {
                return Err(IngestError::DuplicateFixtureId(turn.fixture_id.clone()));
            }
            let embedding = embeddings
                .get(&turn.content)
                .cloned()
                .ok_or_else(|| IngestError::MissingEmbedding(turn.content.clone()))?;
            let episode =
                episode_for_turn(id, session.session_id, turn, embedding, &embedder_model);
            store.insert_episode(&episode)?;
        }
    }

    if let Some(embedder) = consolidation_embedder.filter(|_| mode.runs_consolidation()) {
        drain_consolidation(&store, embedder).await?;
    }

    Ok(IngestOutcome { store, id_map })
}

impl From<&BeamConversation> for IngestSession {
    fn from(conversation: &BeamConversation) -> Self {
        let session_id = Id::from_content_hash(
            format!("beam-session|{}", conversation.conversation_id).as_bytes(),
        );
        let turns = conversation
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| IngestTurn {
                fixture_id: message.id.clone(),
                content: message.text.clone(),
                role: role_from_beam(&message.role),
                captured_at: message
                    .time_anchor
                    .as_deref()
                    .and_then(parse_beam_time_anchor)
                    .unwrap_or_else(|| fallback_timestamp(index)),
                importance: 0.5,
                trust: 0.5,
            })
            .collect();
        Self { session_id, turns }
    }
}

impl From<&[MemoryRow]> for IngestSession {
    fn from(rows: &[MemoryRow]) -> Self {
        let session_id = Id::from_content_hash(b"eval-fixture-memory-rows");
        let turns = rows
            .iter()
            .enumerate()
            .map(|(index, row)| IngestTurn {
                fixture_id: row.id.clone(),
                content: row.text.clone(),
                role: Role::User,
                captured_at: fallback_timestamp(index),
                importance: row.importance,
                trust: row.trust,
            })
            .collect();
        Self { session_id, turns }
    }
}

fn episode_for_turn(
    id: Id,
    session_id: Id,
    turn: &IngestTurn,
    embedding: Embedding,
    embedder_model: &Option<EmbedderModel>,
) -> Episode {
    Episode {
        identity: Identity {
            id,
            ingested_at: turn.captured_at.clone(),
            namespace: Namespace::Global,
            expired_at: None,
        },
        stats: Stats {
            importance: turn.importance,
            trust: turn.trust,
            last_access: turn.captured_at.clone(),
            access_count_recent: 0,
            referenced_count: 0,
            surprise: 0.1,
            is_pinned: false,
        },
        content: turn.content.clone(),
        role: turn.role,
        captured_at: turn.captured_at.clone(),
        agent_id: Id::from_content_hash(b"aionforge-eval-ingest-adapter"),
        session_id: Some(session_id),
        content_hash: ContentHash::of(turn.content.as_bytes()),
        embedding: Some(embedding),
        embedder_model: embedder_model.clone(),
        consolidation_state: ConsolidationState::Raw,
        origin: None,
    }
}

async fn drain_consolidation<E>(store: &Arc<Store>, embedder: Arc<E>) -> Result<(), IngestError>
where
    E: Embedder + 'static,
{
    let mut consolidator = Consolidator::new(Arc::clone(store), ConsolidationConfig::default());
    consolidator.register(Box::new(FactExtractionPass::new(
        Arc::new(RuleExtractor::with_default_rules()),
        embedder,
        Arc::new(RuleSummarizer::with_default_rules()),
        PassConfig::default(),
    )));
    loop {
        let report = consolidator.tick_once().await?;
        if report.pending_after == 0 {
            break;
        }
    }
    Ok(())
}

fn scrub_sessions(sessions: &[IngestSession]) -> Result<(), IngestError> {
    let items = sessions
        .iter()
        .flat_map(|session| session.turns.iter())
        .map(|turn| (turn.fixture_id.as_str(), turn.content.as_str()));
    let violations = scrub_violations(items);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(IngestError::Scrub(violations))
    }
}

fn migration_time(sessions: &[IngestSession]) -> Timestamp {
    sessions
        .iter()
        .flat_map(|session| session.turns.iter())
        .map(|turn| turn.captured_at.clone())
        .min_by(|left, right| left.to_string().cmp(&right.to_string()))
        .unwrap_or_else(|| parse_timestamp(DEFAULT_TIME))
}

fn role_from_beam(role: &str) -> Role {
    match role.trim().to_ascii_lowercase().as_str() {
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        "system" => Role::System,
        "event" => Role::Event,
        _ => Role::User,
    }
}

fn parse_beam_time_anchor(anchor: &str) -> Option<Timestamp> {
    let mut parts = anchor.split('-');
    let month = month_number(parts.next()?.trim())?;
    let day = parts.next()?.trim().parse::<u8>().ok()?;
    let year = parts.next()?.trim().parse::<u16>().ok()?;
    if parts.next().is_some() || day == 0 || day > 31 {
        return None;
    }
    Some(parse_timestamp(&format!(
        "{year:04}-{month:02}-{day:02}T00:00:00Z[UTC]"
    )))
}

fn month_number(month: &str) -> Option<u8> {
    match month.to_ascii_lowercase().as_str() {
        "january" => Some(1),
        "february" => Some(2),
        "march" => Some(3),
        "april" => Some(4),
        "may" => Some(5),
        "june" => Some(6),
        "july" => Some(7),
        "august" => Some(8),
        "september" => Some(9),
        "october" => Some(10),
        "november" => Some(11),
        "december" => Some(12),
        _ => None,
    }
}

fn fallback_timestamp(index: usize) -> Timestamp {
    let seconds = index % 60;
    let minutes = (index / 60) % 60;
    let hours = (index / 3_600) % 24;
    let day = 1 + ((index / 86_400) % 28);
    parse_timestamp(&format!(
        "2026-01-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z[UTC]"
    ))
}

fn parse_timestamp(input: &str) -> Timestamp {
    input.parse().expect("static eval timestamp is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    use aionforge_store::{BoundQuery, QueryResult};

    const DIM: usize = 16;

    #[derive(Clone)]
    struct FakeEmbedder {
        model: EmbedderModel,
        overrides: HashMap<String, Vec<f32>>,
    }

    impl FakeEmbedder {
        fn new(overrides: HashMap<String, Vec<f32>>) -> Self {
            Self {
                model: EmbedderModel {
                    family: "fake-ingest-adapter".to_string(),
                    version: "1".to_string(),
                    dimension: DIM as u32,
                },
                overrides,
            }
        }

        fn vector(&self, text: &str) -> Vec<f32> {
            self.overrides
                .get(text)
                .cloned()
                .unwrap_or_else(|| hash_vec(text))
        }
    }

    #[derive(Debug)]
    struct FakeEmbedError;

    impl fmt::Display for FakeEmbedError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("fake embedder never fails")
        }
    }

    impl Error for FakeEmbedError {}

    impl Embedder for FakeEmbedder {
        type Error = FakeEmbedError;

        fn embed(
            &self,
            inputs: &[String],
        ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
            let out = inputs
                .iter()
                .map(|text| Embedding::new(self.vector(text)).expect("valid fake embedding"))
                .collect();
            async move { Ok(out) }
        }

        fn model(&self) -> &EmbedderModel {
            &self.model
        }
    }

    fn hash_vec(text: &str) -> Vec<f32> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let mut x = h | 1;
        let mut v = Vec::with_capacity(DIM);
        for _ in 0..DIM {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let unit = (x >> 11) as f64 / (1u64 << 53) as f64;
            v.push((unit * 2.0 - 1.0) as f32);
        }
        let norm = v
            .iter()
            .map(|c| c * c)
            .sum::<f32>()
            .sqrt()
            .max(f32::EPSILON);
        v.iter().map(|c| c / norm).collect()
    }

    fn ts(input: &str) -> Timestamp {
        input.parse().expect("valid timestamp")
    }

    fn session(id: &'static [u8], turns: Vec<IngestTurn>) -> IngestSession {
        IngestSession {
            session_id: Id::from_content_hash(id),
            turns,
        }
    }

    fn turn(fixture_id: &str, content: &str, captured_at: &str) -> IngestTurn {
        IngestTurn {
            fixture_id: fixture_id.to_string(),
            content: content.to_string(),
            role: Role::User,
            captured_at: ts(captured_at),
            importance: 0.5,
            trust: 0.8,
        }
    }

    fn count(store: &Store, query: &str) -> usize {
        let QueryResult::Rows(rows) = store.execute(&BoundQuery::new(query)).expect("count query")
        else {
            return 0;
        };
        rows.row_count()
    }

    #[tokio::test]
    async fn raw_turns_preserve_session_time_and_fixture_ids() {
        let session_a = session(
            b"eval-session-a",
            vec![
                turn(
                    "a-1",
                    "Alice remembers the launch plan",
                    "2026-02-01T09:00:00Z[UTC]",
                ),
                turn(
                    "a-2",
                    "Bob tracks the release checklist",
                    "2026-02-01T09:05:00Z[UTC]",
                ),
            ],
        );
        let session_b = session(
            b"eval-session-b",
            vec![turn(
                "b-1",
                "Carol records the customer escalation",
                "2026-02-02T10:00:00Z[UTC]",
            )],
        );

        let outcome = seed_sessions(
            &[session_a.clone(), session_b.clone()],
            IngestMode::RawTurns,
            Arc::new(FakeEmbedder::new(HashMap::new())),
            DIM as u32,
        )
        .await
        .expect("seed sessions");

        let first_id = outcome.id_map["a-1"];
        let first = outcome
            .store
            .episode_by_id(&first_id)
            .expect("read episode")
            .expect("episode exists");
        assert_eq!(first.session_id, Some(session_a.session_id));
        assert_eq!(first.captured_at, session_a.turns[0].captured_at);
        assert_eq!(first.content, session_a.turns[0].content);
        assert_eq!(outcome.id_map.len(), 3);

        let session_b_episodes = outcome
            .store
            .live_episodes_by_session_id(&session_b.session_id, 10)
            .expect("read session episodes");
        assert_eq!(session_b_episodes.len(), 1);
        assert_eq!(
            session_b_episodes[0].captured_at,
            session_b.turns[0].captured_at
        );
    }

    #[tokio::test]
    async fn consolidated_mode_derives_facts_from_svo_turns() {
        let sessions = vec![session(
            b"eval-svo-session",
            vec![
                turn(
                    "svo-1",
                    "Alice works on Aionforge",
                    "2026-03-01T09:00:00Z[UTC]",
                ),
                turn("svo-2", "Bob uses Postgres", "2026-03-01T09:05:00Z[UTC]"),
            ],
        )];

        let outcome = seed_sessions(
            &sessions,
            IngestMode::Consolidated,
            Arc::new(FakeEmbedder::new(HashMap::new())),
            DIM as u32,
        )
        .await
        .expect("seed and consolidate sessions");

        assert!(
            count(&outcome.store, "MATCH (f:Fact) RETURN f.id AS id") > 0,
            "SVO turns should derive facts"
        );
    }

    #[tokio::test]
    async fn raw_and_consolidated_modes_produce_observably_different_contents() {
        let sessions = vec![session(
            b"eval-ab-session",
            vec![turn(
                "ab-1",
                "Delta prefers Determinism",
                "2026-04-01T09:00:00Z[UTC]",
            )],
        )];
        let embedder = Arc::new(FakeEmbedder::new(HashMap::new()));

        let raw = seed_sessions(
            &sessions,
            IngestMode::RawTurns,
            Arc::clone(&embedder),
            DIM as u32,
        )
        .await
        .expect("seed raw");
        let consolidated = seed_sessions(&sessions, IngestMode::Consolidated, embedder, DIM as u32)
            .await
            .expect("seed consolidated");

        assert_eq!(count(&raw.store, "MATCH (f:Fact) RETURN f.id AS id"), 0);
        assert!(
            count(&consolidated.store, "MATCH (f:Fact) RETURN f.id AS id") > 0,
            "consolidated mode should add derived facts"
        );
    }

    #[test]
    fn beam_conversion_maps_time_anchor_and_role() {
        let conversation = BeamConversation {
            conversation_id: "c-1".to_string(),
            title: "Demo".to_string(),
            messages: vec![crate::BeamMessage {
                id: "msg-1".to_string(),
                text: "hello".to_string(),
                role: "assistant".to_string(),
                time_anchor: Some("March-15-2024".to_string()),
            }],
            probes: Vec::new(),
        };

        let session = IngestSession::from(&conversation);
        assert_eq!(session.turns[0].role, Role::Assistant);
        assert_eq!(
            session.turns[0].captured_at,
            ts("2024-03-15T00:00:00Z[UTC]")
        );
    }
}

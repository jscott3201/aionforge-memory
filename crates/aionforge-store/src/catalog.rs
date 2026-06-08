//! The Aionforge v1.0.0 schema catalog: the forward-only DDL the migration runner
//! applies to a closed graph.
//!
//! Every statement is a fixed, compiled-in `CREATE ... TYPE IF NOT EXISTS` — there is
//! no caller input to bind, so the parameter-binding rule does not bite here. Node
//! types come before edge types because an edge endpoint clause resolves its node
//! labels to positional indices, so the node types must already exist.
//!
//! This is the type-shape layer (`NOT NULL` / `DEFAULT` / `IMMUTABLE` / `UNIQUE`,
//! data-model §3–§5). The `INDEXED` markers in the spec tables and the vector / text /
//! composite indexes and candidate-state providers (§7–§9) are registered separately.
//!
//! Nullability follows the domain: a property is `NOT NULL` exactly when its domain
//! field is non-`Option` (so the closed graph rejects a write that omits it, the
//! fail-fast guarantee in §1.1), and nullable when the domain field is `Option<T>` or a
//! collection whose empty value means "absent" (the nullable `LIST` convention). This
//! goes slightly past the spec's explicit `NOT NULL` markers — §3/§4 leave some
//! always-present fields (the stats block, `trust_scores`, `posterior`, …) unmarked,
//! but the domain models them as mandatory, so the schema enforces that.
//!
//! Two deliberate departures from spec §4–§5, both forced by the engine surface:
//! - `CoreBlock` carries `embedder_model` (the domain `CoreBlock` has it and §7 indexes
//!   its embedding); spec §4.7 omits it, which reads as a spec gap.
//! - `DEPENDS_ON` declares `OneOf({Skill, Fact})` on both endpoints rather than the two
//!   disjoint pairs `Skill→Skill` and `Fact→Fact`, because this engine keys an edge
//!   type by its label and rejects a second `DEPENDS_ON` declaration as a duplicate.

/// The schema version this catalog defines.
///
/// The migration runner bumps the `SchemaVersion` singleton to this value once every
/// type is declared. A future embedder change or added kind is a new version with its
/// own forward-only step; this catalog is version 1, the full v1.0.0 surface.
pub const SCHEMA_VERSION: i64 = 1;

/// One catalog entry: a type's identifying label and the statement that declares it.
pub(crate) struct TypeDdl {
    /// The node label / edge relationship name, used to detect a prior creation.
    pub name: &'static str,
    /// The `CREATE ... TYPE IF NOT EXISTS` statement.
    pub ddl: &'static str,
}

/// The 17 node types (data-model §4), in declaration order.
pub(crate) const NODE_TYPES: &[TypeDdl] = &[
    TypeDdl {
        name: "Episode",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Episode (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            content :: STRING NOT NULL,
            role :: STRING NOT NULL,
            captured_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            agent_id :: UUID NOT NULL,
            session_id :: UUID,
            content_hash :: STRING NOT NULL,
            embedding_v1 :: VECTOR,
            embedder_model :: STRING,
            consolidation_state :: STRING NOT NULL DEFAULT 'raw',
            origin :: JSON
        ) STRICT"#,
    },
    TypeDdl {
        name: "Fact",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Fact (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            subject_id :: UUID NOT NULL,
            predicate :: STRING NOT NULL,
            object_kind :: STRING NOT NULL,
            object_entity_id :: UUID,
            object_value :: JSON,
            confidence :: FLOAT NOT NULL,
            status :: STRING NOT NULL DEFAULT 'active',
            statement :: STRING NOT NULL,
            embedding_v1 :: VECTOR,
            embedder_model :: STRING,
            extraction :: JSON
        ) STRICT"#,
    },
    TypeDdl {
        name: "Entity",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Entity (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            canonical_name :: STRING NOT NULL,
            type :: STRING NOT NULL,
            aliases :: LIST<STRING>,
            description :: STRING,
            embedding_v1 :: VECTOR,
            embedder_model :: STRING,
            attributes :: JSON
        ) STRICT"#,
    },
    TypeDdl {
        name: "Skill",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Skill (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            name :: STRING NOT NULL,
            version :: INT NOT NULL,
            description :: STRING NOT NULL,
            problem_embedding_v1 :: VECTOR,
            embedder_model :: STRING,
            language :: STRING NOT NULL,
            body :: STRING NOT NULL,
            params :: JSON NOT NULL,
            preconditions :: JSON,
            postconditions :: JSON,
            capabilities :: LIST<STRING>,
            success_count :: UINT NOT NULL DEFAULT 0,
            failure_count :: UINT NOT NULL DEFAULT 0,
            mean_latency_ms :: FLOAT,
            source_hash :: STRING NOT NULL,
            last_success_at :: ZONED DATETIME,
            last_failure_at :: ZONED DATETIME,
            deprecated_at :: ZONED DATETIME,
            induced :: BOOLEAN NOT NULL DEFAULT FALSE
        ) STRICT"#,
    },
    TypeDdl {
        name: "BadPattern",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :BadPattern (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            description :: STRING NOT NULL,
            embedding_v1 :: VECTOR,
            embedder_model :: STRING,
            observed_at :: ZONED DATETIME NOT NULL IMMUTABLE
        ) STRICT"#,
    },
    TypeDdl {
        name: "Note",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Note (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            content :: STRING NOT NULL,
            context :: STRING,
            keywords :: LIST<STRING>,
            embedding_v1 :: VECTOR,
            embedder_model :: STRING,
            derived_from_episode :: UUID
        ) STRICT"#,
    },
    TypeDdl {
        name: "CoreBlock",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :CoreBlock (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            importance :: FLOAT NOT NULL,
            trust :: FLOAT NOT NULL,
            last_access :: ZONED DATETIME NOT NULL,
            access_count_recent :: UINT NOT NULL,
            referenced_count :: UINT NOT NULL,
            surprise :: FLOAT NOT NULL,
            is_pinned :: BOOLEAN NOT NULL DEFAULT FALSE,
            content :: STRING NOT NULL,
            block_kind :: STRING NOT NULL,
            sensitivity :: STRING,
            drift_baseline :: JSON,
            embedding_v1 :: VECTOR,
            embedder_model :: STRING
        ) STRICT"#,
    },
    TypeDdl {
        name: "Agent",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Agent (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            public_key :: STRING NOT NULL IMMUTABLE,
            model_family :: STRING NOT NULL,
            model_version :: STRING,
            trust_scores :: JSON NOT NULL,
            status :: STRING NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "Session",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Session (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            started_at :: ZONED DATETIME NOT NULL,
            ended_at :: ZONED DATETIME,
            owner_agent_id :: UUID NOT NULL,
            metadata :: JSON NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "ProvenanceRecord",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :ProvenanceRecord (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            subject_id :: UUID NOT NULL,
            writer_agent_id :: UUID NOT NULL,
            signature :: STRING NOT NULL IMMUTABLE,
            source_episode_ids :: LIST<UUID>,
            model_family :: STRING NOT NULL,
            model_version :: STRING,
            trust_at_write :: FLOAT NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "AuditEvent",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :AuditEvent (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            kind :: STRING NOT NULL,
            subject_id :: UUID NOT NULL,
            actor_id :: UUID NOT NULL,
            payload :: JSON NOT NULL,
            signature :: STRING NOT NULL IMMUTABLE,
            occurred_at :: ZONED DATETIME NOT NULL IMMUTABLE
        ) STRICT"#,
    },
    TypeDdl {
        name: "Promotion",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Promotion (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            candidate_fact_id :: UUID NOT NULL,
            posterior :: FLOAT NOT NULL,
            k :: UINT NOT NULL,
            status :: STRING NOT NULL,
            resolved_at :: ZONED DATETIME,
            promoted_fact_id :: UUID
        ) STRICT"#,
    },
    TypeDdl {
        name: "ConsolidationCursor",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :ConsolidationCursor (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            last_position :: STRING NOT NULL,
            last_episode_id :: UUID,
            last_processed_at :: ZONED DATETIME,
            rule_versions :: JSON NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "SchemaVersion",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :SchemaVersion (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            current_version :: INT NOT NULL,
            applied_at :: ZONED DATETIME NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "Scope",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :Scope (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            name :: STRING NOT NULL,
            scope_kind :: STRING NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "RecencyWindow",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :RecencyWindow (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            label :: STRING NOT NULL,
            starts_at :: ZONED DATETIME,
            ends_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "ValidityAnchor",
        ddl: r#"CREATE NODE TYPE IF NOT EXISTS :ValidityAnchor (
            id :: UUID NOT NULL UNIQUE IMMUTABLE,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            namespace :: STRING NOT NULL,
            expired_at :: ZONED DATETIME,
            instant :: ZONED DATETIME NOT NULL,
            label :: STRING
        ) STRICT"#,
    },
];

/// The 19 edge types (data-model §5), in declaration order.
///
/// The four-timestamp bi-temporal block is `valid_from` (NOT NULL), `valid_to`
/// (nullable), `ingested_at` (NOT NULL IMMUTABLE), `expired_at` (nullable). The engine
/// has no native bi-temporal concept, so the block is four ordinary properties and the
/// schema-mirror test is what guards every bi-temporal edge against a forgotten one.
pub(crate) const EDGE_TYPES: &[TypeDdl] = &[
    TypeDdl {
        name: "MENTIONS",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :MENTIONS (
            FROM :Episode TO :Entity,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "ABOUT",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :ABOUT (
            FROM :Fact TO :Entity,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "SUPPORTS",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :SUPPORTS (
            FROM :Fact, :Episode TO :Fact,
            weight :: FLOAT NOT NULL
        ) STRICT"#,
    },
    TypeDdl {
        name: "SUPERSEDED_BY",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :SUPERSEDED_BY (
            FROM :Fact TO :Fact,
            reason :: STRING NOT NULL,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "CONTRADICTS",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :CONTRADICTS (
            FROM :Fact TO :Fact,
            detected_by :: STRING NOT NULL,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "VALID_AT",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :VALID_AT (
            FROM :Fact TO :ValidityAnchor,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "IN_SCOPE",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :IN_SCOPE (
            FROM :Episode, :Fact, :Entity, :Skill, :BadPattern, :Note, :CoreBlock TO :Scope
        ) STRICT"#,
    },
    TypeDdl {
        name: "IN_SESSION",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :IN_SESSION (
            FROM :Episode, :Fact TO :Session
        ) STRICT"#,
    },
    TypeDdl {
        name: "RECENT_IN",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :RECENT_IN (
            FROM :Episode, :Fact, :Entity, :Skill, :BadPattern, :Note, :CoreBlock TO :RecencyWindow
        ) STRICT"#,
    },
    TypeDdl {
        name: "DEPENDS_ON",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :DEPENDS_ON (
            FROM :Skill, :Fact TO :Skill, :Fact
        ) STRICT"#,
    },
    TypeDdl {
        name: "DERIVED_FROM",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :DERIVED_FROM (
            derived_at :: ZONED DATETIME NOT NULL IMMUTABLE
        ) STRICT"#,
    },
    TypeDdl {
        name: "WRITTEN_BY",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :WRITTEN_BY (
            FROM :Episode, :Fact, :Entity, :Skill, :BadPattern, :Note, :CoreBlock TO :Agent
        ) STRICT"#,
    },
    TypeDdl {
        name: "ATTESTED_BY",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :ATTESTED_BY (
            FROM :Fact, :CoreBlock TO :Agent,
            attested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            signature :: STRING NOT NULL IMMUTABLE,
            category :: STRING
        ) STRICT"#,
    },
    TypeDdl {
        name: "PROMOTED_TO",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :PROMOTED_TO (
            FROM :Fact TO :Fact,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "DEMOTED_FROM",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :DEMOTED_FROM (
            FROM :Fact TO :Fact,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "HAS_FAILURE",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :HAS_FAILURE (
            FROM :Skill TO :BadPattern,
            observed_at :: ZONED DATETIME NOT NULL IMMUTABLE
        ) STRICT"#,
    },
    TypeDdl {
        name: "RELATES_TO",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :RELATES_TO (
            FROM :Note TO :Note,
            relationship_label :: STRING NOT NULL,
            valid_from :: ZONED DATETIME NOT NULL,
            valid_to :: ZONED DATETIME,
            ingested_at :: ZONED DATETIME NOT NULL IMMUTABLE,
            expired_at :: ZONED DATETIME
        ) STRICT"#,
    },
    TypeDdl {
        name: "HAS_PROVENANCE",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :HAS_PROVENANCE (
            FROM :Episode, :Fact, :Entity, :Skill, :BadPattern, :Note, :CoreBlock TO :ProvenanceRecord
        ) STRICT"#,
    },
    TypeDdl {
        // Polymorphic marker (AuditEvent → any): spec §5 relaxes both endpoints and it
        // carries no extra properties, so the body is empty (Any → Any).
        name: "AUDIT",
        ddl: r#"CREATE EDGE TYPE IF NOT EXISTS :AUDIT () STRICT"#,
    },
];

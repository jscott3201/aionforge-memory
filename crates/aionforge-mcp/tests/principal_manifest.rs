//! MCP principal, manifest pagination, and current-only recall tests.

use std::future::Future;
use std::sync::Arc;

use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use aionforge_domain::ids::Id;
use aionforge_domain::time::Timestamp;
use aionforge_engine::{Memory, MemoryConfig};
use aionforge_mcp::{
    AuthEnabled, CaptureToolParams, HostPrincipalToolParam, SearchToolParams,
    SessionManifestCursorToolParam, SessionManifestToolParams, capture_tool, search_tool,
    session_manifest_tool,
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
struct FakeEmbedder {
    model: EmbedderModel,
}

impl FakeEmbedder {
    fn new() -> Self {
        Self {
            model: EmbedderModel {
                family: "fake".to_string(),
                version: "1".to_string(),
                dimension: 4,
            },
        }
    }
}

#[derive(Debug)]
struct FakeEmbedError;

impl std::fmt::Display for FakeEmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fake embedder is down")
    }
}

impl std::error::Error for FakeEmbedError {}

impl Embedder for FakeEmbedder {
    type Error = FakeEmbedError;

    fn embed(
        &self,
        inputs: &[String],
    ) -> impl Future<Output = Result<Vec<Embedding>, Self::Error>> + Send {
        let out = inputs
            .iter()
            .map(|_| Embedding::new(vec![1.0, 0.0, 0.0, 0.0]).expect("valid"))
            .collect();
        async move { Ok(out) }
    }

    fn model(&self) -> &EmbedderModel {
        &self.model
    }
}

fn now() -> Timestamp {
    "2026-06-06T09:30:00-05:00[America/Chicago]"
        .parse()
        .expect("valid zoned datetime")
}

fn memory() -> Arc<Memory<FakeEmbedder>> {
    Arc::new(
        Memory::open_in_memory(FakeEmbedder::new(), &now(), MemoryConfig::default())
            .expect("open memory"),
    )
}

fn capture_params(content: &str, agent_id: &str) -> CaptureToolParams {
    CaptureToolParams {
        content: content.to_string(),
        agent_id: Some(agent_id.to_string()),
        principal: None,
        teams: Vec::new(),
        target_namespace: None,
        role: None,
        session_id: None,
        trust: Some(0.1),
        model_family: None,
        captured_at: None,
        supersedes: None,
    }
}

fn capture_id(receipt: &str) -> String {
    receipt
        .split_whitespace()
        .nth(1)
        .expect("compact capture receipt id")
        .to_string()
}

fn host_principal(agent: Id, teams: &[&str]) -> HostPrincipalToolParam {
    HostPrincipalToolParam {
        agent_id: agent.to_string(),
        teams: teams.iter().map(|team| (*team).to_string()).collect(),
    }
}

fn search_params(query: &str, agent: Id) -> SearchToolParams {
    SearchToolParams {
        query: query.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: None,
        verbose: None,
        include_superseded: None,
        fanout: None,
    }
}

fn manifest_params(session_id: Id, agent: Id) -> SessionManifestToolParams {
    SessionManifestToolParams {
        session_id: session_id.to_string(),
        viewer: Some(format!("agent:{agent}")),
        principal: None,
        teams: Vec::new(),
        limit: None,
        after: None,
        include_superseded: None,
        verbose: None,
    }
}

fn manifest_next(output: &str) -> Option<SessionManifestCursorToolParam> {
    let marker = " next=";
    let start = output.find(marker)? + marker.len();
    let end = output[start..].find('\n').unwrap_or(output[start..].len());
    let raw = &output[start..start + end];
    if raw == "none" {
        None
    } else {
        serde_json::from_str(raw).expect("session cursor JSON parses")
    }
}

#[tokio::test]
async fn tools_accept_explicit_host_principal_without_legacy_identity_fields() -> TestResult {
    let memory = memory();
    let writer = Id::generate();
    let reader = Id::generate();

    let mut capture = capture_params("team principal handoff memory", &writer.to_string());
    capture.agent_id = None;
    capture.principal = Some(host_principal(writer, &["squad"]));
    capture.teams = Vec::new();
    capture.target_namespace = Some("team:squad".to_string());
    let receipt = capture_tool(&memory, capture, &now(), None, AuthEnabled(false)).await?;
    assert!(receipt.contains("ns=team:squad"), "{receipt}");

    let found = search_tool(
        &memory,
        SearchToolParams {
            query: "principal handoff".to_string(),
            viewer: None,
            principal: Some(host_principal(reader, &["squad"])),
            teams: Vec::new(),
            limit: None,
            verbose: None,
            include_superseded: None,
            fanout: None,
        },
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    assert!(
        found.contains("team principal handoff memory"),
        "host principal team membership widens recall: {found}"
    );
    Ok(())
}

#[tokio::test]
async fn conflicting_host_principal_identity_is_rejected() -> TestResult {
    let memory = memory();
    let legacy = Id::generate();
    let principal = Id::generate();

    let mut capture = capture_params("conflicting principal memory", &legacy.to_string());
    capture.principal = Some(host_principal(principal, &[]));
    let err = capture_tool(&memory, capture, &now(), None, AuthEnabled(false))
        .await
        .expect_err("capture identity mismatch rejected");
    assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");

    let err = search_tool(
        &memory,
        SearchToolParams {
            query: "conflicting".to_string(),
            viewer: Some(format!("agent:{legacy}")),
            principal: Some(host_principal(principal, &[])),
            teams: Vec::new(),
            limit: None,
            verbose: None,
            include_superseded: None,
            fanout: None,
        },
        &now(),
        None,
        AuthEnabled(false),
    )
    .await
    .expect_err("search identity mismatch rejected");
    assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    Ok(())
}

#[tokio::test]
async fn explicit_host_principal_does_not_inherit_legacy_teams() -> TestResult {
    let memory = memory();
    let writer = Id::generate();
    let reader = Id::generate();

    let mut capture = capture_params("mixed team principal memory", &writer.to_string());
    capture.agent_id = None;
    capture.principal = Some(host_principal(writer, &[]));
    capture.teams = vec!["squad".to_string()];
    capture.target_namespace = Some("team:squad".to_string());
    let err = capture_tool(&memory, capture, &now(), None, AuthEnabled(false))
        .await
        .expect_err("top-level teams cannot extend an explicit principal");
    assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");

    let err = search_tool(
        &memory,
        SearchToolParams {
            query: "mixed team".to_string(),
            viewer: None,
            principal: Some(host_principal(reader, &[])),
            teams: vec!["squad".to_string()],
            limit: None,
            verbose: None,
            include_superseded: None,
            fanout: None,
        },
        &now(),
        None,
        AuthEnabled(false),
    )
    .await
    .expect_err("top-level teams cannot extend an explicit reader principal");
    assert!(err.starts_with("ERR_PRINCIPAL_MISMATCH"), "{err}");
    Ok(())
}

#[tokio::test]
async fn session_manifest_paginates_visible_memories() -> TestResult {
    let memory = memory();
    let session = Id::generate();
    let alice = Id::generate();
    let first: Timestamp = "2026-06-06T09:31:00-05:00[America/Chicago]".parse()?;
    let second: Timestamp = "2026-06-06T09:32:00-05:00[America/Chicago]".parse()?;
    let third: Timestamp = "2026-06-06T09:33:00-05:00[America/Chicago]".parse()?;

    let mut first_params = capture_params("first paged manifest memory", &alice.to_string());
    first_params.session_id = Some(session.to_string());
    let first_id =
        capture_id(&capture_tool(&memory, first_params, &first, None, AuthEnabled(false)).await?);

    let mut second_params = capture_params("second paged manifest memory", &alice.to_string());
    second_params.session_id = Some(session.to_string());
    let second_id =
        capture_id(&capture_tool(&memory, second_params, &second, None, AuthEnabled(false)).await?);

    let mut third_params = capture_params("third paged manifest memory", &alice.to_string());
    third_params.session_id = Some(session.to_string());
    let third_id =
        capture_id(&capture_tool(&memory, third_params, &third, None, AuthEnabled(false)).await?);

    let mut page_one = manifest_params(session, alice);
    page_one.limit = Some(2);
    let page_one = session_manifest_tool(&memory, page_one, None, AuthEnabled(false))?;
    assert!(page_one.contains("count=2"), "{page_one}");
    assert!(page_one.contains("total_visible=3"), "{page_one}");
    assert!(page_one.contains("superseded_hidden=0"), "{page_one}");
    assert!(page_one.contains(&first_id), "{page_one}");
    assert!(
        page_one.contains("first paged manifest memory"),
        "{page_one}"
    );
    assert!(page_one.contains(&second_id), "{page_one}");
    assert!(
        page_one.contains("second paged manifest memory"),
        "{page_one}"
    );
    assert!(!page_one.contains(&third_id), "{page_one}");
    assert!(
        !page_one.contains("third paged manifest memory"),
        "{page_one}"
    );

    let next = manifest_next(&page_one).expect("first page has next cursor");
    assert_eq!(next.id, second_id);
    assert_eq!(next.ingested_at, second.to_string());

    let mut page_two = manifest_params(session, alice);
    page_two.limit = Some(2);
    page_two.after = Some(next);
    let page_two = session_manifest_tool(&memory, page_two, None, AuthEnabled(false))?;
    assert!(page_two.contains("count=1"), "{page_two}");
    assert!(page_two.contains("total_visible=1"), "{page_two}");
    assert!(page_two.contains("superseded_hidden=0"), "{page_two}");
    assert!(!page_two.contains(&first_id), "{page_two}");
    assert!(!page_two.contains(&second_id), "{page_two}");
    assert!(page_two.contains(&third_id), "{page_two}");
    assert!(
        page_two.contains("third paged manifest memory"),
        "{page_two}"
    );
    assert!(manifest_next(&page_two).is_none(), "{page_two}");
    Ok(())
}

#[tokio::test]
async fn session_manifest_reports_total_visible_and_hidden_superseded_counts() -> TestResult {
    let memory = memory();
    let session = Id::generate();
    let alice = Id::generate();
    let first: Timestamp = "2026-06-06T09:31:00-05:00[America/Chicago]".parse()?;
    let second: Timestamp = "2026-06-06T09:32:00-05:00[America/Chicago]".parse()?;
    let third: Timestamp = "2026-06-06T09:33:00-05:00[America/Chicago]".parse()?;

    let mut old = capture_params("obsolete manifest audit memory", &alice.to_string());
    old.session_id = Some(session.to_string());
    let old_id = capture_id(&capture_tool(&memory, old, &first, None, AuthEnabled(false)).await?);

    let mut current = capture_params("still current manifest audit memory", &alice.to_string());
    current.session_id = Some(session.to_string());
    capture_tool(&memory, current, &second, None, AuthEnabled(false)).await?;

    let mut replacement = capture_params("fresh manifest audit memory", &alice.to_string());
    replacement.session_id = Some(session.to_string());
    replacement.supersedes = Some(old_id.clone());
    capture_tool(&memory, replacement, &third, None, AuthEnabled(false)).await?;

    let default = session_manifest_tool(
        &memory,
        manifest_params(session, alice),
        None,
        AuthEnabled(false),
    )?;
    assert!(default.contains("count=3"), "{default}");
    assert!(default.contains("total_visible=3"), "{default}");
    assert!(default.contains("superseded_hidden=0"), "{default}");
    assert!(
        default.contains("obsolete manifest audit memory"),
        "{default}"
    );

    let mut current_only = manifest_params(session, alice);
    current_only.include_superseded = Some(false);
    let current_only = session_manifest_tool(&memory, current_only, None, AuthEnabled(false))?;
    assert!(current_only.contains("count=2"), "{current_only}");
    assert!(current_only.contains("total_visible=2"), "{current_only}");
    assert!(
        current_only.contains("superseded_hidden=1"),
        "{current_only}"
    );
    assert!(
        !current_only.contains("obsolete manifest audit memory"),
        "{current_only}"
    );
    assert!(current_only.contains("still current manifest audit memory"));
    assert!(current_only.contains("fresh manifest audit memory"));
    Ok(())
}

#[tokio::test]
async fn search_can_hide_superseded_episode_evidence() -> TestResult {
    let memory = memory();
    let alice = Id::generate();
    let old = capture_tool(
        &memory,
        capture_params(
            "obsolete superseded lifecycle marker before refresh",
            &alice.to_string(),
        ),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    let old_id = capture_id(&old);

    let mut replacement = capture_params(
        "fresh superseded lifecycle marker after refresh",
        &alice.to_string(),
    );
    replacement.supersedes = Some(old_id.clone());
    let new = capture_tool(&memory, replacement, &now(), None, AuthEnabled(false)).await?;
    let new_id = capture_id(&new);

    let default = search_tool(
        &memory,
        search_params("superseded lifecycle marker", alice),
        &now(),
        None,
        AuthEnabled(false),
    )
    .await?;
    assert!(default.contains(&old_id), "{default}");
    assert!(default.contains("superseded_by=\""), "{default}");

    let mut current_only = search_params("superseded lifecycle marker", alice);
    current_only.include_superseded = Some(false);
    let current_only = search_tool(&memory, current_only, &now(), None, AuthEnabled(false)).await?;
    assert!(current_only.contains(&new_id), "{current_only}");
    assert!(
        !current_only.contains(&format!("<memory id=\"{old_id}\"")),
        "current-only recall hides the superseded episode: {current_only}"
    );
    assert!(
        !current_only.contains("obsolete superseded lifecycle marker before refresh"),
        "{current_only}"
    );
    Ok(())
}

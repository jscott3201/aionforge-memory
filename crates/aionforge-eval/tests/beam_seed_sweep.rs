//! On-demand BEAM source-recall sweep against a LIVE Aionforge MCP store — the
//! "seed-once / sweep-via-search" client.
//!
//! This is the persistent-store sibling of `beam_floor_recall.rs`. Where that runner
//! embeds locally and seeds an in-process [`Store`](aionforge_store::Store), this one talks
//! to a *running* server over Streamable HTTP so the **server** owns embedding and floor
//! application — i.e. it exercises the real production path end to end:
//!
//! 1. **Seed once.** Every BEAM message is `capture`d into the store. The server embeds it
//!    (at whatever dimension that deployment is configured for — the test-env container is
//!    a native gemini-1536 Matryoshka store), so re-embedding never happens on the query
//!    side. Each conversation seeds under its OWN deterministic `agent_id` namespace, and
//!    each probe searches with `viewer` scoped to just that namespace. That reproduces
//!    BEAM's retriever-per-conversation isolation on one shared store, which keeps the
//!    numbers apples-to-apples with the in-process `beam_floor_recall` measurement.
//!    Re-running is idempotent: the store deduplicates by content hash, so a second seed
//!    embeds nothing and resumes a partial one.
//!
//! 2. **Sweep via search.** For each probe we call `search` once per floor value, letting
//!    the server apply `min_relevance`. We must apply the floor server-side, not by
//!    filtering the printed `score`: the printed score is a *fused* rank score, while the
//!    floor gates on raw *dense cosine* (verified empirically — a 0.075-fused hit survives
//!    `min_relevance=0.6`). `min_relevance: None` is the production per-class config
//!    (SingleHopFactual at 0.60, others off).
//!
//! The deliverable is the printed report: the native-served floor/recall numbers to set
//! against the #284 in-process gemini-1536 A/B, closing the loop on the gemini-1536
//! production-greenfield decision.
//!
//! It is `#[ignore]` and additionally a no-op when the BEAM data is absent or the server is
//! unreachable, so it never runs in CI and never fails for want of infrastructure.
//!
//! BEAM (`Mohammadta/BEAM`) is **CC BY-SA 4.0 and never vendored**. Prepare the data and
//! point the runner at a live container:
//!
//! ```bash
//! python3 crates/aionforge-eval/tools/prepare_beam.py --conversations 20
//! # the test-env 1536 container must be up (see the steward memory for the deploy recipe):
//! #   docker ps --filter name=aionforge-memory-test   # -> 127.0.0.1:3919
//! AIONFORGE_BEAM_MCP_URL=http://127.0.0.1:3919/mcp \
//!   cargo test -p aionforge-eval --test beam_seed_sweep -- --ignored --nocapture
//! ```

// This runner's output IS its deliverable: a human reads the printed tables. The workspace
// keeps print_stdout at warn for exactly such cases; allow it here.
#![allow(clippy::print_stdout)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use aionforge_eval::parse_conversations;
use reqwest::header::ACCEPT;
use serde_json::{Value, json};

/// Default endpoint: the isolated gemini-1536 test-env container (never prod on 3918).
const DEFAULT_URL: &str = "http://127.0.0.1:3919/mcp";
const URL_ENV: &str = "AIONFORGE_BEAM_MCP_URL";
/// Override the normalized BEAM JSONL path; defaults to the prepare_beam.py output.
const DATA_ENV: &str = "AIONFORGE_BEAM_DATA";
/// Comma-separated uniform floors to sweep (server-applied). `0.0` and `0.60` are always
/// included so the baseline and the live-factual-floor arms exist regardless.
const FLOORS_ENV: &str = "AIONFORGE_BEAM_FLOORS";
/// Cap the number of conversations seeded/queried (default: all in the file).
const MAX_CONVS_ENV: &str = "AIONFORGE_BEAM_MAX_CONVS";

/// Recall budget per query (mirrors `beam_floor_recall`: admits recall@20 over a long
/// conversation).
const LIMIT: usize = 25;
const FANOUT: usize = 64;
/// The k values recall is reported at.
const KS: [usize; 3] = [5, 10, 20];
/// Headline cutoff.
const K: usize = 10;
/// The live SingleHopFactual floor (router `FACTUAL_FLOOR`).
const FACTUAL_FLOOR: f64 = 0.60;
/// The query class the floor touches, as the search header spells it.
const FACTUAL_CLASS: &str = "single_hop_factual";

/// #284 in-process gemini-1536 reference (SingleHopFactual recall@10), for the comparison
/// line. base = no floor, prod = per-class floors on.
const REF_1536_BASE: f64 = 0.515;
const REF_1536_PROD: f64 = 0.559;

/// How many times to retry one MCP call on a transient failure before giving up.
const CALL_RETRIES: u32 = 5;

// ---------------------------------------------------------------------------------------
// Minimal Streamable-HTTP MCP client (JSON-RPC over a single POST per call; the server
// answers as one SSE `data:` frame and closes the stream). Stateful: the session id from
// the initialize handshake is echoed on every subsequent request.
// ---------------------------------------------------------------------------------------

struct Mcp {
    http: reqwest::Client,
    url: String,
    session: String,
    next_id: u64,
}

impl Mcp {
    async fn connect(url: &str) -> Result<Self, String> {
        // reqwest is pinned to rustls-no-provider, which panics at Client build unless a
        // provider is installed first. Idempotent: ignore the "already installed" error.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;

        let init = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "beam-seed-sweep", "version": "0"}
            }
        });
        let resp = http
            .post(url)
            .header(ACCEPT, "application/json, text/event-stream")
            .json(&init)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let session = resp
            .headers()
            .get("mcp-session-id")
            .ok_or("server returned no mcp-session-id (is it stateful Streamable HTTP?)")?
            .to_str()
            .map_err(|e| e.to_string())?
            .to_string();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        sse_json(&body)?; // validate the handshake actually returned a result frame

        let me = Self {
            http,
            url: url.to_string(),
            session,
            next_id: 2,
        };
        // The protocol requires the initialized notification before any tools/call.
        me.http
            .post(&me.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-session-id", &me.session)
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(me)
    }

    /// One `tools/call`, returning the first text-content block (the tool's receipt line).
    async fn call(&mut self, name: &str, args: &Value) -> Result<String, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        });
        let body = self
            .http
            .post(&self.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-session-id", &self.session)
            .json(&req)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        let value = sse_json(&body)?;
        if let Some(error) = value.get("error") {
            return Err(format!("{name} error: {error}"));
        }
        if value.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
            let text = value
                .pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("<no text>");
            return Err(format!("{name} isError: {text}"));
        }
        value
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{name}: no text content in {body:?}"))
    }

    /// `call` with exponential-backoff retry — a long run must survive a single blip.
    async fn call_retrying(&mut self, name: &str, args: &Value) -> String {
        let mut attempt = 0u32;
        loop {
            match self.call(name, args).await {
                Ok(text) => return text,
                Err(error) => {
                    attempt += 1;
                    assert!(
                        attempt < CALL_RETRIES,
                        "{name} failed after {attempt}: {error}"
                    );
                    println!("  {name} transient error (attempt {attempt}), retrying: {error}");
                    tokio::time::sleep(Duration::from_millis(500u64 << attempt)).await;
                }
            }
        }
    }

    /// Capture one episode; returns the assigned store id (`None` if the capture pipeline
    /// filtered it out and no memory was committed) plus the verdict token.
    async fn capture(&mut self, ns: &str, content: &str, role: &str) -> (Option<String>, String) {
        let receipt = self
            .call_retrying(
                "capture",
                &json!({"agent_id": ns, "content": content, "role": role}),
            )
            .await;
        parse_capture(&receipt)
    }

    /// Search within one namespace at one floor (`None` = production per-class floors).
    async fn search(&mut self, query: &str, ns: &str, floor: Option<f64>) -> SearchOut {
        let mut args = json!({
            "query": query, "viewer": format!("agent:{ns}"),
            "limit": LIMIT, "fanout": FANOUT
        });
        if let Some(floor) = floor {
            args["min_relevance"] = json!(floor);
        }
        let text = self.call_retrying("search", &args).await;
        SearchOut {
            ranked: parse_memory_ids(&text),
            class: parse_class(&text),
        }
    }
}

struct SearchOut {
    /// Returned memory ids in rank order (best first).
    ranked: Vec<String>,
    /// The routed query class, as the search header spells it.
    class: String,
}

/// Extract the single JSON-RPC object from an SSE body: the last parseable `data:` frame.
fn sse_json(body: &str) -> Result<Value, String> {
    let mut last = None;
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            last = Some(value);
        }
    }
    last.ok_or_else(|| format!("no JSON data frame in SSE response: {body:?}"))
}

/// Parse a capture receipt `[capture] <id> verdict=<v> ...` into (id, verdict). The id is
/// `None` when the line carries no committed memory id (a filtered capture).
fn parse_capture(receipt: &str) -> (Option<String>, String) {
    let verdict = field(receipt, "verdict=").unwrap_or_default();
    let id = receipt
        .strip_prefix("[capture]")
        .and_then(|rest| rest.split_whitespace().next())
        .filter(|token| token.len() >= 32 && token.contains('-'))
        .map(str::to_string);
    (id, verdict)
}

/// Pull `<memory id="...">` ids out of a recall wrapper, in document (rank) order.
fn parse_memory_ids(text: &str) -> Vec<String> {
    text.split("<memory id=\"")
        .skip(1)
        .filter_map(|segment| segment.split_once('"').map(|(id, _)| id.to_string()))
        .collect()
}

/// Read the routed class from the search header `... | class=single_hop_factual | ...`.
fn parse_class(text: &str) -> String {
    field(text, "class=").unwrap_or_default()
}

/// Read a `key=value` token's value, stopping at whitespace or a `|` separator.
fn field(text: &str, key: &str) -> Option<String> {
    let rest = &text[text.find(key)? + key.len()..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Source-recall@k over wire-string ids: the fraction of the gold set in the top `k`.
fn recall_at_k(ranked: &[String], gold: &HashSet<String>, k: usize) -> f64 {
    if gold.is_empty() {
        return 1.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|id| gold.contains(*id))
        .count();
    hits as f64 / gold.len() as f64
}

/// A deterministic per-conversation namespace UUID, so re-seeding is stable (and dedup
/// makes it idempotent). FNV-1a of the conversation id, dropped into a valid v4-shaped
/// UUID — the server only needs it to parse.
fn conversation_namespace(conversation_id: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in conversation_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("beac0000-0000-4000-8000-{:012x}", hash & 0xffff_ffff_ffff)
}

/// BEAM roles are `user` / `assistant`; map anything else to `user`.
fn capture_role(role: &str) -> &'static str {
    if role.eq_ignore_ascii_case("assistant") {
        "assistant"
    } else {
        "user"
    }
}

// ---------------------------------------------------------------------------------------
// Accumulation.
// ---------------------------------------------------------------------------------------

/// A running tally for one measurement arm (one floor, or the production config).
#[derive(Default, Clone)]
struct Arm {
    positives: f64,
    negatives: f64,
    negatives_rejected: f64,
    recall: [f64; 3],
    false_rejected: f64,
}

impl Arm {
    fn observe_positive(&mut self, ranked: &[String], gold: &HashSet<String>) {
        self.positives += 1.0;
        for (slot, &k) in self.recall.iter_mut().zip(KS.iter()) {
            *slot += recall_at_k(ranked, gold, k);
        }
        let returned: HashSet<&String> = ranked.iter().collect();
        if !gold.is_empty() && gold.iter().all(|id| !returned.contains(id)) {
            self.false_rejected += 1.0;
        }
    }

    fn observe_negative(&mut self, rejected: bool) {
        self.negatives += 1.0;
        if rejected {
            self.negatives_rejected += 1.0;
        }
    }

    fn recall_at(&self, k: usize) -> f64 {
        let idx = KS.iter().position(|&x| x == k).expect("k in KS");
        if self.positives == 0.0 {
            0.0
        } else {
            self.recall[idx] / self.positives
        }
    }

    fn rejection_rate(&self) -> f64 {
        if self.negatives == 0.0 {
            1.0
        } else {
            self.negatives_rejected / self.negatives
        }
    }

    fn false_rejection_rate(&self) -> f64 {
        if self.positives == 0.0 {
            0.0
        } else {
            self.false_rejected / self.positives
        }
    }
}

/// Every accumulator for the whole run.
#[derive(Default)]
struct Acc {
    /// One arm per uniform floor in `floors`.
    sweep: Vec<Arm>,
    /// `min_relevance: None` — the shipped per-class behavior.
    production: Arm,
    /// SingleHopFactual probes only, at floor 0.0 (the honest baseline of the floored class).
    factual_baseline: Arm,
    /// SingleHopFactual probes only, under production floors (the live blast radius).
    factual_production: Arm,
    /// How probes routed across classes (production run).
    class_counts: BTreeMap<String, usize>,
    /// Per BEAM ability: (baseline 0.0, production) recall arms.
    by_ability: BTreeMap<String, (Arm, Arm)>,
}

fn default_data_path() -> PathBuf {
    if let Ok(path) = std::env::var(DATA_ENV) {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".aionforge")
        .join("beam-data")
        .join("normalized")
        .join("beam-100k.jsonl")
}

/// Uniform floors to sweep: the env list (or `[0.0, 0.60]`), with 0.0 and the factual floor
/// always present, deduped and ascending.
fn sweep_floors() -> Vec<f64> {
    let mut floors: Vec<f64> = std::env::var(FLOORS_ENV)
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|piece| piece.trim().parse::<f64>().ok())
                .filter(|f| (0.0..=1.0).contains(f))
                .collect()
        })
        .unwrap_or_else(|| vec![0.0, FACTUAL_FLOOR]);
    for required in [0.0, FACTUAL_FLOOR] {
        if !floors.iter().any(|f| (f - required).abs() < 1e-9) {
            floors.push(required);
        }
    }
    floors.sort_by(|a, b| a.partial_cmp(b).expect("finite floors"));
    floors.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    floors
}

fn baseline_index(floors: &[f64]) -> usize {
    floors
        .iter()
        .position(|f| f.abs() < 1e-9)
        .expect("0.0 floor present")
}

#[tokio::test]
// The `#[ignore]` is the load-bearing CI gate: the workspace runs do NOT pass `--ignored`,
// so this never executes (or makes a network call) there. The data/reachability gates below
// are a second line of defense for an explicit local invocation.
#[ignore = "on-demand: drives the live test-env MCP container and needs external BEAM data; run with --ignored"]
async fn beam_seed_sweep() {
    let data_path = default_data_path();
    let Ok(data) = std::fs::read_to_string(&data_path) else {
        println!(
            "skipping beam_seed_sweep: no BEAM data at {} (run tools/prepare_beam.py, or set {DATA_ENV})",
            data_path.display()
        );
        return;
    };
    let mut conversations = parse_conversations(&data).expect("parse BEAM conversations");
    assert!(!conversations.is_empty(), "BEAM data has conversations");
    if let Some(max) = std::env::var(MAX_CONVS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
    {
        conversations.truncate(max);
    }

    let url = std::env::var(URL_ENV).unwrap_or_else(|_| DEFAULT_URL.to_string());
    let Ok(mut mcp) = Mcp::connect(&url).await else {
        println!(
            "skipping beam_seed_sweep: cannot reach MCP at {url} (is the test-env container up? \
             docker ps --filter name=aionforge-memory-test)"
        );
        return;
    };

    let floors = sweep_floors();
    let baseline = baseline_index(&floors);
    println!(
        "seed-once/sweep-via-search against {url}\n  {} conversations, floors {:?} (+ production None)\n",
        conversations.len(),
        floors
    );

    // ---- Seed once. ----
    let mut id_map: HashMap<(String, String), String> = HashMap::new();
    let (mut seeded, mut filtered) = (0usize, 0usize);
    for (index, conversation) in conversations.iter().enumerate() {
        let ns = conversation_namespace(&conversation.conversation_id);
        let mut conv_filtered = 0usize;
        for message in &conversation.messages {
            let (id, _verdict) = mcp
                .capture(&ns, &message.text, capture_role(&message.role))
                .await;
            match id {
                Some(id) => {
                    id_map.insert(
                        (conversation.conversation_id.clone(), message.id.clone()),
                        id,
                    );
                    seeded += 1;
                }
                None => {
                    conv_filtered += 1;
                    filtered += 1;
                }
            }
        }
        println!(
            "  seeded conv {} [{}/{}]: {} msgs ({} filtered)",
            conversation.conversation_id,
            index + 1,
            conversations.len(),
            conversation.messages.len(),
            conv_filtered
        );
    }
    assert!(seeded > 0, "seeded at least one BEAM message");
    println!("\nseed complete: {seeded} episodes committed, {filtered} filtered by capture\n");

    // ---- Sweep via search. ----
    let mut acc = Acc {
        sweep: vec![Arm::default(); floors.len()],
        ..Acc::default()
    };
    let (mut total_probes, mut joined_gold) = (0usize, 0usize);
    for conversation in &conversations {
        let ns = conversation_namespace(&conversation.conversation_id);
        for probe in &conversation.probes {
            total_probes += 1;
            let gold: HashSet<String> = probe
                .source_ids
                .iter()
                .filter_map(|sid| {
                    id_map
                        .get(&(conversation.conversation_id.clone(), sid.clone()))
                        .cloned()
                })
                .collect();
            joined_gold += gold.len();
            let negative = probe.is_negative() || gold.is_empty();

            let production = mcp.search(&probe.question, &ns, None).await;
            *acc.class_counts
                .entry(production.class.clone())
                .or_default() += 1;
            let is_factual = production.class == FACTUAL_CLASS;

            let mut floored = Vec::with_capacity(floors.len());
            for &floor in &floors {
                floored.push(mcp.search(&probe.question, &ns, Some(floor)).await);
            }

            let abilities = acc.by_ability.entry(probe.ability.clone()).or_default();
            if negative {
                acc.production
                    .observe_negative(production.ranked.is_empty());
                for (arm, out) in acc.sweep.iter_mut().zip(&floored) {
                    arm.observe_negative(out.ranked.is_empty());
                }
                abilities
                    .0
                    .observe_negative(floored[baseline].ranked.is_empty());
                abilities.1.observe_negative(production.ranked.is_empty());
            } else {
                acc.production.observe_positive(&production.ranked, &gold);
                for (arm, out) in acc.sweep.iter_mut().zip(&floored) {
                    arm.observe_positive(&out.ranked, &gold);
                }
                abilities
                    .0
                    .observe_positive(&floored[baseline].ranked, &gold);
                abilities.1.observe_positive(&production.ranked, &gold);
                if is_factual {
                    acc.factual_baseline
                        .observe_positive(&floored[baseline].ranked, &gold);
                    acc.factual_production
                        .observe_positive(&production.ranked, &gold);
                }
            }
        }
    }

    print_report(
        &acc,
        &floors,
        conversations.len(),
        seeded,
        total_probes,
        joined_gold,
    );

    assert!(
        acc.production.positives > 0.0,
        "BEAM produced positive probes (gold joined to seeded episodes)"
    );
}

fn print_report(
    acc: &Acc,
    floors: &[f64],
    conversations: usize,
    seeded: usize,
    probes: usize,
    joined_gold: usize,
) {
    println!(
        "\n================ BEAM seed-once / sweep-via-search (LIVE MCP) ================\n\
         {conversations} conversations, {seeded} seeded episodes, {probes} probes, \
         {joined_gold} gold links\n(server-side embedding + floor; per-conversation \
         namespace isolation; gold = a probe's cited evidence)\n"
    );

    println!("UNIFORM FLOOR SWEEP (one server-applied floor across every query):");
    println!("floor  reject  false_rej  recall@{K}\n------------------------------------------");
    for (arm, &floor) in acc.sweep.iter().zip(floors.iter()) {
        println!(
            "{:<6.2} {:<7.3} {:<10.3} {:.3}",
            floor,
            arm.rejection_rate(),
            arm.false_rejection_rate(),
            arm.recall_at(K),
        );
    }

    println!(
        "\nPRODUCTION CONFIG (per-class floors; SingleHopFactual=0.60, others off):\n  \
         recall@5={:.3} recall@10={:.3} recall@20={:.3} false_rej={:.3} abstention_reject={:.3}",
        acc.production.recall_at(5),
        acc.production.recall_at(10),
        acc.production.recall_at(20),
        acc.production.false_rejection_rate(),
        acc.production.rejection_rate(),
    );

    let cost = acc.factual_baseline.recall_at(K) - acc.factual_production.recall_at(K);
    println!(
        "\nSingleHopFactual-ONLY (the only class the floor touches), n={}:\n  \
         baseline (0.0):    recall@5={:.3} recall@10={:.3} recall@20={:.3}\n  \
         production (0.60): recall@5={:.3} recall@10={:.3} recall@20={:.3}  false_rej={:.3}\n  \
         >>> recall@{K} cost of the live 0.60 factual floor: {cost:+.3}",
        acc.factual_baseline.positives as usize,
        acc.factual_baseline.recall_at(5),
        acc.factual_baseline.recall_at(10),
        acc.factual_baseline.recall_at(20),
        acc.factual_production.recall_at(5),
        acc.factual_production.recall_at(10),
        acc.factual_production.recall_at(20),
        acc.factual_production.false_rejection_rate(),
    );

    println!(
        "\nVS #284 IN-PROCESS gemini-1536 (SingleHopFactual recall@{K}):\n  \
         base   native={:.3}  in-process={:.3}  delta={:+.3}\n  \
         prod   native={:.3}  in-process={:.3}  delta={:+.3}\n  \
         (deltas near zero confirm a natively-served 1536 vector behaves like a \
         locally-truncated one)",
        acc.factual_baseline.recall_at(K),
        REF_1536_BASE,
        acc.factual_baseline.recall_at(K) - REF_1536_BASE,
        acc.factual_production.recall_at(K),
        REF_1536_PROD,
        acc.factual_production.recall_at(K) - REF_1536_PROD,
    );

    println!("\nROUTED QUERY-CLASS DISTRIBUTION:");
    for (class, count) in &acc.class_counts {
        println!(
            "  {class:<20} {count:>4} ({:>5.1}%)",
            *count as f64 / probes.max(1) as f64 * 100.0
        );
    }

    println!("\nPER-ABILITY recall@{K} (baseline 0.0 / production):");
    for (ability, (base, prod)) in &acc.by_ability {
        println!(
            "  {ability:<24} {:.3} / {:.3}   (pos={})",
            base.recall_at(K),
            prod.recall_at(K),
            base.positives as usize,
        );
    }
}

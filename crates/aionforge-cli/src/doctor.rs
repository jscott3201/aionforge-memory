//! Doctor command execution and rendering.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use aionforge::{Memory, MemoryDoctorReport, Store, Timestamp};
use aionforge_config::Config;

use crate::cli::DoctorArgs;
use crate::error::CliError;
use crate::host::{HostOptions, load_config, memory_config, runtime_embedder};

#[derive(Debug)]
pub(crate) struct DoctorOutcome {
    pub(crate) ok: bool,
    pub(crate) rendered: String,
}

pub(crate) fn run(options: &HostOptions, args: DoctorArgs) -> Result<DoctorOutcome, CliError> {
    let config = load_config(options)?;
    run_with_config(config, &options.config_path, args)
}

fn run_with_config(
    config: Config,
    config_path: &Path,
    args: DoctorArgs,
) -> Result<DoctorOutcome, CliError> {
    let now = Timestamp::now();
    let memory_config = memory_config(&config)?;
    let embedder = runtime_embedder(&config)?;
    let store = Arc::new(Store::open_or_recover(
        config.data_dir(),
        config.store_config(),
        &now,
    )?);
    let memory = Memory::new(store, embedder, memory_config, &now)?;
    let report = memory.doctor_report()?;
    let rendered = if args.json {
        render_json(config_path, config.data_dir(), &report)?
    } else {
        render_human(config_path, config.data_dir(), &report)?
    };
    Ok(DoctorOutcome {
        ok: report.ok,
        rendered,
    })
}

fn render_json(
    config_path: &Path,
    data_dir: &Path,
    report: &MemoryDoctorReport,
) -> Result<String, CliError> {
    let value = serde_json::json!({
        "ok": report.ok,
        "config": {
            "config_path": config_path.display().to_string(),
            "data_dir": data_dir.display().to_string(),
        },
        "store": &report.store,
        "embedder": {
            "ok": report.embedder.ok,
            "model": &report.embedder.model,
            "store_config_dimension": report.embedder.store_config_dimension,
            "matches_store_config": report.embedder.matches_store_config,
            "vector_dimension_mismatches": &report.embedder.vector_dimension_mismatches,
        },
    });
    Ok(serde_json::to_string_pretty(&value)?)
}

fn render_human(
    config_path: &Path,
    data_dir: &Path,
    report: &MemoryDoctorReport,
) -> Result<String, CliError> {
    let store = &report.store;
    let indexes = &store.indexes;
    let providers = &store.providers;
    let lag = &store.consolidation_lag;
    let capacity = &store.capacity;
    let embedder = &report.embedder;

    let mut out = String::new();
    writeln!(out, "aionforge doctor: {}", status(report.ok))?;
    writeln!(
        out,
        "config: path={} data_dir={}",
        config_path.display(),
        data_dir.display()
    )?;
    writeln!(
        out,
        "schema: {} version={}/{} bound={} node_types={} edge_types={}",
        status(store.schema.ok),
        store.schema.current_version,
        store.schema.target_version,
        store.schema.schema_bound,
        store.schema.node_type_count,
        store.schema.edge_type_count
    )?;
    writeln!(
        out,
        "indexes: {} vectors={} text={} properties={} composites={} dim_mismatches={} kind_mismatches={}",
        status(indexes.ok),
        indexes.vector_indexes.actual.len(),
        indexes.text_indexes.actual.len(),
        indexes.property_indexes.actual.len(),
        indexes.composite_indexes.actual.len(),
        indexes.vector_dimension_mismatches.len(),
        indexes.vector_kind_mismatches.len()
    )?;
    write_inventory_issues(&mut out, "vector indexes", &indexes.vector_indexes)?;
    write_inventory_issues(&mut out, "text indexes", &indexes.text_indexes)?;
    write_inventory_issues(&mut out, "property indexes", &indexes.property_indexes)?;
    write_inventory_issues(&mut out, "composite indexes", &indexes.composite_indexes)?;
    if !indexes.vector_dimension_mismatches.is_empty() {
        writeln!(
            out,
            "index dimension mismatches: {:?}",
            indexes.vector_dimension_mismatches
        )?;
    }
    if !indexes.vector_kind_mismatches.is_empty() {
        writeln!(
            out,
            "index kind mismatches: {:?}",
            indexes.vector_kind_mismatches
        )?;
    }
    writeln!(
        out,
        "providers: {} candidate_states={} watermarks={}",
        status(providers.ok),
        providers.candidate_states.actual.len(),
        providers.candidate_state_infos.len()
    )?;
    write_inventory_issues(&mut out, "candidate states", &providers.candidate_states)?;
    writeln!(
        out,
        "embedder: {} model={} dimension={} store_dimension={} vector_dim_mismatches={}",
        status(embedder.ok),
        embedder.model.family,
        embedder.model.dimension,
        embedder.store_config_dimension,
        embedder.vector_dimension_mismatches.len()
    )?;
    writeln!(
        out,
        "consolidation: pending={} failed={} oldest_pending={}",
        lag.episodes_pending,
        lag.episodes_failed,
        lag.oldest_pending_captured_at
            .as_ref()
            .map_or_else(|| "none".to_owned(), ToString::to_string)
    )?;
    writeln!(
        out,
        "capacity: generation={} nodes={} edges={}",
        capacity.generation, capacity.node_count, capacity.edge_count
    )?;
    Ok(out.trim_end().to_owned())
}

fn write_inventory_issues<T: std::fmt::Debug>(
    out: &mut String,
    label: &str,
    check: &aionforge_store::InventoryCheck<T>,
) -> Result<(), std::fmt::Error> {
    if !check.missing.is_empty() {
        writeln!(out, "{label} missing: {:?}", check.missing)?;
    }
    if !check.unexpected.is_empty() {
        writeln!(out, "{label} unexpected: {:?}", check.unexpected)?;
    }
    Ok(())
}

fn status(ok: bool) -> &'static str {
    if ok { "ok" } else { "fail" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge::Embedder;
    use aionforge_config::Config;

    #[test]
    fn human_doctor_report_opens_a_fresh_store_without_network() {
        let dir = unique_dir("human");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();

        let outcome = run_with_config(
            config,
            Path::new("/tmp/missing.toml"),
            DoctorArgs { json: false },
        )
        .expect("doctor report");

        assert!(
            outcome.ok,
            "fresh migrated store is healthy: {}",
            outcome.rendered
        );
        assert!(outcome.rendered.contains("aionforge doctor: ok"));
        assert!(outcome.rendered.contains("schema: ok"));
        assert!(outcome.rendered.contains("embedder: ok"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_doctor_report_carries_store_and_embedder_sections() {
        let dir = unique_dir("json");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();

        let outcome = run_with_config(
            config,
            Path::new("/tmp/missing.toml"),
            DoctorArgs { json: true },
        )
        .expect("doctor report");
        let value: serde_json::Value = serde_json::from_str(&outcome.rendered).expect("valid json");

        assert_eq!(value["ok"], true);
        assert_eq!(value["store"]["ok"], true);
        assert_eq!(value["embedder"]["model"]["dimension"], 1536);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_runtime_embedder_still_reports_the_configured_dimension() {
        let mut config = Config::default();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();

        let embedder = runtime_embedder(&config).expect("disabled embedder builds");
        assert_eq!(embedder.model().family, "disabled");
        assert_eq!(embedder.model().dimension, config.embedder.dimension);

        let memory_config = memory_config(&config).expect("memory config");
        assert!(!memory_config.capture.embed_on_capture);
    }

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "aionforge-cli-doctor-{label}-{}-{nanos}",
            std::process::id()
        ))
    }
}

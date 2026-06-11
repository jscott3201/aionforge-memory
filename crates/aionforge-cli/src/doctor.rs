//! Doctor command execution and rendering.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use aionforge::{MemoryDoctorReport, Store};
use aionforge_config::Config;

use crate::cli::DoctorArgs;
use crate::error::CliError;
use crate::host::{HostOptions, load_config, open_memory};

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
    let persistence = PersistenceProbe::inspect(config.data_dir());
    let memory = match open_memory(&config) {
        Ok(memory) => memory,
        Err(error @ CliError::Store(_)) => {
            let rendered = render_unavailable(
                "doctor",
                config_path,
                config.data_dir(),
                &persistence,
                &error.to_string(),
                args.json,
            )?;
            return Ok(DoctorOutcome {
                ok: false,
                rendered,
            });
        }
        Err(error) => return Err(error),
    };
    let report = memory.doctor_report()?;
    let rendered = if args.json {
        render_json(config_path, config.data_dir(), &persistence, &report)?
    } else {
        render_human(
            "doctor",
            config_path,
            config.data_dir(),
            &persistence,
            &report,
        )?
    };
    Ok(DoctorOutcome {
        ok: report.ok,
        rendered,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistenceProbe {
    wal_path: PathBuf,
    wal_present: bool,
    wal_size_bytes: Option<u64>,
    wal_metadata_error: Option<String>,
}

impl PersistenceProbe {
    pub(crate) fn inspect(data_dir: &Path) -> Self {
        let wal_path = data_dir.join(Store::WAL_FILE_NAME);
        match std::fs::metadata(&wal_path) {
            Ok(metadata) => Self {
                wal_path,
                wal_present: true,
                wal_size_bytes: Some(metadata.len()),
                wal_metadata_error: None,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self {
                wal_path,
                wal_present: false,
                wal_size_bytes: None,
                wal_metadata_error: None,
            },
            Err(error) => Self {
                wal_path,
                wal_present: false,
                wal_size_bytes: None,
                wal_metadata_error: Some(error.to_string()),
            },
        }
    }

    fn mode(&self) -> &'static str {
        if self.wal_present { "recover" } else { "fresh" }
    }
}

pub(crate) fn render_json(
    config_path: &Path,
    data_dir: &Path,
    persistence: &PersistenceProbe,
    report: &MemoryDoctorReport,
) -> Result<String, CliError> {
    let value = serde_json::json!({
        "ok": report.ok,
        "config": {
            "config_path": config_path.display().to_string(),
            "data_dir": data_dir.display().to_string(),
        },
        "store_open": {
            "ok": true,
            "mode": persistence.mode(),
            "error": null,
        },
        "persistence": persistence_json(persistence),
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

pub(crate) fn render_unavailable(
    command: &str,
    config_path: &Path,
    data_dir: &Path,
    persistence: &PersistenceProbe,
    error: &str,
    json: bool,
) -> Result<String, CliError> {
    if json {
        return render_unavailable_json(config_path, data_dir, persistence, error);
    }
    render_unavailable_human(command, config_path, data_dir, persistence, error)
}

fn render_unavailable_json(
    config_path: &Path,
    data_dir: &Path,
    persistence: &PersistenceProbe,
    error: &str,
) -> Result<String, CliError> {
    let value = serde_json::json!({
        "ok": false,
        "config": {
            "config_path": config_path.display().to_string(),
            "data_dir": data_dir.display().to_string(),
        },
        "store_open": {
            "ok": false,
            "mode": persistence.mode(),
            "error": error,
        },
        "persistence": persistence_json(persistence),
        "store": null,
        "embedder": null,
    });
    Ok(serde_json::to_string_pretty(&value)?)
}

pub(crate) fn render_human(
    command: &str,
    config_path: &Path,
    data_dir: &Path,
    persistence: &PersistenceProbe,
    report: &MemoryDoctorReport,
) -> Result<String, CliError> {
    let store = &report.store;
    let indexes = &store.indexes;
    let providers = &store.providers;
    let lag = &store.consolidation_lag;
    let capacity = &store.capacity;
    let embedder = &report.embedder;

    let mut out = String::new();
    writeln!(out, "aionforge {command}: {}", status(report.ok))?;
    writeln!(
        out,
        "config: path={} data_dir={}",
        config_path.display(),
        data_dir.display()
    )?;
    write_store_open(&mut out, true, persistence, None)?;
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

fn render_unavailable_human(
    command: &str,
    config_path: &Path,
    data_dir: &Path,
    persistence: &PersistenceProbe,
    error: &str,
) -> Result<String, CliError> {
    let mut out = String::new();
    writeln!(out, "aionforge {command}: fail")?;
    writeln!(
        out,
        "config: path={} data_dir={}",
        config_path.display(),
        data_dir.display()
    )?;
    write_store_open(&mut out, false, persistence, Some(error))?;
    Ok(out.trim_end().to_owned())
}

fn persistence_json(persistence: &PersistenceProbe) -> serde_json::Value {
    serde_json::json!({
        "wal": {
            "path": persistence.wal_path.display().to_string(),
            "present": persistence.wal_present,
            "size_bytes": persistence.wal_size_bytes,
            "metadata_error": persistence.wal_metadata_error,
        }
    })
}

fn write_store_open(
    out: &mut String,
    ok: bool,
    persistence: &PersistenceProbe,
    error: Option<&str>,
) -> Result<(), std::fmt::Error> {
    write!(
        out,
        "store_open: {} mode={} wal_present={} wal_size_bytes={} wal_path={}",
        status(ok),
        persistence.mode(),
        persistence.wal_present,
        persistence
            .wal_size_bytes
            .map_or_else(|| "unknown".to_owned(), |size| size.to_string()),
        persistence.wal_path.display()
    )?;
    if let Some(metadata_error) = &persistence.wal_metadata_error {
        write!(out, " wal_metadata_error={metadata_error}")?;
    }
    if let Some(error) = error {
        write!(out, " error={error}")?;
    }
    writeln!(out)
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

    use crate::host::{memory_config, runtime_embedder};

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
        assert!(outcome.rendered.contains("store_open: ok mode=fresh"));
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
        assert_eq!(value["store_open"]["ok"], true);
        assert_eq!(value["store_open"]["mode"], "fresh");
        assert_eq!(value["persistence"]["wal"]["present"], false);
        assert_eq!(value["store"]["ok"], true);
        assert_eq!(value["embedder"]["model"]["dimension"], 1536);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_doctor_report_surfaces_corrupt_wal_without_losing_json() {
        let dir = unique_dir("bad-wal-json");
        std::fs::create_dir_all(&dir).expect("create data dir");
        std::fs::write(dir.join(Store::WAL_FILE_NAME), b"not a selene wal").expect("write bad WAL");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();

        let outcome = run_with_config(
            config,
            Path::new("/tmp/aionforge.toml"),
            DoctorArgs { json: true },
        )
        .expect("doctor renders a structured failure");
        let value: serde_json::Value = serde_json::from_str(&outcome.rendered).expect("valid json");

        assert!(!outcome.ok);
        assert_eq!(value["ok"], false);
        assert_eq!(value["store_open"]["ok"], false);
        assert_eq!(value["store_open"]["mode"], "recover");
        assert_eq!(value["persistence"]["wal"]["present"], true);
        assert!(value["store"].is_null());
        assert!(value["embedder"].is_null());
        let error = value["store_open"]["error"].as_str().expect("error string");
        assert!(
            error.to_ascii_lowercase().contains("wal"),
            "error should preserve the WAL failure: {error}"
        );
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

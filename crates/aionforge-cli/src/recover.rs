//! Explicit recovery command execution.

use std::path::Path;
use std::sync::Arc;

use aionforge::{Memory, Store, Timestamp};
use aionforge_config::Config;

use crate::cli::RecoverArgs;
use crate::doctor;
use crate::error::CliError;
use crate::host::{HostOptions, load_config, memory_config, runtime_embedder};

#[derive(Debug)]
pub(crate) struct RecoverOutcome {
    pub(crate) ok: bool,
    pub(crate) rendered: String,
}

pub(crate) fn run(options: &HostOptions, args: RecoverArgs) -> Result<RecoverOutcome, CliError> {
    let config = load_config(options)?;
    run_with_config(config, &options.config_path, args)
}

fn run_with_config(
    config: Config,
    config_path: &Path,
    args: RecoverArgs,
) -> Result<RecoverOutcome, CliError> {
    let now = Timestamp::now();
    let wal_path = config.data_dir().join(Store::WAL_FILE_NAME);
    let persistence = doctor::PersistenceProbe::inspect(config.data_dir());
    if !wal_path.is_file() {
        return Err(CliError::RecoverMissingWal {
            data_dir: config.data_dir().to_path_buf(),
            wal_path,
        });
    }
    let store = match Store::recover(config.data_dir(), config.store_config()) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            let rendered = doctor::render_unavailable(
                "recover",
                config_path,
                config.data_dir(),
                &persistence,
                &error.to_string(),
                args.json,
            )?;
            return Ok(RecoverOutcome {
                ok: false,
                rendered,
            });
        }
    };
    let embedder = runtime_embedder(&config)?;
    let memory = Memory::new(store, embedder, memory_config(&config)?, &now)?;
    let report = memory.doctor_report()?;
    let rendered = if args.json {
        doctor::render_json(config_path, config.data_dir(), &persistence, &report)?
    } else {
        doctor::render_human(
            "recover",
            config_path,
            config.data_dir(),
            &persistence,
            &report,
        )?
    };
    Ok(RecoverOutcome {
        ok: report.ok,
        rendered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionforge::StoreConfig;

    #[test]
    fn json_recover_reopens_an_existing_wal_backed_store() {
        let dir = unique_dir("json");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();
        let now: Timestamp = "2026-06-11T08:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid timestamp");

        {
            let store = Store::open_persistent_migrated(&dir, config.store_config(), &now)
                .expect("create durable store");
            drop(store);
        }

        let outcome = run_with_config(
            config,
            Path::new("/tmp/aionforge.toml"),
            RecoverArgs { json: true },
        )
        .expect("recover report");
        let value: serde_json::Value = serde_json::from_str(&outcome.rendered).expect("json");

        assert!(outcome.ok);
        assert_eq!(value["ok"], true);
        assert_eq!(value["store"]["schema"]["ok"], true);
        assert!(
            value["store"]["capacity"]["node_count"]
                .as_u64()
                .expect("node_count is numeric")
                > 0,
            "recovery report reflects replayed logical state"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recover_refuses_to_create_a_missing_store() {
        let dir = unique_dir("missing");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();

        let error = run_with_config(
            config,
            Path::new("/tmp/aionforge.toml"),
            RecoverArgs { json: false },
        )
        .expect_err("missing WAL-backed store is refused");

        assert!(
            error.to_string().contains("missing WAL file"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recover_refuses_an_empty_existing_data_dir() {
        let dir = unique_dir("empty");
        create_locked_dir(&dir);
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();

        let error = run_with_config(
            config,
            Path::new("/tmp/aionforge.toml"),
            RecoverArgs { json: false },
        )
        .expect_err("an empty directory is not recovered as a fresh store");

        assert!(
            error.to_string().contains("missing WAL file"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn human_recover_report_uses_the_recover_label() {
        let dir = unique_dir("human");
        let mut config = Config::default();
        config.persistence.data_dir = dir.clone();
        config.embedder.enabled = false;
        config.embedder.model.clear();
        config.embedder.endpoint.clear();
        let now: Timestamp = "2026-06-11T08:00:00-05:00[America/Chicago]"
            .parse()
            .expect("valid timestamp");

        {
            let store = Store::open_persistent_migrated(&dir, StoreConfig::default(), &now)
                .expect("create durable store");
            drop(store);
        }

        let outcome = run_with_config(
            config,
            Path::new("/tmp/aionforge.toml"),
            RecoverArgs { json: false },
        )
        .expect("recover report");

        assert!(outcome.ok);
        assert!(outcome.rendered.contains("aionforge recover: ok"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_recover_surfaces_corrupt_wal_without_losing_json() {
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
            RecoverArgs { json: true },
        )
        .expect("recover renders a structured failure");
        let value: serde_json::Value = serde_json::from_str(&outcome.rendered).expect("json");

        assert!(!outcome.ok);
        assert_eq!(value["ok"], false);
        assert_eq!(value["store_open"]["ok"], false);
        assert_eq!(value["store_open"]["mode"], "recover");
        assert_eq!(value["persistence"]["wal"]["present"], true);
        assert!(value["store"].is_null());
        let error = value["store_open"]["error"].as_str().expect("error string");
        assert!(
            error.to_ascii_lowercase().contains("wal"),
            "error should preserve the WAL failure: {error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn unique_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "aionforge-cli-recover-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    fn create_locked_dir(path: &std::path::Path) {
        use std::os::unix::fs::DirBuilderExt;

        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .expect("create locked test dir");
    }

    #[cfg(not(unix))]
    fn create_locked_dir(path: &std::path::Path) {
        std::fs::create_dir_all(path).expect("create test dir");
    }
}

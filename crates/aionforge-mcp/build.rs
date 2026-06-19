//! Bake source/build metadata into the binary for `server_status` build provenance
//! (release-integrity Layer 1).
//!
//! Zero-dependency on purpose: CI/release builds inject the values through environment
//! variables, and local builds shell out to the `git` binary rather than pulling a libgit2 /
//! vergen dependency tree into a tree that guards its dependency surface (store-only-selene,
//! cargo-deny, THIRDPARTY). The emitted vars are read with `option_env!` in `status.rs`, so a
//! build without git (or outside a checkout) degrades to `unknown` instead of failing to
//! compile.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=AIONFORGE_BUILD_SHA");
    println!("cargo:rerun-if-env-changed=AIONFORGE_BUILD_STATUS");
    println!("cargo:rerun-if-env-changed=AIONFORGE_BUILD_TIMESTAMP");
    for path in [".git/HEAD", ".git/index"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let git_sha = git_short_sha();
    let sha = env_non_empty("AIONFORGE_BUILD_SHA")
        .map(shorten_sha)
        .or_else(|| git_sha.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let status = env_non_empty("AIONFORGE_BUILD_STATUS")
        .or_else(|| {
            git_sha.as_ref().map(|_| {
                if working_tree_dirty() {
                    "dirty".to_string()
                } else {
                    "clean".to_string()
                }
            })
        })
        .unwrap_or_else(|| "unknown".to_string());
    let timestamp = env_non_empty("AIONFORGE_BUILD_TIMESTAMP")
        .or_else(git_commit_timestamp)
        .or_else(compile_timestamp)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AIONFORGE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=AIONFORGE_BUILD_STATUS={status}");
    println!("cargo:rustc-env=AIONFORGE_BUILD_TIMESTAMP={timestamp}");
}

fn env_non_empty(key: &str) -> Option<String> {
    env::var(key).ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn shorten_sha(sha: String) -> String {
    if sha == "unknown" {
        return sha;
    }
    sha.chars().take(12).collect()
}

fn command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// The short (12-hex) SHA of `HEAD`, or `None` if git is unavailable or this is not a
/// checkout.
fn git_short_sha() -> Option<String> {
    command_stdout("git", &["rev-parse", "--short=12", "HEAD"])
}

/// The committer timestamp of `HEAD`, or `None` if git is unavailable or this is not a checkout.
fn git_commit_timestamp() -> Option<String> {
    command_stdout("git", &["show", "-s", "--format=%cI", "HEAD"])
}

/// The wall-clock compile timestamp in RFC3339 UTC form, or `None` if the platform lacks `date`.
fn compile_timestamp() -> Option<String> {
    command_stdout("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"])
}

/// Whether tracked content differs from `HEAD` (staged or unstaged). Untracked files are
/// ignored: they do not change the built tracked source, so they must not flip the verdict
/// to `dirty`. `git diff --quiet` exits 1 on a difference, 0 when clean; any other code
/// (e.g. an environment error) is treated as not-dirty rather than implying tamper.
fn working_tree_dirty() -> bool {
    match Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--"])
        .status()
    {
        Ok(status) => status.code() == Some(1),
        Err(_) => false,
    }
}

//! Bake the source git SHA and a clean/dirty flag into the binary for `server_status`
//! build provenance (release-integrity Layer 1).
//!
//! Zero-dependency on purpose: this shells out to the `git` binary rather than pulling a
//! libgit2 / vergen dependency tree into a tree that guards its dependency surface
//! (store-only-selene, cargo-deny, THIRDPARTY). The two emitted vars are read with
//! `option_env!` in `status.rs`, so a build without git (or outside a checkout) degrades to
//! `unknown` instead of failing to compile.
//!
//! No `rerun-if-changed` is emitted, so cargo's default applies: the script re-runs whenever
//! a package source file changes. A clean release build (built fresh from the tagged commit)
//! is therefore authoritative; an incremental dev build is best-effort and advisory.

use std::process::Command;

fn main() {
    let (sha, status) = match git_short_sha() {
        Some(sha) => {
            let status = if working_tree_dirty() {
                "dirty"
            } else {
                "clean"
            };
            (sha, status)
        }
        // Not a checkout / no git on PATH: advertise an honest unknown rather than a
        // fabricated SHA or a misleading clean/dirty verdict.
        None => ("unknown".to_string(), "unknown"),
    };
    println!("cargo:rustc-env=AIONFORGE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=AIONFORGE_BUILD_STATUS={status}");
}

/// The short (12-hex) SHA of `HEAD`, or `None` if git is unavailable or this is not a
/// checkout.
fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
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

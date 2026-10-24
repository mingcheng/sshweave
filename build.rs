//! Records build metadata for `sshweave --version`.
//!
//! Only reproducible inputs are used: the source commit and its date, the
//! compiler, the target, and the profile. Packagers building without a Git
//! checkout can set `SSHWEAVE_BUILD_COMMIT` and `SSHWEAVE_BUILD_DATE`.

use std::{env, fs, path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

/// Reads the commit recorded by `cargo package` when building from a crate.
fn packaged_commit(manifest_dir: &str) -> Option<String> {
    let text = fs::read_to_string(Path::new(manifest_dir).join(".cargo_vcs_info.json")).ok()?;
    let start = text.find("\"sha1\"")?;
    let value = text[start + 6..].split('"').nth(1)?;
    (value.len() >= 12 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value[..12].to_owned())
}

/// Re-runs this script when the checked-out commit changes.
fn watch_git_head() {
    for path in [
        git(&["rev-parse", "--git-path", "HEAD"]),
        git(&["rev-parse", "--git-path", "packed-refs"]),
        git(&["symbolic-ref", "-q", "HEAD"])
            .and_then(|reference| git(&["rev-parse", "--git-path", &reference])),
    ]
    .into_iter()
    .flatten()
    {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SSHWEAVE_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SSHWEAVE_BUILD_DATE");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let in_checkout = git(&["rev-parse", "--show-toplevel"]).is_some_and(|top| {
        let canonical = |path: &str| fs::canonicalize(path).ok();
        canonical(&top).is_some() && canonical(&top) == canonical(&manifest_dir)
    });
    if in_checkout {
        watch_git_head();
    }
    let commit = env::var("SSHWEAVE_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            in_checkout
                .then(|| git(&["rev-parse", "--short=12", "HEAD"]))
                .flatten()
        })
        .or_else(|| packaged_commit(&manifest_dir))
        .unwrap_or_else(|| "unknown".into());
    let date = env::var("SSHWEAVE_BUILD_DATE")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            in_checkout
                .then(|| git(&["log", "-1", "--format=%cs"]))
                .flatten()
        })
        .unwrap_or_else(|| "unknown".into());
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=SSHWEAVE_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=SSHWEAVE_BUILD_DATE={date}");
    println!("cargo:rustc-env=SSHWEAVE_BUILD_RUSTC={rustc}");
    println!("cargo:rustc-env=SSHWEAVE_BUILD_TARGET={target}");
    println!("cargo:rustc-env=SSHWEAVE_BUILD_PROFILE={profile}");
}

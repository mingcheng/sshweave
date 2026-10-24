//! Records build metadata for `sshweave --version`.
//!
//! Only reproducible inputs are used: the source commit and its date, the
//! compiler, the target, and the profile. A commit gets a `-modified` suffix
//! when the sources that build the binary differ from it. Packagers building
//! without a Git checkout can set `SSHWEAVE_BUILD_COMMIT` and
//! `SSHWEAVE_BUILD_DATE`; otherwise `SOURCE_DATE_EPOCH` supplies the date.

use std::{env, fs, path::Path, process::Command};

/// Files and directories compiled into the binary. They are watched for
/// changes and compared with the commit for the `-modified` suffix.
const BUILD_INPUTS: &[&str] = &["src", "build.rs", "Cargo.toml", "Cargo.lock"];

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (output.status.success() && !text.is_empty()).then(|| text.to_owned())
}

/// Reads the commit recorded by `cargo package` when building from a crate,
/// including whether it was packaged from a modified working tree.
fn packaged_commit(manifest_dir: &str) -> Option<String> {
    let text = fs::read_to_string(Path::new(manifest_dir).join(".cargo_vcs_info.json")).ok()?;
    let start = text.find("\"sha1\"")?;
    let value = text[start + 6..].split('"').nth(1)?;
    let commit = (value.len() >= 12 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value[..12].to_owned())?;
    let dirty = text.find("\"dirty\"").is_some_and(|start| {
        text[start + 7..]
            .trim_start_matches([' ', ':'])
            .starts_with("true")
    });
    Some(if dirty {
        format!("{commit}-modified")
    } else {
        commit
    })
}

/// Formats a Unix timestamp as a UTC `YYYY-MM-DD` date.
fn civil_date(seconds: i64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let days = seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Re-runs this script when the checked-out commit changes.
fn watch_git_head() {
    for path in BUILD_INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in [
        git(&["rev-parse", "--git-path", "index"]),
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
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
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
            let commit = git(&["rev-parse", "--short=12", "HEAD"]).filter(|_| in_checkout)?;
            let mut status = vec!["status", "--porcelain", "--untracked-files=no", "--"];
            status.extend(BUILD_INPUTS);
            Some(match git(&status) {
                Some(_) => format!("{commit}-modified"),
                None => commit,
            })
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
        .or_else(|| {
            env::var("SOURCE_DATE_EPOCH")
                .ok()
                .and_then(|value| value.trim().parse::<i64>().ok())
                .map(civil_date)
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

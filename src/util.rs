//! Small, dependency-free helpers shared by the other modules: strict
//! environment and path expansion, input validation, unit parsing, and
//! private-directory creation.

use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// Matches `${NAME}` references; bare `$NAME` is intentionally left untouched.
static ENV_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid environment pattern")
});

/// Matches byte rates such as `512kb`, `10 MiB/s`, or `1.5gb`.
static RATE_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^\s*([0-9]+(?:\.[0-9]+)?)\s*(b|kb|mb|gb|kib|mib|gib)?(?:/s)?\s*$")
        .expect("valid byte rate pattern")
});

/// Upper bound accepted by [`rate_bytes`] (1 TB/s).
const MAX_RATE_BYTES: f64 = 1_000_000_000_000.0;

/// Expands every `${NAME}` reference in `input`.
///
/// Unlike a shell, an unset variable is an error rather than an empty string,
/// so typos cannot silently produce wrong hostnames or paths.
pub fn expand_env(input: &str) -> Result<String> {
    let mut result = String::with_capacity(input.len());
    let mut end = 0;
    for captures in ENV_PATTERN.captures_iter(input) {
        let found = captures.get(0).context("missing environment match")?;
        result.push_str(&input[end..found.start()]);
        let name = &captures[1];
        result.push_str(
            &std::env::var(name)
                .with_context(|| format!("environment variable {name} is not set"))?,
        );
        end = found.end();
    }
    result.push_str(&input[end..]);
    Ok(result)
}

/// Returns the variable names referenced as `${NAME}` in `input`.
pub fn env_references(input: &str) -> impl Iterator<Item = String> + '_ {
    ENV_PATTERN
        .captures_iter(input)
        .map(|captures| captures[1].to_owned())
}

/// Removes every `${NAME}` reference from `input`.
pub fn strip_env_references(input: &str) -> String {
    ENV_PATTERN.replace_all(input, "").into_owned()
}

/// Formats bytes as lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Expands environment variables and a leading `~`, then resolves relative
/// paths against `base`.
///
/// `~otheruser` forms are rejected because they require a password database
/// lookup that OpenSSH and sshweave would not necessarily agree on.
pub fn expand_path(input: &str, base: &Path) -> Result<PathBuf> {
    let value = expand_env(input)?;
    let value = if value == "~" || value.starts_with("~/") {
        let home = std::env::var("HOME").context("HOME is not set")?;
        format!("{home}{}", &value[1..])
    } else {
        ensure!(
            !value.starts_with('~'),
            "~otheruser paths are not supported"
        );
        value
    };
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        base.join(path)
    })
}

/// Validates a connection target such as `db` or an explicit chain `db/bastion`.
///
/// Components must be non-empty, must not look like command-line options, and
/// may only contain characters that are safe to pass to OpenSSH unquoted.
pub fn valid_target(input: &str) -> Result<()> {
    ensure!(
        !input.is_empty() && input.len() <= 1024,
        "invalid target length"
    );
    for part in input.split('/') {
        ensure!(
            !part.is_empty() && !part.starts_with('-'),
            "invalid target component: {part:?}"
        );
        ensure!(
            part.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-:[]".contains(c)),
            "target contains unsupported characters: {part:?}"
        );
    }
    Ok(())
}

/// Rejects values that could inject additional lines into generated files.
pub fn single_line(value: &str) -> Result<()> {
    ensure!(
        !value.contains(['\n', '\r', '\0']),
        "configuration values must be single-line"
    );
    Ok(())
}

/// Parses a human-readable byte rate (for example `10 MiB/s`) into bytes per second.
///
/// Decimal (`kb`, `mb`, `gb`) and binary (`kib`, `mib`, `gib`) units are
/// supported; a bare number means bytes. The result must be between 1 byte
/// and 1 TB per second.
pub fn rate_bytes(input: &str) -> Result<u64> {
    let cap = RATE_PATTERN
        .captures(input)
        .with_context(|| format!("invalid byte rate: {input}"))?;
    let value: f64 = cap[1].parse()?;
    let unit = cap.get(2).map_or("b", |m| m.as_str()).to_ascii_lowercase();
    let multiplier: f64 = match unit.as_str() {
        "b" => 1.0,
        "kb" => 1_000.0,
        "mb" => 1_000_000.0,
        "gb" => 1_000_000_000.0,
        "kib" => 1024.0,
        "mib" => 1_048_576.0,
        "gib" => 1_073_741_824.0,
        _ => bail!("invalid rate unit: {unit}"),
    };
    let bytes = value * multiplier;
    ensure!(
        (1.0..=MAX_RATE_BYTES).contains(&bytes),
        "byte rate out of range"
    );
    // The range check above guarantees a positive value that fits in u64.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(bytes as u64)
}

/// Formats `host:port`, bracketing IPv6 literals as `[host]:port`.
pub fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Creates `path` and any missing parents; new directories are owner-only (0700).
pub fn create_private_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    };
    #[cfg(not(unix))]
    let result = std::fs::create_dir_all(path);
    result.with_context(|| format!("create directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_brackets_ipv6_literals() {
        assert_eq!(authority("db.internal", 22), "db.internal:22");
        assert_eq!(authority("10.0.0.1", 2222), "10.0.0.1:2222");
        assert_eq!(authority("::1", 22), "[::1]:22");
    }

    #[test]
    fn rate_bytes_accepts_decimal_and_binary_units() -> Result<()> {
        assert_eq!(rate_bytes("512")?, 512);
        assert_eq!(rate_bytes("1.5 kb/s")?, 1_500);
        assert_eq!(rate_bytes("10 MiB/s")?, 10 * 1_048_576);
        assert!(rate_bytes("0").is_err());
        assert!(rate_bytes("2000 gb").is_err());
        assert!(rate_bytes("fast").is_err());
        Ok(())
    }
}

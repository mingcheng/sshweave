use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

static ENV_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid environment pattern")
});
static RATE_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^\s*([0-9]+(?:\.[0-9]+)?)\s*(b|kb|mb|gb|kib|mib|gib)?(?:/s)?\s*$")
        .expect("valid byte rate pattern")
});

pub fn expand_env(input: &str) -> Result<String> {
    let mut result = String::new();
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

pub fn single_line(value: &str) -> Result<()> {
    ensure!(
        !value.contains(['\n', '\r', '\0']),
        "configuration values must be single-line"
    );
    Ok(())
}

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
        (1.0..=1_000_000_000_000.0).contains(&bytes),
        "byte rate out of range"
    );
    Ok(bytes as u64)
}

//! Persistent state: the registry of materialized targets and the generated
//! OpenSSH configuration, both updated under file locks with atomic replacement.

use crate::{
    config::{Config, Event},
    hooks, render, util,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
};

/// Returns the parent directory of `path`, creating it (0700) if necessary.
fn ensure_parent(path: &Path) -> Result<&Path> {
    let parent = path.parent().context("path has no parent")?;
    util::create_private_dir(parent)?;
    Ok(parent)
}

/// Opens (creating 0600 if needed) and exclusively locks `path`.
/// The lock is released when the returned file is dropped.
fn lock(path: &Path) -> Result<File> {
    ensure_parent(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("lock {}", path.display()))?;
    file.lock_exclusive()?;
    Ok(file)
}

/// Replaces `path` atomically: write a 0600 temporary file in the same
/// directory, fsync it, rename it over `path`, then fsync the directory.
pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = ensure_parent(path)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Reads the registry of materialized targets; a missing file is empty.
pub fn known_hosts(config: &Config) -> Result<BTreeSet<String>> {
    let path = config.known_hosts()?;
    match fs::read_to_string(&path) {
        Ok(text) => {
            let hosts: BTreeSet<String> =
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            for host in &hosts {
                util::valid_target(host)?;
            }
            Ok(hosts)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Adds `targets` and every gateway they route through to the registry.
///
/// The complete generated target set is validated under the registry lock
/// before committing, so an overflowing update leaves the old registry intact.
pub fn remember(config: &Config, targets: &[String]) -> Result<()> {
    let resolver = config.resolver()?;
    let path = config.known_hosts()?;
    let _guard = lock(&path.with_extension("json.lock"))?;
    let mut known = known_hosts(config)?;
    let original = known.len();
    known.extend(resolver.route_targets(targets.iter().cloned())?);
    if known.len() != original {
        let mut all = config.concrete_names();
        all.extend(known.iter().cloned());
        resolver.route_targets(all).context(
            "registry update would make the generated target set invalid; \
             inspect `sshweave cache list` and remove stale targets with `sshweave cache remove`",
        )?;
        atomic_write(&path, &serde_json::to_vec_pretty(&known)?)?;
    }
    Ok(())
}

/// Removes selected registry entries, or all entries when `targets` is `None`.
///
/// Returns the removed names. Unknown targets are errors. A dry run does not
/// create directories or lock files, run hooks, or write any files. Only the
/// registry is changed; the next configuration synchronization updates its output.
pub fn forget(
    config: &Config,
    targets: Option<&[String]>,
    dry_run: bool,
) -> Result<BTreeSet<String>> {
    if let Some(targets) = targets {
        ensure!(!targets.is_empty(), "no cached targets selected");
        for target in targets {
            util::valid_target(target)?;
        }
    }
    let path = config.known_hosts()?;
    let _guard = if dry_run {
        None
    } else {
        Some(lock(&path.with_extension("json.lock"))?)
    };
    let mut known = known_hosts(config)?;
    let removed = match targets {
        Some(targets) => {
            for target in targets {
                ensure!(
                    known.contains(target),
                    "target {target:?} is not registered"
                );
            }
            targets.iter().cloned().collect()
        }
        None => known.clone(),
    };
    if !dry_run && !removed.is_empty() {
        known.retain(|target| !removed.contains(target));
        atomic_write(&path, &serde_json::to_vec_pretty(&known)?)?;
    }
    Ok(removed)
}

/// Regenerates the OpenSSH configuration if it changed.
///
/// Returns `true` when the file was written. Files lacking the sshweave
/// [`render::HEADER`] are never overwritten. Config-write hooks only run when
/// the content actually changes.
pub async fn sync(config: &Config, automatic_rewrite: bool) -> Result<bool> {
    let output = config.output()?;
    let lock_path = output.with_extension("conf.lock");
    let _guard = tokio::task::spawn_blocking(move || lock(&lock_path)).await??;
    let generated = render::ssh_config(config, &known_hosts(config)?, automatic_rewrite)?;
    match fs::read_to_string(&output) {
        Ok(current) => {
            ensure!(
                current.starts_with(render::HEADER),
                "refusing to overwrite unmanaged file {}",
                output.display()
            );
            if current == generated {
                return Ok(false);
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", output.display())),
    }
    let context = json!({"ssh_config_path": output, "source": config.source});
    let defaults = config.inherit_defaults()?;
    hooks::invoke(&defaults, Event::BeforeConfigWrite, &context).await?;
    atomic_write(&output, generated.as_bytes())?;
    hooks::invoke(&defaults, Event::AfterConfigWrite, &context).await?;
    Ok(true)
}

//! Registry capacity, concurrent updates, and explicit cache recovery.

mod common;

use anyhow::Result;
use sshweave::{config::Config, render, resolver::MAX_ROUTE_TARGETS, store};
use std::{
    collections::BTreeSet,
    fs,
    process::{Command, Output},
    sync::{Arc, Barrier},
};
use tempfile::TempDir;

fn fixture(text: &str) -> Result<(TempDir, Config)> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("config.toml");
    fs::write(
        &source,
        format!("[settings]\noutput='generated.conf'\nknown_hosts='known.json'\n{text}"),
    )?;
    let config = common::load(source)?;
    Ok((directory, config))
}

fn seed(config: &Config, count: usize) -> Result<Vec<u8>> {
    let targets: BTreeSet<_> = (0..count).map(|index| format!("node-{index}")).collect();
    let bytes = serde_json::to_vec(&targets)?;
    fs::write(config.known_hosts()?, &bytes)?;
    Ok(bytes)
}

fn cli(config: &Config, args: &[&str]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_sshweave"))
        .env("HOME", common::isolated_home())
        .arg("-c")
        .arg(&config.source)
        .args(args)
        .output()?)
}

#[test]
fn overflowing_updates_preserve_registry_and_count_configured_routes() -> Result<()> {
    let (_directory, config) = fixture("[hosts.fixed]\naliases=['alias']\ngateways=['gateway']\n")?;
    let original = seed(&config, MAX_ROUTE_TARGETS - 3)?;
    let error = store::remember(&config, &["extra".into()]).unwrap_err();
    assert!(format!("{error:#}").contains("too many expanded gateway targets"));
    assert_eq!(fs::read(config.known_hosts()?)?, original);
    assert!(
        render::ssh_config(&config, &store::known_hosts(&config)?, true)?
            .starts_with(render::HEADER)
    );
    store::forget(&config, Some(&["node-0".into()]), false)?;
    store::remember(&config, &["extra".into()])?;
    let known = store::known_hosts(&config)?;
    assert!(known.contains("extra"));
    assert!(!known.contains("node-0"));
    Ok(())
}

#[test]
fn concurrent_updates_cannot_overfill_the_registry() -> Result<()> {
    let (_directory, config) = fixture("")?;
    seed(&config, MAX_ROUTE_TARGETS - 1)?;
    let config = Arc::new(config);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|target| {
            let config = Arc::clone(&config);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store::remember(&config, &[target.into()])
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("registry writer panicked"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let known = store::known_hosts(&config)?;
    assert_eq!(known.len(), MAX_ROUTE_TARGETS);
    assert_ne!(known.contains("first"), known.contains("second"));
    Ok(())
}

#[test]
fn removal_is_exact_atomic_and_dry_run_is_read_only() -> Result<()> {
    let (directory, config) = fixture("")?;
    assert!(store::forget(&config, None, true)?.is_empty());
    assert_eq!(fs::read_dir(directory.path())?.count(), 1);
    store::remember(&config, &["db/gate/outer".into(), "other".into()])?;
    let original = fs::read(config.known_hosts()?)?;
    let selected = ["db/gate/outer".into()];
    assert_eq!(
        store::forget(&config, Some(&selected), true)?,
        BTreeSet::from(selected.clone())
    );
    assert_eq!(fs::read(config.known_hosts()?)?, original);
    for selected in [
        vec!["db/gate/outer".into(), "missing".into()],
        vec!["bad target".into()],
        Vec::new(),
    ] {
        assert!(store::forget(&config, Some(&selected), false).is_err());
        assert_eq!(fs::read(config.known_hosts()?)?, original);
    }
    store::forget(&config, Some(&selected), false)?;
    let remaining = store::known_hosts(&config)?;
    assert!(!remaining.contains("db/gate/outer"));
    assert!(remaining.contains("gate/outer") && remaining.contains("other"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(config.known_hosts()?)?.permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(store::forget(&config, None, false)?, remaining);
    assert!(store::known_hosts(&config)?.is_empty());
    assert!(!config.output()?.exists());
    Ok(())
}

#[test]
fn cache_cli_recovers_an_overfull_registry_without_touching_output() -> Result<()> {
    let (_directory, config) = fixture("")?;
    let original = seed(&config, MAX_ROUTE_TARGETS + 1)?;
    let listed = cli(&config, &["cache", "list", "--json"])?;
    assert!(listed.status.success(), "{listed:?}");
    let listed: Vec<String> = serde_json::from_slice(&listed.stdout)?;
    assert_eq!(listed.len(), MAX_ROUTE_TARGETS + 1);
    let target = format!("node-{MAX_ROUTE_TARGETS}");
    let preview = cli(&config, &["cache", "remove", "--dry-run", &target])?;
    assert!(preview.status.success(), "{preview:?}");
    assert_eq!(preview.stdout, format!("{target}\n").as_bytes());
    assert_eq!(fs::read(config.known_hosts()?)?, original);
    let removed = cli(&config, &["cache", "remove", &target])?;
    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(removed.stdout, preview.stdout);
    let build = cli(&config, &["config", "build", "--write"])?;
    assert!(build.status.success(), "{build:?}");
    let generated = fs::read(config.output()?)?;
    let cleared = cli(&config, &["cache", "clear"])?;
    assert!(cleared.status.success(), "{cleared:?}");
    assert!(store::known_hosts(&config)?.is_empty());
    assert_eq!(fs::read(config.output()?)?, generated);
    let unknown = cli(&config, &["cache", "remove", "missing"])?;
    assert!(!unknown.status.success());
    assert!(String::from_utf8(unknown.stderr)?.contains("is not registered"));
    Ok(())
}

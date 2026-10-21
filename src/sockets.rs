use crate::{
    config::Config,
    resolver::{self, ResolvedHost},
    store, util,
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeSet, path::PathBuf, process::Stdio};
use tokio::process::Command;

pub fn paths(config: &Config) -> Result<Vec<PathBuf>> {
    let tokens = regex::Regex::new(r"%[%CghnpruLldi]")?;
    let cwd = std::env::current_dir()?;
    let mut patterns = BTreeSet::new();
    let mut hosts = Vec::new();
    for name in config.document.hosts.keys() {
        if resolver::is_pattern(name) {
            hosts.push(config.resolve_inner(name)?);
        }
    }
    let mut names = config.concrete_names();
    names.extend(store::known_hosts(config)?);
    for name in config.route_targets(names)? {
        hosts.push(config.resolve(&name)?);
    }
    hosts.push(ResolvedHost {
        name: "*".into(),
        hostname: "*".into(),
        user: "*".into(),
        port: 22,
        pattern: None,
        gateways: Vec::new(),
        gateway_strategy: Default::default(),
        options: config.inherit_defaults()?,
    });
    for host in hosts {
        if let Some(value) = host.options.control_path()? {
            let value = util::expand_path(&value, &cwd)?;
            let value = value.to_str().context("ControlPath is not UTF-8")?;
            let mut pattern = String::new();
            let mut end = 0;
            for token in tokens.find_iter(value) {
                pattern.push_str(&glob::Pattern::escape(&value[end..token.start()]));
                let replacement = match token.as_str() {
                    "%%" => "%".into(),
                    "%n" if !resolver::is_pattern(&host.name) => glob::Pattern::escape(&host.name),
                    "%d" | "%u" | "%l" | "%L" | "%i" | "%g" => {
                        glob::Pattern::escape(&host.expand(token.as_str(), "")?)
                    }
                    _ => "*".into(),
                };
                pattern.push_str(&replacement);
                end = token.end();
            }
            pattern.push_str(&glob::Pattern::escape(&value[end..]));
            patterns.insert(pattern);
        }
    }
    ensure!(
        !patterns.is_empty(),
        "socket management requires ssh.ControlPath"
    );
    let mut sockets = BTreeSet::new();
    for pattern in patterns {
        for path in glob::glob(&pattern)? {
            let path = path?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if path.symlink_metadata()?.file_type().is_socket() {
                    sockets.insert(path);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                anyhow::bail!("control sockets require Unix");
            }
        }
    }
    Ok(sockets.into_iter().collect())
}

pub async fn flush(config: &Config) -> Result<usize> {
    let sockets = paths(config)?;
    let mut failures = Vec::new();
    for path in &sockets {
        let status = Command::new(config.ssh_binary()?)
            .arg("-F")
            .arg(config.output()?)
            .arg("-S")
            .arg(path)
            .args(["-O", "exit", "--", "localhost"])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("close control socket {}", path.display()))?;
        if !status.success() {
            failures.push(path.display().to_string());
        }
    }
    ensure!(
        failures.is_empty(),
        "failed to close sockets: {}",
        failures.join(", ")
    );
    Ok(sockets.len())
}

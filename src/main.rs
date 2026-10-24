//! `sshweave` command-line interface.

#![forbid(unsafe_code)]

mod wrapper;

use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::json;
use sshweave::{
    config::{Config, Host},
    render,
    resolver::is_pattern,
    runtime, sockets, store, topology, util,
};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};
use tokio::{process::Command, time::Instant};
use wrapper::Program;

/// Port numbers accepted on the command line (0 is rejected).
fn port_parser() -> clap::builder::RangedI64ValueParser<u16> {
    clap::value_parser!(u16).range(1..)
}

/// Default root configuration file, shared by `--config` and `--version`.
macro_rules! default_config {
    () => {
        "~/.config/sshweave/sshweave.toml"
    };
}

/// Detailed build report for `--version`; `-V` keeps the short form for scripts.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit:   ",
    env!("SSHWEAVE_BUILD_COMMIT"),
    " (",
    env!("SSHWEAVE_BUILD_DATE"),
    ")\ntarget:   ",
    env!("SSHWEAVE_BUILD_TARGET"),
    " (",
    env!("SSHWEAVE_BUILD_PROFILE"),
    ")\ncompiler: ",
    env!("SSHWEAVE_BUILD_RUSTC"),
    "\nconfig:   ",
    default_config!(),
    " (override with -c or SSHWEAVE_CONFIG)",
    "\nlicense:  ",
    env!("CARGO_PKG_LICENSE"),
    "\nhomepage: ",
    env!("CARGO_PKG_HOMEPAGE"),
);

#[derive(Parser, Debug)]
#[command(
    version,
    long_version = LONG_VERSION,
    about = "Composable SSH configuration and resilient gateway routing"
)]
struct Cli {
    /// Root configuration file.
    #[arg(
        long,
        short = 'c',
        env = "SSHWEAVE_CONFIG",
        default_value = default_config!()
    )]
    config: PathBuf,
    /// Log informational messages.
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Log debug messages.
    #[arg(long, env = "SSHWEAVE_DEBUG")]
    debug: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Inspect, validate, and generate configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Raw byte transport used by OpenSSH ProxyCommand.
    Connect {
        target: String,
        /// Override the destination port (OpenSSH passes %p).
        #[arg(long, short = 'p', value_parser = port_parser())]
        port: Option<u16>,
        /// Override the destination user (OpenSSH passes %r).
        #[arg(long)]
        user: Option<String>,
        /// Do not regenerate the OpenSSH configuration before connecting.
        #[arg(long)]
        no_rewrite: bool,
        /// Print the resolved host and routes without connecting.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild configuration before invoking an SSH-compatible program.
    #[command(disable_help_flag = true)]
    Wrapper {
        #[arg(value_enum)]
        program: Program,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// TCP latency of the first hop on the selected route (not ICMP).
    Ping(PingArgs),
    /// Check OpenSSH integration without connecting.
    Doctor,
    /// Print SSH route topology without connecting or executing commands.
    Topology {
        target: String,
        /// Use ASCII branches instead of Unicode.
        #[arg(long)]
        ascii: bool,
    },
    /// Manage OpenSSH multiplexing sockets.
    Sockets {
        #[command(subcommand)]
        command: SocketCommand,
    },
    /// Inspect or remove materialized targets (not SSH host keys).
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Show version, paths, source files, and configuration statistics.
    Info,
    /// Generate shell completion.
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Render OpenSSH configuration to stdout, or write a managed output file.
    Build {
        /// Write settings.output instead of printing.
        #[arg(long)]
        write: bool,
        /// Generate a ProxyCommand that never regenerates configuration.
        #[arg(long)]
        no_automatic_rewrite: bool,
        /// Omit materialized targets from the registry (stdout only).
        #[arg(long = "ignore-registry", alias = "ignore-known-hosts")]
        ignore_registry: bool,
    },
    /// Validate TOML, inheritance, options, and gateway graphs.
    Check,
    /// Export the combined configuration, or resolved concrete hosts.
    Json {
        /// Export resolved concrete hosts instead of the merged document.
        #[arg(long, short = 'e')]
        expand: bool,
    },
    /// List hosts, aliases, and patterns.
    List {
        /// Print resolved hosts as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Search names, aliases, comments, endpoints, gateways, proxies, and templates.
    Search { query: String },
    /// Export gateway, alias, and inheritance relationships as Graphviz DOT.
    Graphviz,
    /// Resolve a target without executing commands or connecting.
    Resolve { target: String },
    /// Show which host, template, or defaults supplied each effective field.
    Explain { target: String },
}

#[derive(Subcommand, Debug)]
enum CacheCommand {
    /// List registered targets, including gateway-chain components.
    List {
        /// Print a JSON array instead of one name per line.
        #[arg(long)]
        json: bool,
    },
    /// Remove exact target names; regenerate configuration separately.
    Remove {
        #[arg(required = true)]
        targets: Vec<String>,
        /// Preview removals without creating or changing any files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Clear the registry; never touches OpenSSH's known_hosts.
    Clear {
        /// Preview removals without creating or changing any files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove targets that no longer need their own generated stanza.
    Prune {
        /// Preview removals without creating or changing any files.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SocketCommand {
    /// List existing control sockets.
    List,
    /// Ask every control master to exit.
    Flush,
    /// Start background control masters for targets.
    Master {
        #[arg(required = true)]
        targets: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct PingArgs {
    target: String,
    /// Override the port when the probe is the destination itself.
    #[arg(long, short = 'p', value_parser = port_parser())]
    port: Option<u16>,
    /// Number of probes; 0 runs until interrupted.
    #[arg(long, short = 'n', default_value_t = 4)]
    count: u32,
    /// Seconds between probes.
    #[arg(long, short = 'i', default_value_t = 1.0)]
    interval: f64,
    /// Per-probe timeout in seconds; 0 disables it.
    #[arg(long, short = 'W', default_value_t = 1)]
    timeout: u64,
    /// Stop after the first successful probe.
    #[arg(long, short = 'o')]
    once: bool,
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

/// Resolves every concrete and materialized host.
fn expanded(config: &Config) -> Result<serde_json::Value> {
    let resolver = config.resolver()?;
    let mut names = config.concrete_names();
    names.extend(store::known_hosts(config)?);
    let hosts = names
        .iter()
        .map(|name| resolver.resolve(name))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"hosts": hosts, "templates": config.document.templates, "files": config.files}))
}

/// Runs the CLI and returns the process exit code.
async fn run(cli: Cli) -> Result<i32> {
    let Cli {
        config: path,
        command,
        ..
    } = cli;
    // Completions must work without a configuration file, so load lazily.
    let load = || Config::load(&path);
    match command {
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "sshweave", &mut io::stdout());
        }
        Commands::Config { command } => config_command(&load()?, command).await?,
        Commands::Info => info(&load()?)?,
        Commands::Connect {
            target,
            port,
            user,
            no_rewrite,
            dry_run,
        } => {
            let config = load()?;
            if dry_run {
                let mut host = config.resolve(&target)?;
                if let Some(port) = port {
                    host.port = port;
                }
                if let Some(user) = user {
                    host.user = user;
                }
                print_json(&json!({"host": host, "routes": host.routes()}))?;
            } else {
                connect(&config, &target, port, user.as_deref(), no_rewrite).await?;
            }
        }
        Commands::Wrapper { program, args } => return wrapper::run(&load()?, program, args).await,
        Commands::Ping(args) => ping(&load()?, args).await?,
        Commands::Doctor => return doctor(&load()?).await,
        Commands::Topology { target, ascii } => topology::print(&load()?, &target, ascii)?,
        Commands::Sockets { command } => sockets_command(&load()?, command).await?,
        Commands::Cache { command } => cache_command(&load()?, command)?,
    }
    Ok(0)
}

async fn config_command(config: &Config, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Build {
            write,
            no_automatic_rewrite,
            ignore_registry,
        } => {
            ensure!(
                !(write && ignore_registry),
                "--ignore-registry cannot be combined with --write"
            );
            let automatic = config.document.settings.automatic_rewrite && !no_automatic_rewrite;
            if write {
                let output = config.output()?;
                let next = render::ssh_config(config, &store::known_hosts(config)?, automatic)?;
                if let Ok(current) = std::fs::read_to_string(&output)
                    && current.starts_with(render::HEADER)
                    && current != next
                {
                    let old_hosts = host_directives(&current);
                    let new_hosts = host_directives(&next);
                    for line in old_hosts.difference(&new_hosts) {
                        eprintln!("- {line}");
                    }
                    for line in new_hosts.difference(&old_hosts) {
                        eprintln!("+ {line}");
                    }
                }
                let changed = store::sync(config, automatic).await?;
                eprintln!(
                    "{} {}",
                    if changed { "Updated" } else { "Unchanged" },
                    output.display()
                );
                eprintln!("{}", proxy_scope_note(&output)?);
            } else {
                let known = if ignore_registry {
                    BTreeSet::new()
                } else {
                    store::known_hosts(config)?
                };
                io::stdout()
                    .write_all(render::ssh_config(config, &known, automatic)?.as_bytes())?;
            }
        }
        ConfigCommand::Check => println!(
            "Configuration valid ({} hosts, {} templates, {} files)",
            config.document.hosts.len(),
            config.document.templates.len(),
            config.files.len()
        ),
        ConfigCommand::Json { expand: true } | ConfigCommand::List { json: true } => {
            print_json(&expanded(config)?)?;
        }
        ConfigCommand::Json { expand: false } => print_json(config)?,
        ConfigCommand::Resolve { target } => print_json(&config.resolve(&target)?)?,
        ConfigCommand::Explain { target } => explain(config, &target)?,
        ConfigCommand::List { json: false } => {
            let resolver = config.resolver()?;
            for name in config.document.hosts.keys() {
                let host = resolver.entry(name)?;
                println!("{name} -> {}", host.prototype());
                for alias in &host.options.aliases {
                    if is_pattern(alias) {
                        println!("  pattern: {alias}");
                    } else {
                        println!("{alias} -> {name}");
                    }
                }
                if !host.gateways.is_empty() {
                    println!("  gateways: {}", host.gateways.join(", "));
                    println!("  gateway_strategy: {}", host.gateway_strategy.as_str());
                }
                if let Some(proxy) = &host.options.proxy {
                    println!("  proxy: {proxy}");
                }
                if let Some(rate) = &host.options.rate_limit {
                    println!(
                        "  rate_limit: {rate} (combined send and receive, this connection only)"
                    );
                }
            }
        }
        ConfigCommand::Search { query } => {
            let needle = query.to_lowercase();
            let resolver = config.resolver()?;
            for (name, raw) in &config.document.hosts {
                let host = resolver.entry(name)?;
                let proxy_url = host
                    .options
                    .proxy
                    .as_ref()
                    .and_then(|name| config.document.proxies.get(name))
                    .map(|proxy| proxy.url.as_str())
                    .unwrap_or("");
                let haystack = format!(
                    "{name} {} {} {} {} {} {} {} {} {proxy_url}",
                    host.prototype(),
                    host.hostname,
                    host.options.aliases.join(" "),
                    host.options.comment.as_deref().unwrap_or(""),
                    host.options.proxy.as_deref().unwrap_or(""),
                    host.gateways.join(" "),
                    host.gateway_strategy.as_str(),
                    raw.inherits.join(" "),
                );
                if haystack.to_lowercase().contains(&needle) {
                    println!("{name} -> {}", host.prototype());
                }
            }
        }
        ConfigCommand::Graphviz => io::stdout().write_all(render::graphviz(config)?.as_bytes())?,
    }
    Ok(())
}

struct ExplainLayer<'a> {
    label: String,
    host: &'a Host,
}

fn explain(config: &Config, target: &str) -> Result<()> {
    let resolver = config.resolver()?;
    let resolved = resolver.resolve(target)?;
    let matched = resolver.matched_entries(target);
    let mut layers = Vec::new();
    let mut stack = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in &matched {
        explain_walk(
            config,
            &format!("host:{entry}"),
            &mut layers,
            &mut stack,
            &mut seen,
        )?;
    }
    explain_defaults(config, &mut layers, &mut stack, &mut seen)?;
    println!("{} -> {}", resolved.name, resolved.prototype());
    if matched.is_empty() {
        println!("matched: no host entry");
    }
    for entry in &matched {
        println!("matched: host {entry}");
    }
    if target.contains('/') {
        println!(
            "routes: {} (explicit chain overrides configured gateways)",
            resolved.routes().join(", ")
        );
        println!("gateway_strategy: sequential (explicit chain overrides the configured strategy)");
    } else {
        println!("routes: {}", resolved.routes().join(", "));
        println!("gateway_strategy: {}", resolved.gateway_strategy.as_str());
    }
    report_field("hostname", &layers, |host| host.hostname.clone());
    if layers
        .iter()
        .find_map(|layer| layer.host.hostname.as_deref())
        .is_some_and(|hostname| hostname != resolved.hostname)
    {
        println!("  resolved as {}", resolved.hostname);
    }
    report_field("user", &layers, |host| host.user.clone());
    report_field("port", &layers, |host| {
        host.port.map(|port| port.to_string())
    });
    report_field("gateways", &layers, |host| {
        host.gateways.as_ref().map(|items| items.join(", "))
    });
    report_field("gateway_strategy", &layers, |host| {
        host.gateway_strategy
            .map(|strategy| strategy.as_str().to_owned())
    });
    report_field("proxy", &layers, |host| host.proxy.clone());
    report_field("connect_timeout", &layers, |host| {
        host.connect_timeout.map(|seconds| seconds.to_string())
    });
    report_field("rate_limit", &layers, |host| {
        host.rate_limit
            .as_ref()
            .map(|limit| format!("{limit} (combined send and receive, this connection only)"))
    });
    report_field("resolve_command", &layers, |host| {
        host.resolve_command.as_ref().map(|args| args.join(" "))
    });
    report_field("proxy_command", &layers, |host| {
        host.proxy_command.as_ref().map(|args| args.join(" "))
    });
    let mut options = BTreeSet::new();
    for layer in &layers {
        options.extend(layer.host.ssh.keys().cloned());
    }
    for key in options {
        report_field(&format!("ssh.{key}"), &layers, |host| {
            host.ssh_option(&key).map(|value| value.lines().join(", "))
        });
    }
    if !target.contains('/')
        && resolved.gateway_strategy == sshweave::config::GatewayStrategy::Sequential
        && layers
            .iter()
            .all(|layer| layer.host.gateway_strategy.is_none())
    {
        println!("gateway_strategy built-in: sequential");
    }
    if layers.iter().all(|layer| layer.host.user.is_none()) {
        println!("user: {} (built-in $USER)", resolved.user);
    }
    if layers.iter().all(|layer| layer.host.port.is_none()) {
        println!("port: {} (built-in default)", resolved.port);
    }
    if !config.files.is_empty() {
        println!("files:");
        for file in &config.files {
            println!("  {}", file.display());
        }
    }
    Ok(())
}

fn explain_defaults<'a>(
    config: &'a Config,
    layers: &mut Vec<ExplainLayer<'a>>,
    stack: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    let key = "defaults:".to_owned();
    ensure!(
        stack.len() < 64 && !stack.contains(&key),
        "inheritance cycle/depth limit: {} -> {key}",
        stack.join(" -> ")
    );
    if !seen.insert(key.clone()) {
        return Ok(());
    }
    stack.push(key);
    layers.push(ExplainLayer {
        label: "defaults".to_owned(),
        host: &config.document.defaults,
    });
    for parent in &config.document.defaults.inherits {
        explain_walk(config, parent, layers, stack, seen)?;
    }
    stack.pop();
    Ok(())
}

fn explain_walk<'a>(
    config: &'a Config,
    spec: &str,
    layers: &mut Vec<ExplainLayer<'a>>,
    stack: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    let (kind, name) = match spec.split_once(':') {
        Some((kind @ ("host" | "template"), rest)) => (kind, rest),
        _ if config.document.templates.contains_key(spec) => ("template", spec),
        _ => ("host", spec),
    };
    let key = format!("{kind}:{name}");
    ensure!(
        stack.len() < 64 && !stack.contains(&key),
        "inheritance cycle/depth limit: {} -> {key}",
        stack.join(" -> ")
    );
    if !seen.insert(key.clone()) {
        return Ok(());
    }
    let host = if kind == "template" {
        config.document.templates.get(name)
    } else {
        config.document.hosts.get(name)
    }
    .with_context(|| format!("unknown inheritance parent {key}"))?;
    stack.push(key);
    layers.push(ExplainLayer {
        label: format!("{kind} {name}"),
        host,
    });
    for parent in &host.inherits {
        explain_walk(config, parent, layers, stack, seen)?;
    }
    stack.pop();
    Ok(())
}

fn report_field(title: &str, layers: &[ExplainLayer<'_>], value: impl Fn(&Host) -> Option<String>) {
    let mut winner = None;
    let mut hidden = Vec::new();
    for layer in layers {
        if let Some(text) = value(layer.host) {
            if winner.is_none() {
                winner = Some((layer.label.clone(), text));
            } else {
                hidden.push((layer.label.clone(), text));
            }
        }
    }
    let Some((source, text)) = winner else {
        return;
    };
    println!("{title}: {text}");
    println!("  from {source}");
    for (source, text) in hidden {
        println!("  hidden {source}: {text}");
    }
}

fn info(config: &Config) -> Result<()> {
    let output = config.output()?;
    print_json(&json!({
        "version": env!("CARGO_PKG_VERSION"),
        "commit": env!("SSHWEAVE_BUILD_COMMIT"),
        "commit_date": env!("SSHWEAVE_BUILD_DATE"),
        "target": env!("SSHWEAVE_BUILD_TARGET"),
        "profile": env!("SSHWEAVE_BUILD_PROFILE"),
        "compiler": env!("SSHWEAVE_BUILD_RUSTC"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "executable": std::env::current_exe()?,
        "source": config.source,
        "output": &output,
        "registry": config.known_hosts()?,
        "include": include_line(&output)?,
        "git_ssh_command": git_ssh_command(config, &output)?,
        "rewrite": "An Include connection updates configuration after OpenSSH has read it, then asks you to retry. `sshweave wrapper ssh` applies changes first.",
        "files": config.files,
        "hosts": config.document.hosts.len(),
        "templates": config.document.templates.len(),
        "proxies": config.document.proxies.len(),
    }))
}

fn host_directives(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|line| line.starts_with("Host "))
        .map(str::to_owned)
        .collect()
}

fn include_line(output: &std::path::Path) -> Result<String> {
    let path = output.to_str().context("output path is not UTF-8")?;
    Ok(format!("Include {}", shell_words::quote(path)))
}

fn git_ssh_command(config: &Config, output: &std::path::Path) -> Result<String> {
    let ssh = config.ssh_binary()?;
    let ssh = ssh.to_str().context("ssh_binary path is not UTF-8")?;
    let path = output.to_str().context("output path is not UTF-8")?;
    Ok(shell_words::join([ssh, "-F", path]))
}

fn proxy_scope_note(output: &std::path::Path) -> Result<String> {
    Ok(format!(
        "Host * ProxyCommand routes every other destination through sshweave. Put `{}` before host-specific ProxyJump or ProxyCommand blocks.",
        include_line(output)?
    ))
}

/// Reports integration problems. Missing Include is informational: the wrapper
/// workflow does not need it. An Include that follows a Host or Match block is
/// an error, because OpenSSH keeps the first value it sees.
async fn doctor(config: &Config) -> Result<i32> {
    let mut errors = 0u32;
    let output = config.output()?;
    let ssh = config.ssh_binary()?;
    match Command::new(&ssh).arg("-V").output().await {
        Ok(result) if result.status.success() => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let stdout = String::from_utf8_lossy(&result.stdout);
            let version = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            println!("ssh: {version}");
        }
        Ok(result) => {
            errors += 1;
            println!("ssh: {ssh:?} exited with {}", result.status);
        }
        Err(error) => {
            errors += 1;
            println!("ssh: cannot run {ssh:?}: {error}");
        }
    }
    match std::fs::read_to_string(&output) {
        Ok(text) if text.starts_with(render::HEADER) => {
            println!("output: {} (managed)", output.display());
            let binary = config.binary()?;
            if !text.contains(binary.to_string_lossy().as_ref()) {
                errors += 1;
                println!(
                    "proxycommand: generated file does not reference {}",
                    binary.display()
                );
                if config.document.settings.binary.is_none() {
                    println!(
                        "proxycommand: settings.binary is unset, so each sshweave executable rewrites the file with its own path. Set settings.binary to one stable path."
                    );
                }
            }
        }
        Ok(_) => {
            errors += 1;
            println!(
                "output: {} exists but is not a sshweave-managed file",
                output.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "output: {} (not written; run `sshweave config build --write`)",
                output.display()
            );
        }
        Err(error) => return Err(error).context("read generated configuration"),
    }
    println!("{}", proxy_scope_note(&output)?);
    let ssh_config = util::expand_path("~/.ssh/config", &std::env::current_dir()?)?;
    match std::fs::read_to_string(&ssh_config) {
        Ok(text) => match include_placement(&text, &output)? {
            IncludePlacement::BeforeHosts => {
                println!("include: present at the top of {}", ssh_config.display());
            }
            IncludePlacement::AfterHost => {
                errors += 1;
                println!(
                    "include: {} matches {} after a Host or Match block. Move it above those blocks so it wins.",
                    ssh_config.display(),
                    output.display()
                );
            }
            IncludePlacement::Missing => {
                println!(
                    "include: not in {} (optional when using `sshweave wrapper`)",
                    ssh_config.display()
                );
                println!("include: {}", include_line(&output)?);
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "include: {} does not exist (optional when using `sshweave wrapper`)",
                ssh_config.display()
            );
            println!("include: {}", include_line(&output)?);
        }
        Err(error) => return Err(error).context("read ~/.ssh/config"),
    }
    println!("git: GIT_SSH_COMMAND={}", git_ssh_command(config, &output)?);
    println!(
        "rewrite: an Include connection updates configuration after OpenSSH has read it, then asks you to retry. `sshweave wrapper ssh` applies changes first."
    );
    Ok(if errors == 0 { 0 } else { 1 })
}

enum IncludePlacement {
    BeforeHosts,
    AfterHost,
    Missing,
}

fn include_placement(text: &str, output: &std::path::Path) -> Result<IncludePlacement> {
    let mut saw_host = false;
    let mut found = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if include_mentions(trimmed, output)? {
            found = Some(if saw_host {
                IncludePlacement::AfterHost
            } else {
                IncludePlacement::BeforeHosts
            });
            break;
        }
        let keyword = trimmed.split_whitespace().next().unwrap_or("");
        if keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match") {
            saw_host = true;
        }
    }
    Ok(found.unwrap_or(IncludePlacement::Missing))
}

fn include_mentions(line: &str, output: &std::path::Path) -> Result<bool> {
    let words = shell_words::split(line)
        .unwrap_or_else(|_| line.split_whitespace().map(str::to_owned).collect());
    if !words
        .first()
        .is_some_and(|word| word.eq_ignore_ascii_case("include"))
    {
        return Ok(false);
    }
    let output = output
        .canonicalize()
        .unwrap_or_else(|_| output.to_path_buf());
    let ssh_dir = util::expand_path("~/.ssh", std::path::Path::new("."))?;
    for word in words.iter().skip(1) {
        let candidate = if word.starts_with('~') || std::path::Path::new(word).is_absolute() {
            util::expand_path(word, std::path::Path::new("."))?
        } else {
            ssh_dir.join(word)
        };
        let candidate = candidate.canonicalize().unwrap_or(candidate);
        if candidate == output {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `sshweave connect`: refresh the generated configuration if allowed, then relay.
async fn connect(
    config: &Config,
    target: &str,
    port: Option<u16>,
    user: Option<&str>,
    no_rewrite: bool,
) -> Result<()> {
    // Validate the target before it can be written to the registry.
    let resolver = config.resolver()?;
    resolver.resolve(target)?;
    // A nested hop is started by an outer sshweave process that has already
    // registered the whole route and synchronized the generated file.
    let nested = std::env::var_os(runtime::ROUTE_STACK_ENV).is_some();
    if !no_rewrite && !nested && config.document.settings.automatic_rewrite {
        if resolver.requires_materialization(target) {
            store::remember_with(&resolver, &[target.to_owned()])?;
        }
        // OpenSSH has already parsed the old file, so a changed file cannot
        // take effect for this connection.
        ensure!(
            !store::sync_with(&resolver, true).await?,
            "configuration updated after OpenSSH read it; retry SSH, or use `sshweave wrapper ssh` to apply changes before connecting"
        );
    }
    runtime::connect_with(&resolver, target, port, user).await
}

async fn sockets_command(config: &Config, command: SocketCommand) -> Result<()> {
    match command {
        SocketCommand::List => {
            for path in sockets::paths(config)? {
                println!("{}", path.display());
            }
        }
        SocketCommand::Flush => {
            println!("Closed {} control sockets", sockets::flush(config).await?);
        }
        SocketCommand::Master { targets } => {
            let resolver = config.resolver()?;
            for target in &targets {
                resolver.resolve(target)?;
            }
            store::remember_with(&resolver, &targets)?;
            store::sync_with(&resolver, config.document.settings.automatic_rewrite).await?;
            for target in targets {
                let host = resolver.resolve(&target)?;
                runtime::prepare_control_path(&host)?;
                let status = Command::new(config.ssh_binary()?)
                    .arg("-F")
                    .arg(config.output()?)
                    .args(["-M", "-N", "-f", "--"])
                    .arg(&target)
                    .status()
                    .await
                    .context("start master SSH connection")?;
                ensure!(status.success(), "ssh master {target} exited with {status}");
            }
        }
    }
    Ok(())
}

fn cache_command(config: &Config, command: CacheCommand) -> Result<()> {
    let (targets, dry_run) = match command {
        CacheCommand::List { json } => {
            let known = store::known_hosts(config)?;
            if json {
                print_json(&known)?;
            } else {
                for target in known {
                    println!("{target}");
                }
            }
            return Ok(());
        }
        CacheCommand::Remove { targets, dry_run } => (Some(targets), dry_run),
        CacheCommand::Clear { dry_run } => (None, dry_run),
        CacheCommand::Prune { dry_run } => {
            return report_removed(&store::prune(config, dry_run)?, dry_run);
        }
    };
    report_removed(
        &store::forget(config, targets.as_deref(), dry_run)?,
        dry_run,
    )
}

fn report_removed(removed: &BTreeSet<String>, dry_run: bool) -> Result<()> {
    for target in removed {
        println!("{target}");
    }
    eprintln!(
        "{} {} cached targets",
        if dry_run { "Would remove" } else { "Removed" },
        removed.len()
    );
    if !dry_run && !removed.is_empty() {
        eprintln!(
            "Run `sshweave config build --write` or a wrapper command to refresh the generated configuration."
        );
    }
    Ok(())
}

/// `sshweave ping`: time a TCP handshake to the first hop of the selected route.
async fn ping(config: &Config, args: PingArgs) -> Result<()> {
    ensure!(
        args.interval.is_finite() && (0.0..=86400.0).contains(&args.interval),
        "interval must be 0..86400 seconds"
    );
    let mut probe = topology::tcp_probe(config, &args.target)?;
    if let Some(name) = probe.prepare_host.clone() {
        let mut host = config.resolve(&name)?;
        runtime::prepare(&mut host, "").await?;
        probe.hostname = host.hostname;
        probe.port = host.port;
    }
    if probe.honor_port_override
        && let Some(port) = args.port
    {
        probe.port = port;
    }
    println!(
        "PING {} via {} ({}), TCP port {}",
        args.target, probe.via, probe.hostname, probe.port
    );
    let mut attempts = 0u32;
    let mut timings = Vec::new();
    while args.count == 0 || attempts < args.count {
        if attempts > 0 {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs_f64(args.interval)) => {}
                result = tokio::signal::ctrl_c() => { result?; break; }
            }
        }
        let start = Instant::now();
        let connection = tokio::select! {
            result = runtime::dial_with(&probe.hostname, probe.port, args.timeout, &probe.dial) => result,
            result = tokio::signal::ctrl_c() => { result?; break; }
        };
        attempts += 1;
        match connection {
            Ok(stream) => {
                drop(stream);
                let millis = start.elapsed().as_secs_f64() * 1000.0;
                timings.push(millis);
                println!("seq={attempts} time={millis:.3} ms");
                if args.once {
                    break;
                }
            }
            Err(error) => println!("seq={attempts} failed: {error:#}"),
        }
    }
    print_ping_statistics(&args.target, attempts, &timings)?;
    ensure!(!timings.is_empty(), "no TCP probes succeeded");
    Ok(())
}

fn print_ping_statistics(target: &str, attempts: u32, timings: &[f64]) -> Result<()> {
    let received = u32::try_from(timings.len())?;
    let loss = if attempts == 0 {
        0.0
    } else {
        f64::from(attempts - received) * 100.0 / f64::from(attempts)
    };
    println!("--- {target} sshweave ping statistics ---");
    println!("{attempts} transmitted, {received} received, {loss:.1}% loss");
    if !timings.is_empty() {
        println!(
            "min/avg/max = {:.3}/{:.3}/{:.3} ms",
            timings.iter().copied().fold(f64::INFINITY, f64::min),
            timings.iter().sum::<f64>() / f64::from(received),
            timings.iter().copied().fold(0.0, f64::max)
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let level = if cli.debug {
        "debug"
    } else if cli.verbose {
        "info"
    } else {
        "warn"
    };
    // Logs go to stderr: stdout carries the SSH protocol in `connect`.
    if let Err(error) = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()),
        )
        .try_init()
    {
        eprintln!("sshweave: initialize logging: {error}");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("sshweave: initialize runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli));
    // Tokio stdin uses a blocking OS read. A remote EOF must not wait for local stdin.
    runtime.shutdown_background();
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("sshweave: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_version_reports_build_details_and_short_version_stays_plain() {
        let command = Cli::command();
        assert_eq!(
            command.render_version(),
            concat!("sshweave ", env!("CARGO_PKG_VERSION"), "\n")
        );
        let long = command.render_long_version();
        assert!(long.starts_with(concat!("sshweave ", env!("CARGO_PKG_VERSION"), "\n")));
        for label in [
            "commit:",
            "target:",
            "compiler:",
            "config:",
            "license:",
            "homepage:",
        ] {
            assert!(long.contains(label), "missing {label} in {long}");
        }
    }

    #[test]
    fn wrapper_cli_preserves_ssh_flags_and_remote_arguments() -> Result<()> {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["sshweave", "ssh", "db"]).is_err());
        for args in [
            vec!["-V"],
            vec!["-h"],
            vec!["--help"],
            vec![
                "-vvvp",
                "2222",
                "-o",
                "BatchMode=yes",
                "db",
                "echo",
                "-n",
                "a b",
            ],
            vec!["-c", "aes128-ctr", "db"],
        ] {
            let cli = Cli::try_parse_from(
                ["sshweave", "-c", "custom.toml", "-v", "wrapper", "ssh"]
                    .into_iter()
                    .chain(args.iter().copied()),
            )?;
            assert_eq!(cli.config, PathBuf::from("custom.toml"));
            assert!(cli.verbose);
            let Commands::Wrapper {
                program: Program::Ssh,
                args: forwarded,
            } = cli.command
            else {
                anyhow::bail!("expected SSH wrapper");
            };
            assert_eq!(forwarded, args);
        }
        Ok(())
    }
}

//! `sshweave` command-line interface.

#![forbid(unsafe_code)]

mod wrapper;

use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::json;
use sshweave::{config::Config, render, runtime, sockets, store, topology};
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

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Composable SSH configuration and resilient gateway routing"
)]
struct Cli {
    /// Root configuration file.
    #[arg(
        long,
        short = 'c',
        env = "SSHWEAVE_CONFIG",
        default_value = "~/.config/sshweave/sshweave.toml"
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
    /// TCP connection latency probe (not ICMP).
    Ping(PingArgs),
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
        #[arg(long)]
        ignore_known_hosts: bool,
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
    /// Search host names, aliases, comments, and resolved endpoints.
    Search { query: String },
    /// Export gateway, alias, and inheritance relationships as Graphviz DOT.
    Graphviz,
    /// Resolve a target without executing commands or connecting.
    Resolve { target: String },
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
    /// Override the destination port.
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
            ignore_known_hosts,
        } => {
            ensure!(
                !(write && ignore_known_hosts),
                "--ignore-known-hosts cannot be combined with --write"
            );
            let automatic = config.document.settings.automatic_rewrite && !no_automatic_rewrite;
            if write {
                let changed = store::sync(config, automatic).await?;
                eprintln!(
                    "{} {}",
                    if changed { "Updated" } else { "Unchanged" },
                    config.output()?.display()
                );
            } else {
                let known = if ignore_known_hosts {
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
        ConfigCommand::List { json: false } => {
            let resolver = config.resolver()?;
            for name in config.document.hosts.keys() {
                let host = resolver.entry(name)?;
                println!("{name} -> {}", host.prototype());
                if !host.options.aliases.is_empty() {
                    println!("  aliases: {}", host.options.aliases.join(", "));
                }
                if !host.gateways.is_empty() {
                    println!("  gateways: {}", host.gateways.join(", "));
                    println!("  gateway_strategy: {}", host.gateway_strategy.as_str());
                }
                if let Some(proxy) = &host.options.proxy {
                    println!("  proxy: {proxy}");
                }
            }
        }
        ConfigCommand::Search { query } => {
            let needle = query.to_lowercase();
            let resolver = config.resolver()?;
            for (name, raw) in &config.document.hosts {
                let host = resolver.entry(name)?;
                let haystack = format!(
                    "{name} {} {:?} {:?}",
                    host.prototype(),
                    raw.aliases,
                    raw.comment
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

fn info(config: &Config) -> Result<()> {
    print_json(&json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "executable": std::env::current_exe()?,
        "source": config.source,
        "output": config.output()?,
        "known_hosts": config.known_hosts()?,
        "files": config.files,
        "hosts": config.document.hosts.len(),
        "templates": config.document.templates.len(),
        "proxies": config.document.proxies.len(),
    }))
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
    if !no_rewrite && config.document.settings.automatic_rewrite {
        if resolver.requires_materialization(target) {
            store::remember(config, &[target.to_owned()])?;
        }
        // OpenSSH has already parsed the old file, so a changed file cannot
        // take effect for this connection.
        ensure!(
            !store::sync(config, true).await?,
            "configuration updated after OpenSSH read it; retry SSH, or use `sshweave wrapper ssh` to apply changes before connecting"
        );
    }
    runtime::connect(config, target, port, user).await
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
            store::remember(config, &targets)?;
            store::sync(config, config.document.settings.automatic_rewrite).await?;
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
    };
    let removed = store::forget(config, targets.as_deref(), dry_run)?;
    for target in &removed {
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

/// `sshweave ping`: measure TCP connect latency to a direct route.
async fn ping(config: &Config, args: PingArgs) -> Result<()> {
    ensure!(
        args.interval.is_finite() && (0.0..=86400.0).contains(&args.interval),
        "interval must be 0..86400 seconds"
    );
    let mut host = config.resolve(&args.target)?;
    ensure!(
        host.gateways.is_empty()
            && host.options.proxy_command.is_none()
            && host.options.proxy.is_none(),
        "ping supports direct TCP routes only"
    );
    if let Some(port) = args.port {
        host.port = port;
    }
    runtime::prepare(&mut host, "").await?;
    println!(
        "PING {} ({}), TCP port {}",
        args.target, host.hostname, host.port
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
            result = runtime::dial(&host.hostname, host.port, args.timeout) => result,
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

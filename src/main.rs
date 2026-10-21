use anyhow::{Context, Result, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::json;
use sshweave::{config::Config, render, runtime, sockets, store, topology, util};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::PathBuf,
    process::{ExitCode, Stdio},
    time::Duration,
};
use tokio::{process::Command, time::Instant};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Composable SSH configuration and resilient gateway routing"
)]
struct Cli {
    #[arg(
        long,
        short = 'c',
        env = "SSHWEAVE_CONFIG",
        default_value = "~/.config/sshweave/sshweave.toml"
    )]
    config: PathBuf,
    #[arg(long, short = 'v')]
    verbose: bool,
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
        #[arg(long, short = 'p')]
        port: Option<u16>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        no_rewrite: bool,
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
    /// Show version, paths, source files, and configuration statistics.
    Info,
    /// Generate shell completion.
    Completions { shell: clap_complete::Shell },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Render OpenSSH configuration to stdout, or write a managed output file.
    Build {
        #[arg(long)]
        write: bool,
        #[arg(long)]
        no_automatic_rewrite: bool,
        #[arg(long)]
        ignore_known_hosts: bool,
    },
    /// Validate TOML, inheritance, options, and gateway graphs.
    Check,
    /// Export the combined configuration, or resolved concrete hosts.
    Json {
        #[arg(long, short = 'e')]
        expand: bool,
    },
    /// List hosts, aliases, and patterns.
    List {
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

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum Program {
    Ssh,
    Scp,
    Sftp,
    Rsync,
}

#[derive(Subcommand, Debug)]
enum SocketCommand {
    List,
    Flush,
    Master {
        #[arg(required = true)]
        targets: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct PingArgs {
    target: String,
    #[arg(long, short = 'p')]
    port: Option<u16>,
    #[arg(long, short = 'n', default_value_t = 4)]
    count: u32,
    #[arg(long, short = 'i', default_value_t = 1.0)]
    interval: f64,
    #[arg(long, short = 'W', default_value_t = 1)]
    timeout: u64,
    #[arg(long, short = 'o')]
    once: bool,
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn expanded(config: &Config) -> Result<serde_json::Value> {
    let mut names = config.concrete_names();
    names.extend(store::known_hosts(config)?);
    let hosts = names
        .iter()
        .map(|name| config.resolve(name))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"hosts": hosts, "templates": config.document.templates, "files":config.files}))
}

async fn run(cli: Cli) -> Result<i32> {
    if let Commands::Completions { shell } = cli.command {
        clap_complete::generate(shell, &mut Cli::command(), "sshweave", &mut io::stdout());
        return Ok(0);
    }
    let config = Config::load(&cli.config)?;
    match cli.command {
        Commands::Config { command } => match command {
            ConfigCommand::Build {
                write,
                no_automatic_rewrite,
                ignore_known_hosts,
            } => {
                let automatic = config.document.settings.automatic_rewrite && !no_automatic_rewrite;
                ensure!(
                    !(write && ignore_known_hosts),
                    "--ignore-known-hosts cannot be combined with --write"
                );
                if write {
                    let changed = store::sync(&config, automatic).await?;
                    eprintln!(
                        "{} {}",
                        if changed { "Updated" } else { "Unchanged" },
                        config.output()?.display()
                    );
                } else {
                    let known = if ignore_known_hosts {
                        BTreeSet::new()
                    } else {
                        store::known_hosts(&config)?
                    };
                    io::stdout()
                        .write_all(render::ssh_config(&config, &known, automatic)?.as_bytes())?;
                }
            }
            ConfigCommand::Check => println!(
                "Configuration valid ({} hosts, {} templates, {} files)",
                config.document.hosts.len(),
                config.document.templates.len(),
                config.files.len()
            ),
            ConfigCommand::Json { expand } => {
                if expand {
                    print_json(&expanded(&config)?)?;
                } else {
                    print_json(&config)?;
                }
            }
            ConfigCommand::Resolve { target } => print_json(&config.resolve(&target)?)?,
            ConfigCommand::List { json: true } => print_json(&expanded(&config)?)?,
            ConfigCommand::List { json: false } => {
                for name in config.document.hosts.keys() {
                    let host = config.entry(name)?;
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
                for (name, raw) in &config.document.hosts {
                    let host = config.entry(name)?;
                    if format!(
                        "{name} {} {:?} {:?}",
                        host.prototype(),
                        raw.aliases,
                        raw.comment
                    )
                    .to_lowercase()
                    .contains(&needle)
                    {
                        println!("{name} -> {}", host.prototype());
                    }
                }
            }
            ConfigCommand::Graphviz => {
                io::stdout().write_all(render::graphviz(&config)?.as_bytes())?
            }
        },
        Commands::Info => print_json(&json!({
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS, "arch":std::env::consts::ARCH,
            "executable":std::env::current_exe()?, "source":config.source,
            "output":config.output()?, "known_hosts":config.known_hosts()?,
            "files":config.files, "hosts":config.document.hosts.len(), "templates":config.document.templates.len(),
            "proxies":config.document.proxies.len()
        }))?,
        Commands::Connect {
            target,
            port,
            user,
            no_rewrite,
            dry_run,
        } => {
            let host = config.resolve(&target)?;
            if dry_run {
                let mut host = host;
                if let Some(port) = port {
                    ensure!(port > 0, "port must be positive");
                    host.port = port;
                }
                if let Some(user) = user {
                    host.user = user;
                }
                print_json(&json!({"host":host, "routes":host.routes()}))?;
            } else {
                if !no_rewrite && config.document.settings.automatic_rewrite {
                    if config.requires_materialization(&target)? {
                        store::remember(&config, std::slice::from_ref(&target))?;
                    }
                    if store::sync(&config, true).await? {
                        anyhow::bail!(
                            "configuration updated after OpenSSH read it; retry SSH, or use `sshweave wrapper ssh` to apply changes before connecting"
                        );
                    }
                }
                runtime::connect(&config, &target, port, user.as_deref()).await?;
            }
        }
        Commands::Wrapper { program, args } => return wrapper(&config, program, args).await,
        Commands::Ping(args) => ping(&config, args).await?,
        Commands::Topology { target, ascii } => {
            topology::print(&config, &target, ascii)?;
        }
        Commands::Sockets { command } => match command {
            SocketCommand::List => {
                for path in sockets::paths(&config)? {
                    println!("{}", path.display());
                }
            }
            SocketCommand::Flush => {
                println!("Closed {} control sockets", sockets::flush(&config).await?)
            }
            SocketCommand::Master { targets } => {
                for target in &targets {
                    config.resolve(target)?;
                }
                store::remember(&config, &targets)?;
                store::sync(&config, config.document.settings.automatic_rewrite).await?;
                for target in targets {
                    let host = config.resolve(&target)?;
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
        },
        Commands::Completions { .. } => unreachable!(),
    }
    Ok(0)
}

/// Locate destinations without interpreting or modifying the remote command.
fn wrapper_targets(program: Program, args: &[String]) -> Result<Vec<String>> {
    let mut positionals = Vec::new();
    let mut skip = false;
    let mut pending_option = None;
    let mut options = true;
    let takes_value = match program {
        Program::Ssh => "BbcDEeFIiJLlmOopQRSWw",
        Program::Scp => "cDFiJloPSX",
        Program::Sftp => "BbcDFiJloPRSs",
        Program::Rsync => "BefMT@",
    };
    for arg in args {
        if skip {
            if pending_option == Some('o') {
                check_ssh_override(arg)?;
            }
            skip = false;
            pending_option = None;
            continue;
        }
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg.starts_with('-') && arg != "-" {
            if matches!(program, Program::Rsync) && arg.starts_with("--") {
                ensure!(
                    !["-e", "--rsh", "--config"]
                        .iter()
                        .any(|key| arg == key || arg.starts_with(&format!("{key}="))),
                    "wrapper manages rsync remote shell"
                );
                skip = [
                    "--exclude",
                    "--include",
                    "--filter",
                    "--files-from",
                    "--exclude-from",
                    "--include-from",
                    "--log-file",
                    "--out-format",
                    "--chmod",
                    "--chown",
                    "--bwlimit",
                    "--timeout",
                    "--contimeout",
                    "--password-file",
                    "--rsync-path",
                    "--remote-option",
                ]
                .contains(&arg.as_str());
            } else {
                // OpenSSH permits clusters such as -vvvp2222 and -vp 2222.
                let flags = &arg[1..];
                for (offset, flag) in flags.char_indices() {
                    if takes_value.contains(flag) {
                        ensure!(flag != 'F', "wrapper manages the SSH -F configuration");
                        ensure!(
                            !(matches!(program, Program::Rsync) && flag == 'e'),
                            "wrapper manages rsync remote shell"
                        );
                        skip = offset + flag.len_utf8() == flags.len();
                        if flag == 'o' && !skip {
                            check_ssh_override(&flags[offset + 1..])?;
                        }
                        if skip {
                            pending_option = Some(flag);
                        }
                        break;
                    }
                }
            }
            continue;
        }
        positionals.push(arg.as_str());
        if matches!(program, Program::Ssh) {
            break;
        }
    }
    ensure!(!skip, "missing option argument");
    let mut targets = Vec::new();
    for arg in positionals {
        let candidate = destination(program, arg)?;
        if let Some(candidate) = candidate {
            let candidate = candidate.rsplit('@').next().context("missing host")?;
            util::valid_target(candidate)?;
            targets.push(candidate.into());
        }
    }
    Ok(targets)
}

fn check_ssh_override(value: &str) -> Result<()> {
    let key = value.split(['=', ' ', '\t']).next().unwrap_or(value);
    ensure!(
        !["proxycommand", "proxyjump", "include"].contains(&key.to_ascii_lowercase().as_str()),
        "wrapper manages SSH routing; conflicting option {value:?}"
    );
    Ok(())
}

fn destination(program: Program, value: &str) -> Result<Option<&str>> {
    let uri = value.split_once("://");
    let authority = if let Some((scheme, rest)) = uri {
        if !["ssh", "scp", "sftp"].contains(&scheme) {
            return Ok(None);
        }
        rest.split('/').next().context("URI authority is missing")?
    } else {
        value
    };
    let host = authority.rsplit('@').next().context("missing host")?;
    if let Some(bracketed) = host.strip_prefix('[') {
        let (host, tail) = bracketed
            .split_once(']')
            .context("unclosed IPv6 destination")?;
        if uri.is_some() || matches!(program, Program::Ssh | Program::Sftp) || tail.starts_with(':')
        {
            return Ok(Some(host));
        }
        return Ok(None);
    }
    if uri.is_some() {
        return Ok(host.split(':').next());
    }
    if matches!(program, Program::Ssh) {
        return Ok(Some(host));
    }
    if matches!(program, Program::Sftp) {
        return Ok(host.split(':').next());
    }
    Ok(host
        .split_once(':')
        .map(|(host, _)| host)
        .filter(|h| !h.contains('/')))
}

async fn wrapper(config: &Config, program: Program, args: Vec<String>) -> Result<i32> {
    ensure!(!args.is_empty(), "wrapper requires program arguments");
    let targets = wrapper_targets(program, &args)?;
    store::remember(config, &targets)?;
    store::sync(config, config.document.settings.automatic_rewrite).await?;
    for target in &targets {
        let host = config.resolve(target)?;
        runtime::prepare_control_path(&host)?;
    }
    let output = config.output()?;
    let mut command = match program {
        Program::Ssh => Command::new(config.ssh_binary()?),
        Program::Scp => Command::new("scp"),
        Program::Sftp => Command::new("sftp"),
        Program::Rsync => Command::new("rsync"),
    };
    if matches!(program, Program::Rsync) {
        let ssh_binary = config.ssh_binary()?;
        let rsh = shell_words::join([
            ssh_binary
                .to_str()
                .context("ssh_binary path is not UTF-8")?,
            "-F",
            output.to_str().context("output path is not UTF-8")?,
        ]);
        command.args(["-e", &rsh]);
    } else {
        command.arg("-F").arg(output);
    }
    let status = command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("execute wrapped program")?;
    Ok(status.code().unwrap_or(1))
}

async fn ping(config: &Config, args: PingArgs) -> Result<()> {
    ensure!(
        args.interval.is_finite() && args.interval >= 0.0 && args.interval <= 86400.0,
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
        ensure!(port > 0, "port must be positive");
        host.port = port;
    }
    runtime::prepare(&mut host, "").await?;
    println!(
        "PING {} ({}), TCP port {}",
        args.target, host.hostname, host.port
    );
    let mut attempts = 0u32;
    let mut timings = Vec::new();
    loop {
        if args.count != 0 && attempts >= args.count {
            break;
        }
        if attempts > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs_f64(args.interval)) => {}
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
    let loss = if attempts == 0 {
        0.0
    } else {
        (attempts as usize - timings.len()) as f64 * 100.0 / attempts as f64
    };
    println!("--- {} sshweave ping statistics ---", args.target);
    println!(
        "{attempts} transmitted, {} received, {loss:.1}% loss",
        timings.len()
    );
    if !timings.is_empty() {
        println!(
            "min/avg/max = {:.3}/{:.3}/{:.3} ms",
            timings.iter().copied().fold(f64::INFINITY, f64::min),
            timings.iter().sum::<f64>() / timings.len() as f64,
            timings.iter().copied().fold(0.0, f64::max)
        );
    }
    ensure!(!timings.is_empty(), "no TCP probes succeeded");
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

    #[test]
    fn wrapper_preserves_remote_command_and_finds_destination() -> Result<()> {
        let args = ["-vvvp", "2222", "alice@db/gateway", "echo", "remote"].map(str::to_owned);
        assert_eq!(wrapper_targets(Program::Ssh, &args)?, vec!["db/gateway"]);
        Ok(())
    }

    #[test]
    fn wrapper_finds_scp_and_rsync_endpoints() -> Result<()> {
        let args = ["-P", "2222", "file", "alice@db:/tmp/file"].map(str::to_owned);
        assert_eq!(wrapper_targets(Program::Scp, &args)?, vec!["db"]);
        let args = ["--exclude", "ignored:pattern", "source/", "db:/srv/"].map(str::to_owned);
        assert_eq!(wrapper_targets(Program::Rsync, &args)?, vec!["db"]);
        Ok(())
    }

    #[test]
    fn wrapper_handles_rsync_short_option_arguments() -> Result<()> {
        for args in [
            vec!["-avf", "exclude:ignored", "source/", "db:/srv/"],
            vec!["-fexclude:ignored", "source/", "db:/srv/"],
            vec!["-T", "temp:dir", "source/", "db:/srv/"],
            vec!["-avF", "source/", "db:/srv/"],
        ] {
            assert_eq!(
                wrapper_targets(
                    Program::Rsync,
                    &args.into_iter().map(str::to_owned).collect::<Vec<_>>()
                )?,
                vec!["db"]
            );
        }
        for option in ["-e", "-essh", "-avessh", "--rsh=ssh", "--config=other"] {
            let args = [option, "source/", "db:/srv/"].map(str::to_owned);
            assert!(wrapper_targets(Program::Rsync, &args).is_err(), "{option}");
        }
        let args = ["-avf"].map(str::to_owned);
        assert!(wrapper_targets(Program::Rsync, &args).is_err());
        Ok(())
    }

    #[test]
    fn wrapper_handles_ipv6_uris_and_rejects_conflicting_config() -> Result<()> {
        for (program, input, expected) in [
            (Program::Scp, "alice@[::1]:/tmp", Some("::1")),
            (Program::Ssh, "ssh://alice@[::1]:2222", Some("::1")),
            (Program::Sftp, "sftp://alice@db:2222/path", Some("db")),
            (Program::Scp, "./local:filename", None),
            (Program::Scp, "directory/local:filename", None),
        ] {
            assert_eq!(destination(program, input)?, expected);
        }
        for args in [
            vec!["-vF", "other.conf", "db"],
            vec!["-o", "ProxyCommand=evil", "db"],
        ] {
            assert!(
                wrapper_targets(
                    Program::Ssh,
                    &args.into_iter().map(str::to_owned).collect::<Vec<_>>()
                )
                .is_err()
            );
        }
        Ok(())
    }
}

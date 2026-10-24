//! Read-only route previews rendered as terminal trees.
//!
//! Every candidate route is expanded recursively without connecting, running
//! commands, invoking hooks, or writing files.

use crate::{
    config::{Config, GatewayStrategy},
    proxy,
    resolver::{MAX_GATEWAY_DEPTH, Resolver},
    runtime, util,
};
use anyhow::{Context, Result, ensure};
use ptree::{
    Color, PrintConfig, Style,
    item::StringItem,
    print_config::{ASCII_CHARS_TICK, StyleWhen, UTF_CHARS},
};
use std::io::{self, IsTerminal, Write};

/// Upper bound on visited targets and rendered nodes.
const MAX_NODES: usize = 4096;

fn node(text: String, children: Vec<StringItem>) -> StringItem {
    StringItem { text, children }
}

// Configuration labels must never become terminal control sequences.
fn label(text: &str) -> String {
    text.chars().flat_map(char::escape_debug).collect()
}

/// Accumulates one tree branch per fully expanded route.
struct Planner<'a> {
    config: &'a Config,
    resolver: Resolver<'a>,
    routes: Vec<StringItem>,
    visits: usize,
    nodes: usize,
}

impl Planner<'_> {
    /// Expands every route to `target`.
    ///
    /// `suffix` holds the hops already planned after `target` (closest first),
    /// and `choices` the gateway decisions made so far. A route is recorded
    /// once it reaches a `direct` hop.
    fn visit(
        &mut self,
        target: &str,
        inherited_proxy: Option<&str>,
        stack: &mut Vec<String>,
        suffix: &[String],
        choices: &[String],
    ) -> Result<()> {
        ensure!(
            stack.len() < MAX_GATEWAY_DEPTH && !stack.iter().any(|name| name == target),
            "gateway cycle/depth limit: {} -> {target}",
            stack.join(" -> ")
        );
        self.visits += 1;
        ensure!(
            self.visits <= MAX_NODES,
            "topology expansion exceeds {MAX_NODES} nodes"
        );
        stack.push(target.into());
        let host = self.resolver.resolve(target)?;
        let proxy_name = inherited_proxy.or(host.options.proxy.as_deref());
        let destination = format!(
            "{}  [SSH {}@{}]{}",
            label(target.split('/').next().context("empty target")?),
            label(&host.user),
            label(&util::authority(&host.hostname, host.port)),
            if host.options.resolve_command.is_some() {
                " [resolve_command not executed; endpoint is provisional]"
            } else {
                ""
            }
        );
        for (index, gateway) in host.routes().iter().enumerate() {
            let mut decisions = choices.to_vec();
            if !host.gateways.is_empty() && (!target.contains('/') || choices.is_empty()) {
                let strategy = if target.contains('/') {
                    "explicit chain"
                } else {
                    host.gateway_strategy.as_str()
                };
                decisions.push(format!(
                    "{}: {strategy} #{} via {}",
                    label(target),
                    index + 1,
                    label(gateway)
                ));
            }
            let mut hops = Vec::new();
            if host.options.proxy_command.is_some() {
                hops.push(format!(
                    "proxy_command [{}; not executed]",
                    if gateway == "direct" {
                        "local"
                    } else {
                        "on preceding SSH gateway"
                    }
                ));
            }
            hops.push(destination.clone());
            hops.extend_from_slice(suffix);
            if gateway != "direct" {
                self.visit(gateway, proxy_name, stack, &hops, &decisions)?;
                continue;
            }
            if let Some(name) = proxy_name {
                ensure!(
                    host.options.proxy_command.is_none(),
                    "proxy and proxy_command cannot be combined on the same transport"
                );
                let definition = self
                    .config
                    .document
                    .proxies
                    .get(name)
                    .with_context(|| format!("unknown proxy {name:?}"))?;
                let endpoint = proxy::Endpoint::parse(&definition.url)?;
                let protocol = match endpoint.protocol {
                    proxy::Protocol::Http => "HTTP CONNECT",
                    proxy::Protocol::Socks5 => "SOCKS5",
                };
                hops.insert(
                    0,
                    format!(
                        "{protocol} proxy {}  [{}]",
                        label(name),
                        label(&definition.url)
                    ),
                );
            }
            self.nodes += hops.len() + 1;
            ensure!(
                self.nodes <= MAX_NODES,
                "topology expansion exceeds {MAX_NODES} nodes"
            );
            // Nest hops so the tree reads from the first hop down to the target.
            let mut children = Vec::new();
            for hop in hops.into_iter().rev() {
                children = vec![node(hop, children)];
            }
            let description = if decisions.is_empty() {
                "direct (no SSH gateway)".into()
            } else {
                decisions.join("; ")
            };
            self.routes.push(node(
                format!("Route {} | {description}", self.routes.len() + 1),
                children,
            ));
        }
        stack.pop();
        Ok(())
    }
}

/// Builds the complete topology tree for `target`.
fn tree(config: &Config, target: &str) -> Result<StringItem> {
    let mut planner = Planner {
        config,
        resolver: config.resolver()?,
        routes: Vec::new(),
        visits: 0,
        nodes: 0,
    };
    planner.visit(target, None, &mut Vec::new(), &[], &[])?;
    Ok(node(
        format!("SSH route topology: {} [preview only]", label(target)),
        vec![node("Local (this machine)".into(), planner.routes)],
    ))
}

/// Writes the topology of `target` to `output`, with ANSI styling if `styled`.
pub fn write(
    config: &Config,
    target: &str,
    ascii: bool,
    styled: bool,
    mut output: impl Write,
) -> Result<()> {
    let tree = tree(config, target)?;
    let formatting = PrintConfig {
        characters: if ascii { ASCII_CHARS_TICK } else { UTF_CHARS }.into(),
        indent: 4,
        styled: if styled {
            StyleWhen::Always
        } else {
            StyleWhen::Never
        },
        branch: Style {
            foreground: Some(Color::Cyan),
            ..Style::default()
        },
        leaf: Style {
            bold: true,
            ..Style::default()
        },
        ..PrintConfig::default()
    };
    ptree::write_tree_with(&tree, &mut output, &formatting)?;
    writeln!(
        output,
        "\nRead top to bottom: Local -> proxy/gateways -> target.\n\
         sequential = ordered fallback; random = candidates shown in configuration order, shuffled only when connecting;\n\
         chain = all hops in order. Preview only: no network, commands, hooks, or configuration writes.\n\
         sshweave ping times a TCP handshake to the first hop and does not follow the rest of this preview."
    )?;
    Ok(())
}

/// TCP endpoint measured by `sshweave ping`.
///
/// Planning walks the first configured route only. It does not connect, run
/// commands, or shuffle `random` candidates.
pub struct TcpProbe {
    /// Address passed to `TcpStream::connect`.
    pub hostname: String,
    /// Port passed to `TcpStream::connect`.
    pub port: u16,
    /// Host whose `resolve_command` and control-path setup run before dialing.
    pub prepare_host: Option<String>,
    /// Whether `ping -p` replaces [`TcpProbe::port`]. True only for a direct
    /// destination with no proxy.
    pub honor_port_override: bool,
    /// Short description of the hop, for the probe banner.
    pub via: String,
    /// `AddressFamily`/`BindAddress` of the probed host (defaults for proxies).
    pub dial: runtime::DialOptions,
}

/// Resolves the first TCP hop of `target`.
///
/// A configured proxy is the hop, so the destination is not dialed directly.
/// `proxy_command` has no inferable address and is rejected.
pub fn tcp_probe(config: &Config, target: &str) -> Result<TcpProbe> {
    fn walk(
        resolver: &Resolver<'_>,
        config: &Config,
        target: &str,
        inherited_proxy: Option<&str>,
        stack: &mut Vec<String>,
        random_first: bool,
    ) -> Result<TcpProbe> {
        ensure!(
            stack.len() < MAX_GATEWAY_DEPTH && !stack.iter().any(|name| name == target),
            "gateway cycle/depth limit: {} -> {target}",
            stack.join(" -> ")
        );
        stack.push(target.into());
        let result = (|| {
            let host = resolver.resolve(target)?;
            let proxy_name = inherited_proxy.or(host.options.proxy.as_deref());
            let routes = host.routes();
            let gateway = routes.first().context("host has no route")?;
            let random_first = random_first
                || (host.gateway_strategy == GatewayStrategy::Random && routes.len() > 1);
            if gateway != "direct" {
                return walk(resolver, config, gateway, proxy_name, stack, random_first);
            }
            ensure!(
                host.options.proxy_command.is_none(),
                "ping measures a TCP handshake and does not start proxy_command on {}",
                host.name
            );
            let note = if random_first {
                "; configuration order, not the shuffled connect order"
            } else {
                ""
            };
            if let Some(name) = proxy_name {
                let definition = config
                    .document
                    .proxies
                    .get(name)
                    .with_context(|| format!("unknown proxy {name:?}"))?;
                let endpoint = proxy::Endpoint::parse(&definition.url)?;
                return Ok(TcpProbe {
                    hostname: endpoint.host,
                    port: endpoint.port,
                    prepare_host: None,
                    honor_port_override: false,
                    via: format!("proxy {name} ({}){note}", definition.url),
                    dial: runtime::DialOptions::default(),
                });
            }
            let direct = stack.len() == 1;
            Ok(TcpProbe {
                hostname: host.hostname.clone(),
                port: host.port,
                prepare_host: (host.options.resolve_command.is_some() || direct)
                    .then(|| host.name.clone()),
                honor_port_override: direct,
                via: format!(
                    "{} {}{note}",
                    if direct { "direct" } else { "gateway" },
                    host.name
                ),
                dial: runtime::DialOptions::from_host(&host.options)?,
            })
        })();
        stack.pop();
        result
    }

    walk(
        &config.resolver()?,
        config,
        target,
        None,
        &mut Vec::new(),
        false,
    )
}

/// Prints the topology of `target` to stdout, styling only on capable terminals
/// (honoring `NO_COLOR` and `TERM=dumb`).
pub fn print(config: &Config, target: &str, ascii: bool) -> Result<()> {
    let stdout = io::stdout();
    let styled = stdout.is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
    write(config, target, ascii, styled, stdout.lock())
}

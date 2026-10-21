use crate::{config::Config, proxy};
use anyhow::{Context, Result, ensure};
use ptree::{
    Color, PrintConfig, Style,
    item::StringItem,
    print_config::{ASCII_CHARS_TICK, StyleWhen, UTF_CHARS},
};
use std::io::{self, IsTerminal, Write};

const MAX_NODES: usize = 4096;

fn node(text: String, children: Vec<StringItem>) -> StringItem {
    StringItem { text, children }
}

// Configuration labels must never become terminal control sequences.
fn label(text: &str) -> String {
    text.chars().flat_map(char::escape_debug).collect()
}

struct Planner<'a> {
    config: &'a Config,
    routes: Vec<StringItem>,
    visits: usize,
    nodes: usize,
}

impl Planner<'_> {
    fn visit(
        &mut self,
        target: &str,
        inherited_proxy: Option<&str>,
        stack: &mut Vec<String>,
        suffix: &[String],
        choices: &[String],
    ) -> Result<()> {
        ensure!(
            stack.len() < 32 && !stack.iter().any(|name| name == target),
            "gateway cycle/depth limit: {} -> {target}",
            stack.join(" -> ")
        );
        self.visits += 1;
        ensure!(
            self.visits <= MAX_NODES,
            "topology expansion exceeds {MAX_NODES} nodes"
        );
        stack.push(target.into());
        let host = self.config.resolve(target)?;
        let proxy_name = inherited_proxy.or(host.options.proxy.as_deref());
        let endpoint = if host.hostname.contains(':') {
            format!("[{}]", host.hostname)
        } else {
            host.hostname.clone()
        };
        let destination = format!(
            "{}  [SSH {}@{}:{}]{}",
            label(target.split('/').next().context("empty target")?),
            label(&host.user),
            label(&endpoint),
            host.port,
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

fn tree(config: &Config, target: &str) -> Result<StringItem> {
    let mut planner = Planner {
        config,
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
         chain = all hops in order. Preview only: no network, commands, hooks, or configuration writes."
    )?;
    Ok(())
}

pub fn print(config: &Config, target: &str, ascii: bool) -> Result<()> {
    let stdout = io::stdout();
    let styled = stdout.is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb");
    write(config, target, ascii, styled, stdout.lock())
}

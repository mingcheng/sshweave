//! Host resolution: matching literal names, aliases, globs, and regular
//! expressions, then layering inheritance and defaults into a [`ResolvedHost`].

use crate::{
    config::{Config, GatewayStrategy, Host, Inheritance},
    util,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write,
    sync::LazyLock,
};

/// Maximum depth of nested gateway routes (shared by validation, topology,
/// and the runtime cycle guard).
pub const MAX_GATEWAY_DEPTH: usize = 32;

/// Maximum number of distinct materialized targets: cached names and gateway
/// or chain components that are not configured host names or literal aliases.
/// Configured names are bounded by the configuration itself and never count.
pub const MAX_ROUTE_TARGETS: usize = 4096;

/// Returns whether `name` is a glob or `regex:` pattern rather than a literal.
pub fn is_pattern(name: &str) -> bool {
    name.starts_with("regex:") || name.contains(['*', '?', '['])
}

/// Compiles a `regex:` body anchored to the whole name.
fn compile_regex(expr: &str) -> Result<regex::Regex> {
    Ok(regex::Regex::new(&format!(r"\A(?:{expr})\z"))?)
}

/// Validates a host name, alias, or pattern from the configuration.
pub fn validate_pattern(pattern: &str) -> Result<()> {
    CompiledPattern::new(pattern).map(|_| ())
}

/// Tests whether `name` matches a glob or `regex:` pattern.
pub fn matches(pattern: &str, name: &str) -> Result<bool> {
    if let Some(expr) = pattern.strip_prefix("regex:") {
        Ok(compile_regex(expr)?.is_match(name))
    } else {
        Ok(glob::Pattern::new(pattern)?.matches(name))
    }
}

#[derive(Debug)]
enum CompiledPattern {
    Literal,
    Glob(glob::Pattern),
    Regex(regex::Regex),
}

impl CompiledPattern {
    fn new(pattern: &str) -> Result<Self> {
        ensure!(!pattern.is_empty(), "empty host pattern");
        util::single_line(pattern)?;
        if let Some(expr) = pattern.strip_prefix("regex:") {
            return Ok(Self::Regex(compile_regex(expr).with_context(|| {
                format!("invalid regular expression {pattern:?}")
            })?));
        }
        ensure!(
            !pattern.starts_with('-')
                && !pattern.contains([' ', '\t', '/', '#', '"', '\'', '%', '\\']),
            "invalid host pattern: {pattern:?}"
        );
        let compiled =
            glob::Pattern::new(pattern).with_context(|| format!("invalid glob {pattern:?}"))?;
        Ok(if is_pattern(pattern) {
            Self::Glob(compiled)
        } else {
            Self::Literal
        })
    }

    fn matches(&self, literal: &str, target: &str) -> bool {
        match self {
            Self::Literal => literal == target,
            Self::Glob(pattern) => pattern.matches(target),
            Self::Regex(pattern) => pattern.is_match(target),
        }
    }
}

#[derive(Debug)]
struct Rule<'a> {
    name: &'a str,
    pattern: CompiledPattern,
}

#[derive(Debug)]
struct Entry<'a> {
    name: &'a str,
    options: Host,
    rules: Vec<Rule<'a>>,
    /// The hostname needs regex captures, so literal names are not hosts.
    captures_only: bool,
}

/// An immutable configuration view for repeated host and route lookups.
///
/// Patterns are compiled once, literal aliases are indexed, and inheritance is
/// expanded once per entry. The source configuration remains borrowed, preventing
/// mutation while this view is in use. Construct a new view after editing a
/// [`Config`]; no global or cross-process cache can return stale results.
#[derive(Debug)]
pub struct Resolver<'a> {
    config: &'a Config,
    entries: Vec<Entry<'a>>,
    aliases: HashMap<&'a str, (usize, usize)>,
    patterns: Vec<(usize, usize)>,
    defaults: Host,
}

impl<'a> Resolver<'a> {
    fn new(config: &'a Config) -> Result<Self> {
        let mut inheritance = Inheritance::new(config);
        for name in config.document.templates.keys() {
            inheritance
                .host(&format!("template:{name}"))?
                .validate_gateway_strategy()?;
        }
        let defaults = inheritance.defaults()?;
        defaults.validate_gateway_strategy()?;
        let mut entries = Vec::with_capacity(config.document.hosts.len());
        let mut aliases = HashMap::new();
        let mut all_aliases = HashMap::new();
        let mut patterns = Vec::new();
        for (index, (name, host)) in config.document.hosts.iter().enumerate() {
            let mut rules = Vec::with_capacity(host.aliases.len() + 1);
            for (rule_index, candidate) in std::iter::once(name).chain(&host.aliases).enumerate() {
                let pattern = CompiledPattern::new(candidate)?;
                if rule_index != 0 {
                    if let Some(previous) = all_aliases.insert(candidate, name) {
                        bail!("alias {candidate:?} belongs to both {previous:?} and {name:?}");
                    }
                    ensure!(
                        !config.document.hosts.contains_key(candidate),
                        "alias {candidate:?} conflicts with a host"
                    );
                    if matches!(pattern, CompiledPattern::Literal) {
                        aliases.insert(candidate.as_str(), (index, rule_index));
                    }
                }
                if !matches!(pattern, CompiledPattern::Literal) {
                    patterns.push((index, rule_index));
                }
                rules.push(Rule {
                    name: candidate,
                    pattern,
                });
            }
            let options = inheritance.host(&format!("host:{name}"))?;
            options.validate_gateway_strategy()?;
            let captures_only = match &options.hostname {
                Some(hostname) => validate_captures(name, hostname, &rules)?,
                None => false,
            };
            entries.push(Entry {
                name,
                options,
                rules,
                captures_only,
            });
        }
        Ok(Self {
            config,
            entries,
            aliases,
            patterns,
            defaults,
        })
    }

    /// Returns the defaults with their inheritance applied, before token expansion.
    pub fn defaults(&self) -> &Host {
        &self.defaults
    }

    /// Returns the configuration this view was compiled from.
    pub fn config(&self) -> &'a Config {
        self.config
    }
}

/// A target with every inherited and default value applied.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedHost {
    /// The requested target, including any explicit `/gateway` chain.
    pub name: String,
    /// The `[hosts]` key that matched first, if any.
    pub pattern: Option<String>,
    /// Final network address.
    pub hostname: String,
    /// Final login user.
    pub user: String,
    /// Final SSH port.
    pub port: u16,
    /// Gateway candidates (an explicit chain overrides configured gateways).
    pub gateways: Vec<String>,
    /// How [`ResolvedHost::gateways`] are tried.
    pub gateway_strategy: GatewayStrategy,
    /// The merged options the values above were derived from.
    pub options: Host,
}

impl ResolvedHost {
    /// Deterministic route plan; random ordering is applied only when connecting.
    pub fn routes(&self) -> Vec<String> {
        if self.gateways.is_empty() {
            vec!["direct".into()]
        } else if self.gateway_strategy == GatewayStrategy::Chain {
            vec![
                self.gateways
                    .iter()
                    .rev()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("/"),
            ]
        } else {
            self.gateways.clone()
        }
    }

    /// Expands `${ENV}`, a leading `~`, and OpenSSH-style `%` tokens.
    ///
    /// Supported tokens: `%%`, `%h`, `%n`, `%p`, `%r`, `%g` (current gateway),
    /// `%u`, `%i`, `%d`, `%l`, `%L`, and `%C`. Unknown tokens are kept verbatim.
    pub fn expand(&self, value: &str, gateway: &str) -> Result<String> {
        let local_user = std::env::var("USER").context("USER is not set")?;
        let home = std::env::var("HOME").context("HOME is not set")?;
        let local_host = hostname::get()?
            .into_string()
            .map_err(|_| anyhow::anyhow!("local hostname is not UTF-8"))?;
        let digest = Sha1::digest(
            format!("{local_host}{}{}{}", self.hostname, self.port, self.user).as_bytes(),
        );
        let mut hash = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(hash, "{byte:02x}")?;
        }
        let expanded = util::expand_env(value)?;
        let mut result = String::new();
        let mut chars = expanded.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                result.push(c);
                continue;
            }
            let Some(token) = chars.next() else {
                result.push('%');
                break;
            };
            match token {
                '%' => result.push('%'),
                'h' => result.push_str(&self.hostname),
                'n' => result.push_str(&self.name),
                'p' => result.push_str(&self.port.to_string()),
                'r' => result.push_str(&self.user),
                'g' => result.push_str(gateway),
                'u' => result.push_str(&local_user),
                'i' => result.push_str(&rustix::process::getuid().as_raw().to_string()),
                'd' => result.push_str(&home),
                'l' => result.push_str(&local_host),
                'L' => result.push_str(local_host.split('.').next().unwrap_or(&local_host)),
                'C' => result.push_str(&hash),
                other => {
                    result.push('%');
                    result.push(other);
                }
            }
        }
        if result == "~" || result.starts_with("~/") {
            result = format!("{home}{}", &result[1..]);
        }
        Ok(result)
    }

    /// Returns a compact `user@hostname:port` summary.
    pub fn prototype(&self) -> String {
        format!("{}@{}:{}", self.user, self.hostname, self.port)
    }
}

/// Matches `{0}`, `{1}`, ... and `{name}` capture placeholders.
static CAPTURE_PLACEHOLDER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\{([0-9]+|[A-Za-z_][A-Za-z0-9_]*)\}").expect("valid placeholder pattern")
});

/// Validates capture placeholders in a host's hostname and returns whether
/// there are any.
///
/// Placeholders need at least one `regex:` rule, and every regex rule must
/// define each of them. Glob rules cannot substitute captures. Literal names
/// of such an entry only label the regex rule and are never rendered as hosts.
/// Other braces are rejected because OpenSSH configuration cannot express them.
fn validate_captures(name: &str, hostname: &str, rules: &[Rule<'_>]) -> Result<bool> {
    let hostname = util::strip_env_references(hostname);
    let placeholders: Vec<&str> = CAPTURE_PLACEHOLDER
        .captures_iter(&hostname)
        .filter_map(|captures| captures.get(1).map(|m| m.as_str()))
        .collect();
    ensure!(
        !CAPTURE_PLACEHOLDER
            .replace_all(&hostname, "")
            .contains(['{', '}']),
        "{name}: hostname contains an unmatched brace"
    );
    if placeholders.is_empty() {
        return Ok(false);
    }
    ensure!(
        rules
            .iter()
            .any(|rule| matches!(rule.pattern, CompiledPattern::Regex(_))),
        "{name}: hostname capture placeholders require a regex: host key or alias"
    );
    for rule in rules {
        let pattern = match &rule.pattern {
            CompiledPattern::Regex(pattern) => pattern,
            CompiledPattern::Literal => continue,
            CompiledPattern::Glob(_) => bail!(
                "{name}: glob {:?} cannot fill hostname capture placeholders; use a regex: pattern",
                rule.name
            ),
        };
        for placeholder in &placeholders {
            let defined = match placeholder.parse::<usize>() {
                Ok(index) => index < pattern.captures_len(),
                Err(_) => pattern.capture_names().flatten().any(|n| n == *placeholder),
            };
            ensure!(
                defined,
                "{name}: hostname placeholder {{{placeholder}}} is not a capture group of {:?}",
                rule.name
            );
        }
    }
    Ok(true)
}

/// Replaces `{0}`, `{1}`, ... and `{name}` in `hostname` with regex captures.
fn substitute_captures(hostname: &mut String, re: &regex::Regex, name: &str) {
    let Some(captures) = re.captures(name) else {
        return;
    };
    for (index, capture) in captures.iter().enumerate() {
        if let Some(value) = capture {
            *hostname = hostname.replace(&format!("{{{index}}}"), value.as_str());
        }
    }
    for key in re.capture_names().flatten() {
        if let Some(value) = captures.name(key) {
            *hostname = hostname.replace(&format!("{{{key}}}"), value.as_str());
        }
    }
}

impl<'a> Resolver<'a> {
    /// Returns whether `target` needs its own generated stanza: explicit
    /// chains and names matched only through patterns.
    pub fn requires_materialization(&self, target: &str) -> bool {
        if target.contains('/') {
            return true;
        }
        self.patterns.iter().any(|&(entry, rule)| {
            let rule = &self.entries[entry].rules[rule];
            rule.pattern.matches(rule.name, target)
        })
    }

    /// Resolves a `[hosts]` key exactly as written, including pattern keys.
    pub fn entry(&self, name: &str) -> Result<ResolvedHost> {
        ensure!(
            self.config.document.hosts.contains_key(name),
            "unknown host entry {name:?}"
        );
        self.resolve_inner(name)
    }

    /// Validates and resolves a user-supplied target.
    pub fn resolve(&self, target: &str) -> Result<ResolvedHost> {
        util::valid_target(target)?;
        let host = self.resolve_inner(target)?;
        ensure!(
            !host.hostname.contains(['{', '}']),
            "{target}: hostname {:?} has unfilled capture placeholders; \
             use a name matching the entry's regex: pattern",
            host.hostname
        );
        Ok(host)
    }

    /// Returns matching entries in layering order: the exact host key, then a
    /// literal alias, then patterns in configuration order.
    fn select(&self, name: &str) -> Vec<(usize, &Rule<'_>)> {
        let mut selected: Vec<(usize, &Rule<'_>)> = Vec::new();
        if let Some(index) = self.config.document.hosts.get_index_of(name) {
            selected.push((index, &self.entries[index].rules[0]));
        }
        if let Some(&(entry, rule)) = self.aliases.get(name) {
            selected.push((entry, &self.entries[entry].rules[rule]));
        }
        for &(entry, rule) in &self.patterns {
            if selected.iter().any(|(index, _)| *index == entry) {
                continue;
            }
            let rule = &self.entries[entry].rules[rule];
            if rule.pattern.matches(rule.name, name) {
                selected.push((entry, rule));
            }
        }
        selected
    }

    /// Returns the `[hosts]` keys layered into `target`, highest priority first.
    ///
    /// An explicit `/gateway` chain is ignored; only the destination name matches.
    pub fn matched_entries(&self, target: &str) -> Vec<&'a str> {
        let name = target.split('/').next().unwrap_or(target);
        self.select(name)
            .into_iter()
            .map(|(entry, _)| self.entries[entry].name)
            .collect()
    }

    /// Resolves without target validation (pattern keys are not valid targets).
    ///
    /// Matching entries are layered in priority order: the exact host key,
    /// then literal aliases, then patterns in configuration order.
    pub(crate) fn resolve_inner(&self, target: &str) -> Result<ResolvedHost> {
        let mut parts = target.splitn(2, '/');
        let name = parts.next().context("empty target")?;
        let chain = parts.next();
        let selected = self.select(name);
        let mut options = Host::default();
        for (index, (entry, rule)) in selected.iter().enumerate() {
            let mut incoming = self.entries[*entry].options.clone();
            if let Some(hostname) = &mut incoming.hostname
                && let CompiledPattern::Regex(pattern) = &rule.pattern
            {
                substitute_captures(hostname, pattern, name);
            }
            if index == 0 {
                options = incoming;
            } else {
                options.fallback(&incoming);
            }
        }
        options.fallback(&self.defaults);
        let canonical = selected.first().map(|(entry, _)| self.entries[*entry].name);
        // Literal aliases use the canonical hostname; patterns use the requested name.
        let default_hostname = canonical.filter(|p| !is_pattern(p)).unwrap_or(name);
        let hostname = options
            .hostname
            .clone()
            .unwrap_or_else(|| default_hostname.into());
        let hostname = hostname.replace("%n", name).replace("%h", name);
        let hostname = util::expand_env(&hostname)?;
        util::single_line(&hostname)?;

        ensure!(
            !hostname.is_empty()
                && !hostname.chars().any(char::is_whitespace)
                && !hostname.starts_with('-'),
            "invalid resolved hostname {hostname:?}"
        );
        let user = match &options.user {
            Some(user) => util::expand_env(user)?,
            None => std::env::var("USER").context("USER is not set")?,
        };
        ensure!(
            !user.is_empty() && !user.chars().any(char::is_whitespace),
            "invalid user"
        );
        let gateways = chain.map_or_else(
            || options.gateways.clone().unwrap_or_default(),
            |s| vec![s.to_owned()],
        );
        let gateway_strategy = if chain.is_some() {
            GatewayStrategy::Sequential
        } else {
            options.validate_gateway_strategy()?;
            options.gateway_strategy.unwrap_or_default()
        };
        let host = ResolvedHost {
            name: target.into(),
            pattern: canonical.map(str::to_owned),
            hostname,
            user,
            port: options.port.unwrap_or(22),
            gateways,
            gateway_strategy,
            options,
        };
        ensure!(
            host.options.proxy_command.is_none()
                || host.options.proxy.is_none()
                || !host.routes().iter().any(|route| route == "direct"),
            "proxy and proxy_command cannot be combined on the same transport"
        );
        Ok(host)
    }

    /// Rejects gateway cycles and overly deep routes for every configured host.
    pub(crate) fn validate_gateway_graph(&self) -> Result<()> {
        fn visit(
            resolver: &Resolver<'_>,
            target: &str,
            stack: &mut Vec<String>,
            depths: &mut HashMap<String, usize>,
        ) -> Result<usize> {
            ensure!(
                stack.len() < MAX_GATEWAY_DEPTH && !stack.iter().any(|x| x == target),
                "gateway cycle/depth limit: {} -> {target}",
                stack.join(" -> ")
            );
            if let Some(depth) = depths.get(target) {
                ensure!(
                    stack.len() + depth <= MAX_GATEWAY_DEPTH,
                    "gateway cycle/depth limit: {} -> {target}",
                    stack.join(" -> ")
                );
                return Ok(*depth);
            }
            stack.push(target.into());
            let host = resolver.resolve_inner(target)?;
            let mut depth = 1;
            for gateway in host.routes() {
                if gateway != "direct" {
                    depth = depth.max(visit(resolver, &gateway, stack, depths)? + 1);
                }
            }
            stack.pop();
            depths.insert(target.into(), depth);
            Ok(depth)
        }
        let mut depths = HashMap::new();
        for name in self.config.document.hosts.keys() {
            visit(self, name, &mut Vec::new(), &mut depths)?;
        }
        Ok(())
    }

    /// Literal host names and aliases that get their own stanza (patterns and
    /// names that only label a regex rule with capture placeholders excluded).
    pub fn concrete_names(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter(|entry| !entry.captures_only)
            .flat_map(|entry| &entry.rules)
            .filter(|rule| matches!(rule.pattern, CompiledPattern::Literal))
            .map(|rule| rule.name.to_owned())
            .collect()
    }

    /// Returns whether `name` is a literal `[hosts]` key or literal alias.
    pub fn is_configured_name(&self, name: &str) -> bool {
        (!is_pattern(name) && self.config.document.hosts.contains_key(name))
            || self.aliases.contains_key(name)
    }

    /// Expands targets with every gateway and chain component they can route
    /// through, so each one gets a generated stanza.
    ///
    /// At most [`MAX_ROUTE_TARGETS`] of the results may be materialized names;
    /// configured names are not limited.
    pub fn route_targets(
        &self,
        targets: impl IntoIterator<Item = String>,
    ) -> Result<BTreeSet<String>> {
        let mut pending: Vec<_> = targets.into_iter().collect();
        let mut visited = BTreeSet::new();
        let mut materialized = 0usize;
        while let Some(target) = pending.pop() {
            util::valid_target(&target)?;
            if !visited.insert(target.clone()) {
                continue;
            }
            if !self.is_configured_name(&target) {
                materialized += 1;
                ensure!(
                    materialized <= MAX_ROUTE_TARGETS,
                    "too many materialized targets (limit {MAX_ROUTE_TARGETS}); \
                     configured host names and aliases are not counted"
                );
            }
            if let Some((first, rest)) = target.split_once('/') {
                pending.push(first.into());
                pending.push(rest.into());
            }
            pending.extend(
                self.resolve(&target)?
                    .routes()
                    .into_iter()
                    .filter(|g| g != "direct"),
            );
        }
        Ok(visited)
    }
}

impl Config {
    /// Compiles an immutable resolver for a batch of lookups.
    pub fn resolver(&self) -> Result<Resolver<'_>> {
        Resolver::new(self)
    }

    /// Resolves a user-supplied target; use [`Config::resolver`] for repeated lookups.
    pub fn resolve(&self, target: &str) -> Result<ResolvedHost> {
        self.resolver()?.resolve(target)
    }

    /// Resolves a `[hosts]` key exactly as written, including pattern keys.
    pub fn entry(&self, name: &str) -> Result<ResolvedHost> {
        self.resolver()?.entry(name)
    }

    /// Returns whether a target needs a concrete generated stanza.
    pub fn requires_materialization(&self, target: &str) -> Result<bool> {
        Ok(self.resolver()?.requires_materialization(target))
    }

    /// Literal host names and aliases (patterns excluded).
    ///
    /// Names that only label a regex rule with capture placeholders are
    /// excluded as well; see [`Resolver::concrete_names`].
    pub fn concrete_names(&self) -> BTreeSet<String> {
        if let Ok(resolver) = self.resolver() {
            return resolver.concrete_names();
        }
        self.document
            .hosts
            .iter()
            .flat_map(|(name, host)| std::iter::once(name).chain(host.aliases.iter()))
            .filter(|name| !is_pattern(name))
            .cloned()
            .collect()
    }

    /// Expands targets with all reachable gateways and chain components.
    pub fn route_targets(
        &self,
        targets: impl IntoIterator<Item = String>,
    ) -> Result<BTreeSet<String>> {
        self.resolver()?.route_targets(targets)
    }
}

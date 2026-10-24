//! TOML configuration model, include loading, inheritance, and validation.
//!
//! A [`Config`] is built from a root file plus its includes. Every entry is
//! validated eagerly at load time so later stages (resolution, rendering, and
//! transport) can assume well-formed input.

use crate::{hooks::Hook, util};
use anyhow::{Context, Result, ensure};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Component, Path, PathBuf},
};

/// Fragment directory that is always included first, even when not listed.
const DEFAULT_INCLUDE: &str = "${HOME}/.config/sshweave/conf.d/*.toml";

/// Maximum include nesting depth.
const MAX_INCLUDE_DEPTH: usize = 64;

/// Maximum `inherits` chain depth.
const MAX_INHERIT_DEPTH: usize = 64;

/// OpenSSH keywords that must be expressed through typed sshweave fields,
/// because sshweave generates or controls them itself.
const RESERVED_SSH_OPTIONS: &[&str] = &[
    "host",
    "match",
    "include",
    "hostname",
    "user",
    "port",
    "proxycommand",
    "proxyjump",
    "connecttimeout",
];

/// A raw OpenSSH option value as written in TOML.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SshValue {
    /// A verbatim OpenSSH argument string.
    Text(String),
    /// An integer value.
    Integer(i64),
    /// A boolean, rendered as `yes`/`no`.
    Bool(bool),
    /// A repeated option; each element becomes its own line.
    List(Vec<String>),
}

impl SshValue {
    /// Returns one OpenSSH value per emitted configuration line.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Text(s) => vec![s.clone()],
            Self::Integer(n) => vec![n.to_string()],
            Self::Bool(b) => vec![if *b { "yes" } else { "no" }.into()],
            Self::List(v) => v.clone(),
        }
    }
}

/// A host, template, or the `[defaults]` table.
///
/// All scalar fields are optional so that values can be layered by
/// [`Host::fallback`]; resolution fills in final defaults.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Host {
    /// Network address; may contain `%n`/`%h`, `${ENV}`, and regex captures.
    pub hostname: Option<String>,
    /// Remote login user.
    pub user: Option<String>,
    /// Remote SSH port.
    pub port: Option<u16>,
    /// Extra names (literal or pattern) that resolve to this host.
    pub aliases: Vec<String>,
    /// Parents in priority order; `host:` or `template:` prefixes disambiguate.
    pub inherits: Vec<String>,
    /// Candidate SSH gateways (`direct` means no gateway).
    pub gateways: Option<Vec<String>>,
    /// How [`Host::gateways`] are tried.
    pub gateway_strategy: Option<GatewayStrategy>,
    /// Name of an entry in `[proxies]` used for the first network hop.
    pub proxy: Option<String>,
    /// Connection and startup timeout in seconds; `0` disables it.
    pub connect_timeout: Option<u64>,
    /// Command whose stdout replaces the hostname right before connecting.
    pub resolve_command: Option<Vec<String>>,
    /// Custom transport command, run locally or on the preceding gateway.
    pub proxy_command: Option<Vec<String>>,
    /// Shared upload/download rate limit such as `10 MiB/s`.
    pub rate_limit: Option<String>,
    /// Create the parent directory of `ControlPath` before connecting.
    pub control_master_mkdir: Option<bool>,
    /// Free-form note emitted as a comment in the generated configuration.
    pub comment: Option<String>,
    /// Hooks keyed by lifecycle event.
    pub hooks: BTreeMap<Event, Vec<Hook>>,
    /// Additional raw OpenSSH options (keys are case-insensitive).
    pub ssh: IndexMap<String, SshValue>,
}

/// Order in which gateway candidates are attempted.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayStrategy {
    /// Try every candidate in a freshly shuffled order.
    Random,
    /// Try candidates in configuration order. This is the default.
    #[default]
    Sequential,
    /// Traverse all gateways as one multi-hop route, outermost first.
    Chain,
}

impl GatewayStrategy {
    /// Returns the configuration spelling of the strategy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Sequential => "sequential",
            Self::Chain => "chain",
        }
    }
}

/// Lifecycle events that can trigger hooks.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// Before each route attempt.
    BeforeConnect,
    /// After a route has been established.
    OnConnect,
    /// After a route attempt has failed.
    OnConnectError,
    /// After an established connection has closed.
    OnDisconnect,
    /// Before the generated OpenSSH configuration is replaced.
    BeforeConfigWrite,
    /// After the generated OpenSSH configuration has been replaced.
    AfterConfigWrite,
}

impl Host {
    /// Fills only missing values from `other`.
    ///
    /// Repeated application yields the precedence
    /// child > first parent > later parents > defaults.
    pub fn fallback(&mut self, other: &Self) {
        macro_rules! fill {
            ($($field:ident),*) => {$(if self.$field.is_none() { self.$field = other.$field.clone(); })*};
        }
        fill!(
            hostname,
            user,
            port,
            gateways,
            gateway_strategy,
            proxy,
            connect_timeout,
            resolve_command,
            proxy_command,
            rate_limit,
            control_master_mkdir,
            comment
        );
        for (key, value) in &other.ssh {
            if self.ssh_option(key).is_none() {
                self.ssh.insert(key.clone(), value.clone());
            }
        }
        for (event, hooks) in &other.hooks {
            self.hooks.entry(*event).or_insert_with(|| hooks.clone());
        }
    }

    /// Merges a same-name entry from an earlier file into this later one.
    ///
    /// Fields are merged like [`Host::fallback`]; `aliases` and `inherits`
    /// are also kept from `earlier` when this entry leaves them empty.
    fn merge_earlier(&mut self, earlier: &Self) {
        self.fallback(earlier);
        if self.aliases.is_empty() {
            self.aliases.clone_from(&earlier.aliases);
        }
        if self.inherits.is_empty() {
            self.inherits.clone_from(&earlier.inherits);
        }
    }

    /// Looks up a raw OpenSSH option case-insensitively.
    pub fn ssh_option(&self, key: &str) -> Option<&SshValue> {
        self.ssh
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    /// Ensures chain gateways are individual host names.
    pub(crate) fn validate_gateway_strategy(&self) -> Result<()> {
        if self.gateway_strategy == Some(GatewayStrategy::Chain) {
            for gateway in self.gateways.iter().flatten() {
                ensure!(
                    gateway != "direct" && !gateway.contains('/'),
                    "chain gateways must be individual host names, not direct or slash chains: {gateway:?}"
                );
            }
        }
        Ok(())
    }

    /// Returns the unexpanded `ControlPath`, or `None` when unset or `none`.
    pub fn control_path(&self) -> Result<Option<String>> {
        let Some(value) = self.ssh_option("ControlPath") else {
            return Ok(None);
        };
        let lines = value.lines();
        ensure!(lines.len() == 1, "ControlPath requires one value");
        let mut args = shell_words::split(&lines[0]).context("parse ControlPath")?;
        ensure!(
            args.len() == 1,
            "ControlPath must be a single (possibly quoted) path"
        );
        let path = args.remove(0);
        Ok((path != "none").then_some(path))
    }

    /// Validates fields that do not depend on other entries.
    fn validate_fields(&self) -> Result<()> {
        ensure!(self.port != Some(0), "port must be positive");
        ensure!(
            self.connect_timeout
                .is_none_or(|seconds| seconds <= i32::MAX.unsigned_abs().into()),
            "connect_timeout exceeds OpenSSH's supported integer range"
        );
        self.control_path().context("ControlPath")?;
        if let Some(rate) = &self.rate_limit {
            util::rate_bytes(rate)?;
        }
        for (label, command) in [
            ("resolve_command", &self.resolve_command),
            ("proxy_command", &self.proxy_command),
        ] {
            if let Some(args) = command {
                ensure!(
                    args.first().is_some_and(|program| !program.is_empty()),
                    "{label} cannot be empty"
                );
                for arg in args {
                    util::single_line(arg)?;
                }
            }
        }
        for gateway in self.gateways.iter().flatten() {
            util::valid_target(gateway)?;
        }
        self.validate_gateway_strategy()
            .context("gateway_strategy")?;
        self.validate_ssh_options()?;
        crate::runtime::DialOptions::from_host(self)?;
        for hook in self.hooks.values().flatten() {
            hook.validate()?;
        }
        Ok(())
    }

    /// Validates raw OpenSSH options: well-formed, unique, and not reserved.
    fn validate_ssh_options(&self) -> Result<()> {
        let mut keys = BTreeSet::new();
        for (key, value) in &self.ssh {
            ensure!(
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()),
                "invalid SSH option {key:?}"
            );
            let lower = key.to_ascii_lowercase();
            ensure!(
                !RESERVED_SSH_OPTIONS.contains(&lower.as_str()),
                "{key} must be configured through typed sshweave fields (use gateways for ProxyJump)"
            );
            ensure!(keys.insert(lower), "duplicate SSH option {key}");
            for line in value.lines() {
                util::single_line(&line)?;
            }
        }
        Ok(())
    }
}

/// The `[settings]` table; only honored in the root configuration file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Generated OpenSSH configuration file.
    pub output: String,
    /// Registry of dynamically materialized targets (not OpenSSH's known_hosts).
    ///
    /// TOML accepts `registry` and the older `known_hosts` key. The on-disk
    /// default remains `sshweave_known_hosts.json`.
    #[serde(rename = "registry", alias = "known_hosts")]
    pub known_hosts: String,
    /// OpenSSH client used for gateways, wrappers, and control sockets.
    pub ssh_binary: String,
    /// `scp` client used by `sshweave wrapper scp`.
    pub scp_binary: String,
    /// `sftp` client used by `sshweave wrapper sftp`.
    pub sftp_binary: String,
    /// `rsync` client used by `sshweave wrapper rsync`.
    pub rsync_binary: String,
    /// sshweave executable referenced by the generated ProxyCommand.
    pub binary: Option<String>,
    /// Regenerate the configuration automatically on connect.
    pub automatic_rewrite: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output: "~/.config/sshweave/sshweave.conf".into(),
            known_hosts: "~/.config/sshweave/sshweave_known_hosts.json".into(),
            ssh_binary: "ssh".into(),
            scp_binary: "scp".into(),
            sftp_binary: "sftp".into(),
            rsync_binary: "rsync".into(),
            binary: None,
            automatic_rewrite: true,
        }
    }
}

/// One parsed TOML file, or the merged result of all files.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Document {
    /// Glob patterns of additional files, relative to the including file.
    pub includes: Vec<String>,
    /// Global settings.
    pub settings: Settings,
    /// Values inherited by every host.
    pub defaults: Host,
    /// Named HTTP/SOCKS5 proxies.
    pub proxies: IndexMap<String, Proxy>,
    /// Reusable host fragments that are never rendered on their own.
    pub templates: IndexMap<String, Host>,
    /// Concrete hosts and host patterns.
    pub hosts: IndexMap<String, Host>,
}

/// A named network proxy.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Proxy {
    /// `http://host:port` or `socks5://host:port`.
    pub url: String,
}

/// A fully loaded and validated configuration.
#[derive(Clone, Debug, Serialize)]
pub struct Config {
    /// Canonical path of the root configuration file.
    pub source: PathBuf,
    /// Every loaded file in load order.
    pub files: Vec<PathBuf>,
    /// The merged document.
    pub document: Document,
    /// SHA-1 over every loaded file path and its contents, in load order.
    #[serde(skip)]
    source_digest: String,
    /// Environment variables referenced as `${NAME}` by any loaded file.
    #[serde(skip)]
    env_references: BTreeSet<String>,
}

/// Merges `incoming` entries into `existing`; later files take precedence.
fn merge_entries(existing: &mut IndexMap<String, Host>, incoming: IndexMap<String, Host>) {
    for (name, mut host) in incoming {
        if let Some(current) = existing.get_mut(&name) {
            host.merge_earlier(current);
            *current = host;
        } else {
            existing.insert(name, host);
        }
    }
}

/// The default root file is the only one that implicitly loads conf.d.
fn is_default_source(path: &Path) -> Result<bool> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let default = PathBuf::from(home).join(".config/sshweave/sshweave.toml");
    Ok(normalized(default)? == path)
}

/// Canonicalizes `path`, or lexically normalizes it when it does not exist yet.
fn normalized(path: PathBuf) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut result = PathBuf::new();
            for part in path.components() {
                match part {
                    Component::ParentDir => {
                        result.pop();
                    }
                    Component::CurDir => {}
                    _ => result.push(part),
                }
            }
            Ok(result)
        }
        Err(error) => Err(error.into()),
    }
}

impl Config {
    /// Loads, merges, and validates the configuration rooted at `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let source =
            util::expand_path(&path.as_ref().to_string_lossy(), &std::env::current_dir()?)?;
        let source = source
            .canonicalize()
            .with_context(|| format!("open configuration {}", source.display()))?;
        let mut result = Self {
            source: source.clone(),
            files: Vec::new(),
            document: Document::default(),
            source_digest: String::new(),
            env_references: BTreeSet::new(),
        };
        let mut active = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut digest = Sha1::new();
        result.load_file(&source, true, &mut active, &mut seen, &mut digest)?;
        result.source_digest = util::hex(&digest.finalize());
        result.validate()?;
        Ok(result)
    }

    /// Loads one file depth-first. Includes are merged before the including
    /// file, so the including file overrides them.
    fn load_file(
        &mut self,
        path: &Path,
        root: bool,
        active: &mut BTreeSet<PathBuf>,
        seen: &mut BTreeSet<PathBuf>,
        digest: &mut Sha1,
    ) -> Result<()> {
        let path = path
            .canonicalize()
            .with_context(|| format!("include {}", path.display()))?;
        ensure!(
            !active.contains(&path),
            "include cycle at {}",
            path.display()
        );
        if !seen.insert(path.clone()) {
            return Ok(());
        }
        ensure!(
            active.len() < MAX_INCLUDE_DEPTH,
            "include nesting exceeds {MAX_INCLUDE_DEPTH} files"
        );
        active.insert(path.clone());
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        for part in [path.to_string_lossy().as_bytes(), text.as_bytes()] {
            digest.update(part.len().to_le_bytes());
            digest.update(part);
        }
        self.env_references.extend(util::env_references(&text));
        let mut doc: Document =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let base = path
            .parent()
            .context("configuration has no parent directory")?;
        // Only the implicit default directory may match nothing.
        let default_pattern = if root {
            self.document.settings = doc.settings;
            // Fragments are implicit only for the default config file. A config
            // selected with -c stays isolated unless it lists this include.
            if is_default_source(&path)?
                && !doc
                    .includes
                    .iter()
                    .any(|include| include == DEFAULT_INCLUDE)
            {
                doc.includes.insert(0, DEFAULT_INCLUDE.into());
            }
            self.document.includes.clone_from(&doc.includes);
            Some(util::expand_path(DEFAULT_INCLUDE, base)?)
        } else {
            None
        };
        self.files.push(path.clone());
        for include in doc.includes {
            let pattern = util::expand_path(&include, base)?;
            let mut paths = glob::glob(&pattern.to_string_lossy())?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            paths.sort();
            ensure!(
                !paths.is_empty() || default_pattern.as_ref() == Some(&pattern),
                "include {include:?} in {} matches no files",
                path.display()
            );
            for child in paths {
                self.load_file(&child, false, active, seen, digest)?;
            }
        }
        let mut defaults = doc.defaults;
        defaults.merge_earlier(&self.document.defaults);
        self.document.defaults = defaults;
        self.document.proxies.extend(doc.proxies);
        merge_entries(&mut self.document.templates, doc.templates);
        merge_entries(&mut self.document.hosts, doc.hosts);
        active.remove(&path);
        Ok(())
    }

    /// Resolves a settings path relative to the root configuration directory.
    pub fn path(&self, input: &str) -> Result<PathBuf> {
        util::expand_path(
            input,
            self.source.parent().context("missing source directory")?,
        )
    }

    /// Path of the generated OpenSSH configuration.
    pub fn output(&self) -> Result<PathBuf> {
        self.path(&self.document.settings.output)
    }

    /// Path of the materialized-target registry.
    pub fn known_hosts(&self) -> Result<PathBuf> {
        self.path(&self.document.settings.known_hosts)
    }

    /// sshweave executable referenced by the generated ProxyCommand.
    pub fn binary(&self) -> Result<PathBuf> {
        match &self.document.settings.binary {
            Some(path) => self.path(path),
            None => Ok(std::env::current_exe()?),
        }
    }

    /// OpenSSH client; bare names are looked up in `PATH`.
    pub fn ssh_binary(&self) -> Result<PathBuf> {
        self.program(&self.document.settings.ssh_binary)
    }

    /// Resolves a configured client program; bare names are looked up in `PATH`.
    pub fn program(&self, setting: &str) -> Result<PathBuf> {
        let value = util::expand_env(setting)?;
        if value.contains('/') || value.starts_with('~') {
            self.path(&value)
        } else {
            Ok(PathBuf::from(value))
        }
    }

    /// Fingerprint of every loaded source file (paths and contents).
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    /// Names of environment variables referenced as `${NAME}` in source files.
    pub fn env_references(&self) -> &BTreeSet<String> {
        &self.env_references
    }

    /// Validates the complete merged configuration.
    pub fn validate(&self) -> Result<()> {
        self.validate_proxies()?;
        self.validate_settings()?;
        let defaults_name = String::from("defaults");
        for (name, host) in self
            .document
            .hosts
            .iter()
            .chain(&self.document.templates)
            .chain(std::iter::once((&defaults_name, &self.document.defaults)))
        {
            host.validate_fields().with_context(|| name.clone())?;
            if let Some(proxy) = &host.proxy {
                ensure!(
                    self.document.proxies.contains_key(proxy),
                    "{name}: unknown proxy {proxy:?}"
                );
            }
        }
        self.resolver()?.validate_gateway_graph()?;
        self.validate_output_paths()
    }

    fn validate_proxies(&self) -> Result<()> {
        for (name, proxy) in &self.document.proxies {
            util::valid_target(name)?;
            ensure!(!name.contains('/'), "proxy name must not contain /");
            crate::proxy::Endpoint::parse(&proxy.url).with_context(|| format!("proxy {name:?}"))?;
        }
        Ok(())
    }

    fn validate_settings(&self) -> Result<()> {
        let settings = &self.document.settings;
        for value in [
            &settings.output,
            &settings.known_hosts,
            &settings.ssh_binary,
            &settings.scp_binary,
            &settings.sftp_binary,
            &settings.rsync_binary,
        ] {
            util::single_line(value)?;
            ensure!(
                !value.is_empty(),
                "settings paths and client binaries cannot be empty"
            );
        }
        Ok(())
    }

    /// Generated files must never overwrite a source file or each other.
    fn validate_output_paths(&self) -> Result<()> {
        let output = normalized(self.output()?)?;
        let known = normalized(self.known_hosts()?)?;
        ensure!(
            !self.files.contains(&output) && !self.files.contains(&known),
            "output and registry must not overwrite any source configuration"
        );
        ensure!(
            output != known,
            "output and registry must be different files"
        );
        Ok(())
    }

    /// Returns `[defaults]` with its own `inherits` applied.
    pub(crate) fn inherit_defaults(&self) -> Result<Host> {
        Inheritance::new(self).defaults()
    }

    /// Splits an `inherits` entry into its kind and name. Bare names prefer
    /// templates over hosts.
    fn parent_kind<'s>(&self, spec: &'s str) -> (&'static str, &'s str) {
        match spec.split_once(':') {
            Some(("host", rest)) => ("host", rest),
            Some(("template", rest)) => ("template", rest),
            _ if self.document.templates.contains_key(spec) => ("template", spec),
            _ => ("host", spec),
        }
    }

    /// Lists the layers merged for the given `[hosts]` keys, highest priority
    /// first: each entry followed by its parents (depth-first, in `inherits`
    /// order), then `[defaults]` and its parents. Layers already listed are
    /// skipped, so folding them with [`Host::fallback`] reproduces resolution.
    pub fn inheritance_layers(&self, entries: &[&str]) -> Result<Vec<InheritanceLayer<'_>>> {
        let mut layers = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in entries {
            self.collect_layers(
                &format!("host:{entry}"),
                &mut layers,
                &mut Vec::new(),
                &mut seen,
            )?;
        }
        if seen.insert("defaults:".to_owned()) {
            layers.push(InheritanceLayer {
                label: "defaults".to_owned(),
                host: &self.document.defaults,
            });
            for parent in &self.document.defaults.inherits {
                self.collect_layers(
                    parent,
                    &mut layers,
                    &mut vec!["defaults:".to_owned()],
                    &mut seen,
                )?;
            }
        }
        Ok(layers)
    }

    fn collect_layers<'s>(
        &'s self,
        spec: &str,
        layers: &mut Vec<InheritanceLayer<'s>>,
        stack: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        let (kind, name) = self.parent_kind(spec);
        let key = format!("{kind}:{name}");
        ensure!(
            stack.len() < MAX_INHERIT_DEPTH && !stack.contains(&key),
            "inheritance cycle/depth limit: {} -> {key}",
            stack.join(" -> ")
        );
        if !seen.insert(key.clone()) {
            return Ok(());
        }
        let host = if kind == "template" {
            self.document.templates.get(name)
        } else {
            self.document.hosts.get(name)
        }
        .with_context(|| format!("unknown inheritance parent {key}"))?;
        layers.push(InheritanceLayer {
            label: format!("{kind} {name}"),
            host,
        });
        stack.push(key);
        for parent in &host.inherits {
            self.collect_layers(parent, layers, stack, seen)?;
        }
        stack.pop();
        Ok(())
    }
}

/// One source of settings in an inheritance chain; see [`Config::inheritance_layers`].
#[derive(Clone, Debug)]
pub struct InheritanceLayer<'a> {
    /// `host NAME`, `template NAME`, or `defaults`.
    pub label: String,
    /// The settings declared by this layer itself.
    pub host: &'a Host,
}

/// Memoizes inheritance only while its source configuration is immutably borrowed.
pub(crate) struct Inheritance<'a> {
    config: &'a Config,
    cache: HashMap<String, (Host, usize)>,
}

impl<'a> Inheritance<'a> {
    pub(crate) fn new(config: &'a Config) -> Self {
        Self {
            config,
            cache: HashMap::new(),
        }
    }

    pub(crate) fn defaults(&mut self) -> Result<Host> {
        let mut defaults = self.config.document.defaults.clone();
        for parent in &self.config.document.defaults.inherits {
            defaults.fallback(&self.host(parent)?);
        }
        Ok(defaults)
    }

    pub(crate) fn host(&mut self, name: &str) -> Result<Host> {
        Ok(self.visit(name, &mut Vec::new())?.0)
    }

    fn visit(&mut self, name: &str, stack: &mut Vec<String>) -> Result<(Host, usize)> {
        let (kind, name) = self.config.parent_kind(name);
        let key = format!("{kind}:{name}");
        ensure!(
            stack.len() < MAX_INHERIT_DEPTH && !stack.contains(&key),
            "inheritance cycle/depth limit: {} -> {key}",
            stack.join(" -> ")
        );
        if let Some((host, depth)) = self.cache.get(&key) {
            // A cached subtree must still fit when reached through a longer prefix.
            ensure!(
                stack.len() + depth <= MAX_INHERIT_DEPTH,
                "inheritance cycle/depth limit: {} -> {key}",
                stack.join(" -> ")
            );
            return Ok((host.clone(), *depth));
        }
        let host = if kind == "template" {
            self.config.document.templates.get(name)
        } else {
            self.config.document.hosts.get(name)
        }
        .with_context(|| format!("unknown inheritance parent {key}"))?;
        stack.push(key.clone());
        let mut result = host.clone();
        let mut depth = 1;
        for parent in &host.inherits {
            let (inherited, parent_depth) = self.visit(parent, stack)?;
            result.fallback(&inherited);
            depth = depth.max(parent_depth + 1);
        }
        stack.pop();
        self.cache.insert(key, (result.clone(), depth));
        Ok((result, depth))
    }
}

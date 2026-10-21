use crate::{hooks::Hook, util};
use anyhow::{Context, Result, bail, ensure};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SshValue {
    Text(String),
    Integer(i64),
    Bool(bool),
    List(Vec<String>),
}

impl SshValue {
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Text(s) => vec![s.clone()],
            Self::Integer(n) => vec![n.to_string()],
            Self::Bool(b) => vec![if *b { "yes" } else { "no" }.into()],
            Self::List(v) => v.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Host {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub aliases: Vec<String>,
    pub inherits: Vec<String>,
    pub gateways: Option<Vec<String>>,
    pub connect_timeout: Option<u64>,
    pub resolve_command: Option<Vec<String>>,
    pub proxy_command: Option<Vec<String>>,
    pub rate_limit: Option<String>,
    pub control_master_mkdir: Option<bool>,
    pub comment: Option<String>,
    pub hooks: BTreeMap<Event, Vec<Hook>>,
    pub ssh: IndexMap<String, SshValue>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    BeforeConnect,
    OnConnect,
    OnConnectError,
    OnDisconnect,
    BeforeConfigWrite,
    AfterConfigWrite,
}

impl Host {
    /// Fill missing values only: child > first parent > later parents > defaults.
    pub fn fallback(&mut self, other: &Self) {
        macro_rules! fill {
            ($($field:ident),*) => {$(if self.$field.is_none() { self.$field = other.$field.clone(); })*};
        }
        fill!(
            hostname,
            user,
            port,
            gateways,
            connect_timeout,
            resolve_command,
            proxy_command,
            rate_limit,
            control_master_mkdir,
            comment
        );
        for (key, value) in &other.ssh {
            if !self.ssh.keys().any(|k| k.eq_ignore_ascii_case(key)) {
                self.ssh.insert(key.clone(), value.clone());
            }
        }
        for (event, hooks) in &other.hooks {
            self.hooks.entry(*event).or_insert_with(|| hooks.clone());
        }
    }

    pub fn ssh_option(&self, key: &str) -> Option<&SshValue> {
        self.ssh
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    pub fn control_path(&self) -> Result<Option<String>> {
        let Some(value) = self.ssh_option("ControlPath") else {
            return Ok(None);
        };
        let lines = value.lines();
        ensure!(lines.len() == 1, "ControlPath requires one value");
        let args = shell_words::split(&lines[0]).context("parse ControlPath")?;
        ensure!(
            args.len() == 1,
            "ControlPath must be a single (possibly quoted) path"
        );
        Ok(if args[0] == "none" {
            None
        } else {
            Some(args[0].clone())
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub output: String,
    pub known_hosts: String,
    pub ssh_binary: String,
    pub binary: Option<String>,
    pub automatic_rewrite: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output: "~/.config/sshweave/sshweave.conf".into(),
            known_hosts: "~/.config/sshweave/sshweave_known_hosts.json".into(),
            ssh_binary: "ssh".into(),
            binary: None,
            automatic_rewrite: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Document {
    pub includes: Vec<String>,
    pub settings: Settings,
    pub defaults: Host,
    pub templates: IndexMap<String, Host>,
    pub hosts: IndexMap<String, Host>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    pub source: PathBuf,
    pub files: Vec<PathBuf>,
    pub document: Document,
}

const DEFAULT_INCLUDE: &str = "${HOME}/.config/sshweave/conf.d/*.toml";

impl Config {
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
        };
        let mut active = BTreeSet::new();
        let mut seen = BTreeSet::new();
        result.load_file(&source, true, &mut active, &mut seen)?;
        result.validate()?;
        Ok(result)
    }

    fn load_file(
        &mut self,
        path: &Path,
        root: bool,
        active: &mut BTreeSet<PathBuf>,
        seen: &mut BTreeSet<PathBuf>,
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
        ensure!(active.len() < 64, "include nesting exceeds 64 files");
        active.insert(path.clone());
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut doc: Document =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let base = path
            .parent()
            .context("configuration has no parent directory")?;
        let default_pattern = if root {
            Some(util::expand_path(DEFAULT_INCLUDE, base)?)
        } else {
            None
        };
        if root {
            self.document.settings = doc.settings;
            if !doc
                .includes
                .iter()
                .any(|include| include == DEFAULT_INCLUDE)
            {
                doc.includes.insert(0, DEFAULT_INCLUDE.into());
            }
            self.document.includes = doc.includes.clone();
        }
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
                self.load_file(&child, false, active, seen)?;
            }
        }
        let mut defaults = doc.defaults;
        defaults.fallback(&self.document.defaults);
        self.document.defaults = defaults;
        for (name, host) in doc.templates {
            if let Some(existing) = self.document.templates.get_mut(&name) {
                let mut merged = host;
                merged.fallback(existing);
                *existing = merged;
            } else {
                self.document.templates.insert(name, host);
            }
        }
        for (name, host) in doc.hosts {
            if let Some(existing) = self.document.hosts.get_mut(&name) {
                let mut merged = host;
                merged.fallback(existing);
                *existing = merged;
            } else {
                self.document.hosts.insert(name, host);
            }
        }
        active.remove(&path);
        Ok(())
    }

    pub fn path(&self, input: &str) -> Result<PathBuf> {
        util::expand_path(
            input,
            self.source.parent().context("missing source directory")?,
        )
    }

    pub fn output(&self) -> Result<PathBuf> {
        self.path(&self.document.settings.output)
    }
    pub fn known_hosts(&self) -> Result<PathBuf> {
        self.path(&self.document.settings.known_hosts)
    }

    pub fn binary(&self) -> Result<PathBuf> {
        match &self.document.settings.binary {
            Some(path) => self.path(path),
            None => Ok(std::env::current_exe()?),
        }
    }

    pub fn ssh_binary(&self) -> Result<PathBuf> {
        let value = util::expand_env(&self.document.settings.ssh_binary)?;
        if value.contains('/') || value.starts_with('~') {
            self.path(&value)
        } else {
            Ok(PathBuf::from(value))
        }
    }

    pub fn validate(&self) -> Result<()> {
        for value in [
            &self.document.settings.output,
            &self.document.settings.known_hosts,
            &self.document.settings.ssh_binary,
        ] {
            util::single_line(value)?;
            ensure!(
                !value.is_empty(),
                "settings paths and ssh_binary cannot be empty"
            );
        }
        let mut aliases = BTreeMap::new();
        for (name, host) in &self.document.hosts {
            crate::resolver::validate_pattern(name)?;
            for alias in &host.aliases {
                crate::resolver::validate_pattern(alias)?;
                if let Some(previous) = aliases.insert(alias, name) {
                    bail!("alias {alias:?} belongs to both {previous:?} and {name:?}");
                }
                ensure!(
                    !self.document.hosts.contains_key(alias),
                    "alias {alias:?} conflicts with a host"
                );
            }
        }
        for (name, host) in self
            .document
            .hosts
            .iter()
            .chain(self.document.templates.iter())
            .chain(std::iter::once((
                &String::from("defaults"),
                &self.document.defaults,
            )))
        {
            ensure!(host.port != Some(0), "{name}: port must be positive");
            ensure!(
                host.connect_timeout
                    .is_none_or(|seconds| seconds <= i32::MAX as u64),
                "{name}: connect_timeout exceeds OpenSSH's supported integer range"
            );
            host.control_path()
                .with_context(|| format!("{name}: ControlPath"))?;
            if let Some(rate) = &host.rate_limit {
                util::rate_bytes(rate).with_context(|| name.clone())?;
            }
            for (label, command) in [
                ("resolve_command", &host.resolve_command),
                ("proxy_command", &host.proxy_command),
            ] {
                if let Some(args) = command {
                    ensure!(
                        !args.is_empty() && !args[0].is_empty(),
                        "{name}: {label} cannot be empty"
                    );
                    for arg in args {
                        util::single_line(arg)?;
                    }
                }
            }
            for gateway in host.gateways.iter().flatten() {
                util::valid_target(gateway)?;
            }
            let mut keys = BTreeSet::new();
            for (key, value) in &host.ssh {
                ensure!(
                    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()),
                    "{name}: invalid SSH option {key:?}"
                );
                let lower = key.to_ascii_lowercase();
                ensure!(
                    keys.insert(lower.clone()),
                    "{name}: duplicate SSH option {key}"
                );
                ensure!(
                    ![
                        "host",
                        "match",
                        "include",
                        "hostname",
                        "user",
                        "port",
                        "proxycommand",
                        "proxyjump",
                        "connecttimeout"
                    ]
                    .contains(&lower.as_str()),
                    "{name}: {key} must be configured through typed sshweave fields (use gateways for ProxyJump)"
                );
                for line in value.lines() {
                    util::single_line(&line)?;
                }
            }
            for hooks in host.hooks.values() {
                for hook in hooks {
                    hook.validate()?;
                }
            }
        }
        // Check every inheritance graph even if no concrete host currently uses it.
        for name in self
            .document
            .templates
            .keys()
            .chain(self.document.hosts.keys())
        {
            self.inherit(name, &mut Vec::new())?;
        }
        self.inherit_defaults()?;
        self.validate_gateway_graph()?;
        fn normalized(path: PathBuf) -> Result<PathBuf> {
            match path.canonicalize() {
                Ok(path) => Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let mut result = PathBuf::new();
                    for part in path.components() {
                        match part {
                            std::path::Component::ParentDir => {
                                result.pop();
                            }
                            std::path::Component::CurDir => {}
                            _ => result.push(part),
                        }
                    }
                    Ok(result)
                }
                Err(error) => Err(error.into()),
            }
        }
        let output = normalized(self.output()?)?;
        let known = normalized(self.known_hosts()?)?;
        ensure!(
            !self.files.contains(&output) && !self.files.contains(&known),
            "output and known_hosts must not overwrite any source configuration"
        );
        ensure!(
            output != known,
            "output and known_hosts must be different files"
        );
        Ok(())
    }

    pub(crate) fn inherit_defaults(&self) -> Result<Host> {
        let mut defaults = self.document.defaults.clone();
        for parent in &self.document.defaults.inherits {
            defaults.fallback(&self.inherit(parent, &mut Vec::new())?);
        }
        Ok(defaults)
    }

    pub(crate) fn inherit(&self, name: &str, stack: &mut Vec<String>) -> Result<Host> {
        let (kind, name) = name
            .split_once(':')
            .filter(|(k, _)| ["host", "template"].contains(k))
            .unwrap_or(("", name));
        let kind = if kind.is_empty() {
            if self.document.templates.contains_key(name) {
                "template"
            } else {
                "host"
            }
        } else {
            kind
        };
        let key = format!("{kind}:{name}");
        ensure!(
            stack.len() < 64 && !stack.contains(&key),
            "inheritance cycle/depth limit: {} -> {key}",
            stack.join(" -> ")
        );
        let host = if kind == "template" {
            self.document.templates.get(name)
        } else {
            self.document.hosts.get(name)
        }
        .with_context(|| format!("unknown inheritance parent {key}"))?;
        stack.push(key);
        let mut result = host.clone();
        for parent in &host.inherits {
            result.fallback(&self.inherit(parent, stack)?);
        }
        stack.pop();
        Ok(result)
    }
}

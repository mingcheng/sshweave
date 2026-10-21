use crate::{
    config::{Config, Host},
    util,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{collections::BTreeSet, fmt::Write};

pub fn is_pattern(name: &str) -> bool {
    name.starts_with("regex:") || name.contains(['*', '?', '['])
}

fn compile_regex(expr: &str) -> Result<regex::Regex> {
    Ok(regex::Regex::new(&format!(r"\A(?:{expr})\z"))?)
}

pub fn validate_pattern(pattern: &str) -> Result<()> {
    ensure!(!pattern.is_empty(), "empty host pattern");
    util::single_line(pattern)?;
    if let Some(expr) = pattern.strip_prefix("regex:") {
        compile_regex(expr).with_context(|| format!("invalid regular expression {pattern:?}"))?;
    } else {
        ensure!(
            !pattern.starts_with('-')
                && !pattern.contains([' ', '\t', '/', '#', '"', '\'', '%', '\\']),
            "invalid host pattern: {pattern:?}"
        );
        glob::Pattern::new(pattern).with_context(|| format!("invalid glob {pattern:?}"))?;
    }
    Ok(())
}

pub fn matches(pattern: &str, name: &str) -> Result<bool> {
    if let Some(expr) = pattern.strip_prefix("regex:") {
        Ok(compile_regex(expr)?.is_match(name))
    } else {
        Ok(glob::Pattern::new(pattern)?.matches(name))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedHost {
    pub name: String,
    pub pattern: Option<String>,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub gateways: Vec<String>,
    pub options: Host,
}

impl ResolvedHost {
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

    pub fn prototype(&self) -> String {
        format!("{}@{}:{}", self.user, self.hostname, self.port)
    }
}

impl Config {
    pub fn requires_materialization(&self, target: &str) -> Result<bool> {
        if target.contains('/') {
            return Ok(true);
        }
        for (pattern, host) in &self.document.hosts {
            for candidate in std::iter::once(pattern).chain(host.aliases.iter()) {
                if is_pattern(candidate) && matches(candidate, target)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn entry(&self, name: &str) -> Result<ResolvedHost> {
        ensure!(
            self.document.hosts.contains_key(name),
            "unknown host entry {name:?}"
        );
        self.resolve_inner(name)
    }

    pub fn resolve(&self, target: &str) -> Result<ResolvedHost> {
        util::valid_target(target)?;
        self.resolve_inner(target)
    }

    pub(crate) fn resolve_inner(&self, target: &str) -> Result<ResolvedHost> {
        let mut parts = target.splitn(2, '/');
        let name = parts.next().context("empty target")?;
        let chain = parts.next();
        let mut selected: Vec<(&str, &Host, &str)> = Vec::new();
        if let Some(host) = self.document.hosts.get(name) {
            selected.push((name, host, name));
        }
        for (pattern, host) in &self.document.hosts {
            if pattern == name {
                continue;
            }
            for candidate in std::iter::once(pattern).chain(host.aliases.iter()) {
                if !is_pattern(candidate) && candidate == name {
                    selected.push((pattern, host, candidate));
                    break;
                }
            }
        }
        for (pattern, host) in &self.document.hosts {
            if selected.iter().any(|(p, _, _)| *p == pattern) {
                continue;
            }
            for candidate in std::iter::once(pattern).chain(host.aliases.iter()) {
                if is_pattern(candidate) && matches(candidate, name)? {
                    selected.push((pattern, host, candidate));
                    break;
                }
            }
        }
        let mut options = Host::default();
        for (index, (pattern, _, candidate)) in selected.iter().enumerate() {
            let mut incoming = self.inherit(&format!("host:{pattern}"), &mut Vec::new())?;
            if let Some(hostname) = &mut incoming.hostname
                && let Some(expr) = candidate.strip_prefix("regex:")
            {
                let re = compile_regex(expr)?;
                if let Some(captures) = re.captures(name) {
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
            }
            if index == 0 {
                options = incoming;
            } else {
                options.fallback(&incoming);
            }
        }
        options.fallback(&self.inherit_defaults()?);
        let canonical = selected.first().map(|(pattern, _, _)| *pattern);
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
        let gateways = chain
            .map(|s| vec![s.to_owned()])
            .unwrap_or_else(|| options.gateways.clone().unwrap_or_default());
        Ok(ResolvedHost {
            name: target.into(),
            pattern: canonical.map(str::to_owned),
            hostname,
            user,
            port: options.port.unwrap_or(22),
            gateways,
            options,
        })
    }

    pub(crate) fn validate_gateway_graph(&self) -> Result<()> {
        fn visit(config: &Config, target: &str, stack: &mut Vec<String>) -> Result<()> {
            ensure!(
                stack.len() < 32 && !stack.iter().any(|x| x == target),
                "gateway cycle/depth limit: {} -> {target}",
                stack.join(" -> ")
            );
            stack.push(target.into());
            let host = config.resolve_inner(target)?;
            for gateway in &host.gateways {
                if gateway != "direct" {
                    visit(config, gateway, stack)?;
                }
            }
            stack.pop();
            Ok(())
        }
        for name in self.document.hosts.keys() {
            visit(self, name, &mut Vec::new())?;
        }
        Ok(())
    }

    pub fn concrete_names(&self) -> BTreeSet<String> {
        self.document
            .hosts
            .iter()
            .flat_map(|(name, host)| std::iter::once(name).chain(host.aliases.iter()))
            .filter(|n| !is_pattern(n))
            .cloned()
            .collect()
    }
}

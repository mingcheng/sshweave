//! sshweave: composable SSH configuration and resilient gateway routing.
//!
//! The crate is organized as a pipeline:
//!
//! 1. [`config`] loads and validates TOML files (includes, templates, inheritance).
//! 2. [`resolver`] turns a target name into a [`resolver::ResolvedHost`].
//! 3. [`render`] compiles the configuration into an OpenSSH `ssh_config`, and
//!    [`store`] persists it atomically together with the materialized-target registry.
//! 4. [`runtime`] is the `ProxyCommand` transport: it tries each route (direct,
//!    [`proxy`], or SSH gateways), runs [`hooks`], and relays bytes.
//!
//! [`topology`] previews routes without connecting, [`sockets`] manages
//! OpenSSH control sockets, and [`util`] holds shared validation helpers.
//!
//! For repeated lookups, borrow a compiled [`resolver::Resolver`] so patterns
//! and inherited options are reused without caching mutable configuration:
//!
//! ```no_run
//! use sshweave::config::Config;
//!
//! let config = Config::load("sshweave.toml")?;
//! let resolver = config.resolver()?;
//! let database = resolver.resolve("database")?;
//! let gateway = resolver.resolve("bastion")?;
//! # Ok::<(), anyhow::Error>(())
//! ```

#![forbid(unsafe_code)]

pub mod config;
pub mod hooks;
pub mod proxy;
pub mod render;
pub mod resolver;
pub mod runtime;
pub mod sockets;
pub mod store;
pub mod topology;
pub mod util;

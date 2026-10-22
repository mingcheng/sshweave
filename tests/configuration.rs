//! Configuration loading, inheritance, resolution, rendering, and persistence.

mod common;

use anyhow::Result;
use sshweave::{
    config::{Config, GatewayStrategy},
    render, store, util,
};
use std::{collections::BTreeSet, fs};
use tempfile::TempDir;

fn fixture(text: &str) -> Result<(TempDir, Config)> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("config.toml"),
        format!("[settings]\noutput = 'generated.conf'\nknown_hosts = 'known.json'\n{text}"),
    )?;
    let config = common::load(dir.path().join("config.toml"))?;
    Ok((dir, config))
}

#[test]
fn defaults_aliases_and_arbitrary_hosts() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[defaults]
user = "ops"
port = 2200
[hosts.gateway]
hostname = "192.0.2.1"
aliases = ["gate", "g"]
"#,
    )?;
    let host = config.resolve("g")?;
    assert_eq!(host.hostname, "192.0.2.1");
    assert_eq!(host.user, "ops");
    assert_eq!(host.port, 2200);
    assert_eq!(config.resolve("example.net")?.hostname, "example.net");
    Ok(())
}

#[test]
fn literal_alias_uses_canonical_name_without_hostname() -> Result<()> {
    let (_dir, config) = fixture("[hosts.server]\naliases = ['s']")?;
    assert_eq!(config.resolve("s")?.hostname, "server");
    Ok(())
}

#[test]
fn recursive_inheritance_has_deterministic_precedence() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[defaults]
user = "default"
[templates.base]
port = 2222
user = "base"
[templates.first]
inherits = ["base"]
user = "first"
[templates.second]
user = "second"
hostname = "192.0.2.8"
[hosts.a]
inherits = ["first", "second"]
[hosts.b]
inherits = ["host:a"]
user = "child"
"#,
    )?;
    let a = config.resolve("a")?;
    assert_eq!(a.user, "first");
    assert_eq!(a.port, 2222);
    assert_eq!(a.hostname, "192.0.2.8");
    assert_eq!(config.resolve("b")?.user, "child");
    assert!(config.resolve("base")?.pattern.is_none());
    Ok(())
}

#[test]
fn wildcard_rules_fill_missing_fields_in_declaration_order() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[hosts."*.prod"]
user = "production"
port = 2222
hostname = "%n.example.net"
[hosts."db*"]
user = "database"
[hosts."db.prod"]
user = "root"
"#,
    )?;
    let db = config.resolve("db.prod")?;
    assert_eq!(db.user, "root");
    assert_eq!(db.port, 2222);
    assert_eq!(db.hostname, "db.prod.example.net");
    assert_eq!(config.resolve("web.prod")?.user, "production");
    Ok(())
}

#[test]
fn regex_captures_and_alias_patterns() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[hosts."regex:^db-(?P<region>[a-z]+)-([0-9]+)$"]
hostname = "{region}-{2}.internal"
user = "db"
[hosts.edge]
aliases = ["regex:^edge([0-9]+)$"]
hostname = "edge-{1}.example.net"
"#,
    )?;
    assert_eq!(config.resolve("db-eu-42")?.hostname, "eu-42.internal");
    assert_eq!(config.resolve("edge3")?.hostname, "edge-3.example.net");
    assert!(config.requires_materialization("edge3")?);
    assert!(config.resolve("prefix-db-eu-42")?.pattern.is_none());
    Ok(())
}

#[test]
fn regex_captures_follow_the_rule_that_supplied_hostname() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[hosts.db42]
user = "override"
[hosts."regex:db(?P<id>[0-9]+)"]
hostname = "node-{id}.internal"
"#,
    )?;
    let host = config.resolve("db42")?;
    assert_eq!(host.hostname, "node-42.internal");
    assert_eq!(host.user, "override");
    assert!(sshweave::resolver::matches("regex:foo|foobar", "foobar")?);
    assert!(!sshweave::resolver::matches("regex:foo|foobar", "foobarx")?);
    Ok(())
}

#[test]
fn slash_chain_overrides_configured_gateways() -> Result<()> {
    let (_dir, config) =
        fixture("[hosts.db]\ngateways = ['old', 'older']\ngateway_strategy='chain'")?;
    let host = config.resolve("db/gate/outer")?;
    assert_eq!(host.hostname, "db");
    assert_eq!(host.gateways, vec!["gate/outer"]);
    assert_eq!(host.gateway_strategy, GatewayStrategy::Sequential);
    assert_eq!(host.routes(), ["gate/outer"]);
    Ok(())
}

#[test]
fn gateway_strategies_resolve_inherit_and_disable() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[defaults]
gateway_strategy = "sequential"
[templates.chain]
gateway_strategy = "chain"
gateways = ["outer", "inner"]
[hosts.db]
inherits = ["chain"]
aliases = ["database"]
[hosts.disabled]
inherits = ["chain"]
gateways = []
[hosts.random]
inherits = ["chain"]
gateway_strategy = "random"
[hosts.ordered]
gateways = ["direct", "outer"]
[hosts."*.private"]
gateway_strategy = "chain"
gateways = ["outer", "inner"]
"#,
    )?;
    let db = config.resolve("database")?;
    assert_eq!(db.gateway_strategy, GatewayStrategy::Chain);
    assert_eq!(db.gateways, ["outer", "inner"]);
    assert_eq!(db.routes(), ["inner/outer"]);
    assert_eq!(config.resolve("disabled")?.routes(), ["direct"]);
    assert_eq!(
        config.resolve("random")?.gateway_strategy,
        GatewayStrategy::Random
    );
    assert_eq!(config.resolve("random")?.routes(), ["outer", "inner"]);
    assert_eq!(
        config.resolve("ordered")?.gateway_strategy,
        GatewayStrategy::Sequential
    );
    assert_eq!(config.resolve("ordered")?.routes(), ["direct", "outer"]);
    assert_eq!(config.resolve("service.private")?.routes(), ["inner/outer"]);
    assert_eq!(
        config.resolve("service.private")?.gateway_strategy,
        GatewayStrategy::Chain
    );
    let (_dir, defaults) = fixture("[hosts.db]\ngateways=['a','b']")?;
    assert_eq!(
        defaults.resolve("db")?.gateway_strategy,
        GatewayStrategy::Random
    );
    assert_eq!(defaults.resolve("unknown")?.routes(), ["direct"]);
    Ok(())
}

#[tokio::test]
async fn chain_routes_are_materialized_deterministically_and_remembered() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[hosts.db]
gateway_strategy = "chain"
gateways = ["outer", "inner"]
[hosts.inner]
hostname = "inner.internal"
user = "ops"
[hosts."*.internal"]
user = "pattern"
"#,
    )?;
    let generated = render::ssh_config(&config, &BTreeSet::new(), true)?;
    assert_eq!(
        generated,
        render::ssh_config(&config, &BTreeSet::new(), true)?
    );
    assert!(generated.contains("Host inner/outer\n  HostName \"inner.internal\"\n  User \"ops\""));
    assert!(generated.contains("Host outer\n"));
    assert!(
        render::graphviz(&config)?
            .contains("\"db\" -> \"inner/outer\" [label=\"gateway (chain)\"]")
    );
    store::remember(&config, &["db".into()])?;
    assert_eq!(
        store::known_hosts(&config)?,
        ["db", "inner/outer", "inner", "outer"]
            .map(String::from)
            .into_iter()
            .collect()
    );
    assert!(store::sync(&config, true).await?);
    assert!(!store::sync(&config, true).await?);
    Ok(())
}

#[test]
fn includes_are_relative_sorted_and_root_overrides() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join("parts"))?;
    fs::write(
        dir.path().join("parts/a.toml"),
        "[templates.base]\nuser='base'\ngateway_strategy='random'\n[hosts.db]\nport=2222\nuser='included'\ngateway_strategy='random'",
    )?;
    fs::write(
        dir.path().join("parts/b.toml"),
        "[hosts.db]\nport=3333\ngateway_strategy='sequential'\ngateways=['outer','inner']",
    )?;
    fs::write(
        dir.path().join("root.toml"),
        "includes=['parts/*.toml']\n[hosts.db]\nuser='root'\ninherits=['base']\ngateway_strategy='chain'",
    )?;
    let config = common::load(dir.path().join("root.toml"))?;
    assert_eq!(config.files.len(), 3);
    assert_eq!(config.resolve("db")?.port, 3333);
    assert_eq!(config.resolve("db")?.user, "root");
    assert_eq!(
        config.resolve("db")?.gateway_strategy,
        GatewayStrategy::Chain
    );
    assert_eq!(config.resolve("db")?.routes(), ["inner/outer"]);
    Ok(())
}

#[test]
fn named_proxies_remain_separate_from_gateway_routes_and_materialized_hosts() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[proxies.local]
url = "http://127.0.0.1:7890"
[proxies.socks]
url = "socks5://[::1]:1080"
[templates.proxied]
proxy = "local"
[hosts.db]
inherits = ["proxied"]
gateways = ["direct", "outer"]
[hosts.private]
proxy = "socks"
gateway_strategy = "chain"
gateways = ["outer", "inner"]
[hosts.outer]
hostname = "outer.internal"
user = "ops"
"#,
    )?;
    assert_eq!(
        config.resolve("db")?.options.proxy.as_deref(),
        Some("local")
    );
    assert_eq!(
        config.resolve("private")?.options.proxy.as_deref(),
        Some("socks")
    );
    assert_eq!(config.resolve("private")?.routes(), ["inner/outer"]);
    let names = config.route_targets(config.concrete_names())?;
    assert!(!names.contains("local") && !names.contains("socks"));
    assert!(names.contains("inner/outer"));
    let generated = render::ssh_config(&config, &BTreeSet::new(), true)?;
    assert!(!generated.contains("127.0.0.1:7890"));
    assert!(generated.contains("Host outer\n  HostName \"outer.internal\"\n  User \"ops\""));
    store::remember(&config, &["private".into(), "db".into()])?;
    assert_eq!(store::known_hosts(&config)?, names);
    let graph = render::graphviz(&config)?;
    assert!(!graph.contains("http://"));
    assert!(graph.contains("\"db\" -> \"proxy:local\" [style=dotted,label=\"proxy\"]"));
    Ok(())
}

#[test]
fn includes_detect_cycles_and_missing_matches() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("a.toml"), "includes=['b.toml']")?;
    fs::write(dir.path().join("b.toml"), "includes=['./a.toml']")?;
    let error = common::load(dir.path().join("a.toml")).unwrap_err();
    assert!(error.to_string().contains("include cycle"));
    fs::write(dir.path().join("a.toml"), "includes=['missing*.toml']")?;
    assert!(common::load(dir.path().join("a.toml")).is_err());
    Ok(())
}

#[test]
fn proxy_definitions_merge_by_name_and_references_inherit_normally() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("included.toml"),
        "[proxies.local]\nurl='http://127.0.0.1:8080'\n[proxies.remote]\nurl='socks5://localhost:1080'\n[defaults]\nproxy='remote'\n[hosts.db]\nproxy='remote'",
    )?;
    fs::write(
        dir.path().join("root.toml"),
        "includes=['included.toml']\n[proxies.local]\nurl='http://127.0.0.1:7890'\n[hosts.db]\nproxy='local'",
    )?;
    let config = common::load(dir.path().join("root.toml"))?;
    assert_eq!(config.document.proxies.len(), 2);
    assert_eq!(
        config.document.proxies["local"].url,
        "http://127.0.0.1:7890"
    );
    assert_eq!(
        config.resolve("db")?.options.proxy.as_deref(),
        Some("local")
    );
    assert_eq!(
        config.resolve("arbitrary")?.options.proxy.as_deref(),
        Some("remote")
    );
    Ok(())
}
#[test]
fn repeated_includes_are_loaded_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("a.toml"), "includes=['b.toml','./b.toml']")?;
    fs::write(dir.path().join("b.toml"), "[hosts.x]")?;
    assert_eq!(common::load(dir.path().join("a.toml"))?.files.len(), 2);
    Ok(())
}

#[test]
fn invalid_configuration_is_rejected() -> Result<()> {
    for text in [
        "[hosts.x]\nport=0",
        "[hosts.x]\nunknown_option=true",
        "[hosts.x]\ngateway_strategy='unknown'",
        "[hosts.x]\ngateway_strategy='chain'\ngateways=['direct']",
        "[hosts.x]\ngateway_strategy='chain'\ngateways=['a/b']",
        "[hosts.x]\ngateways=['proxy+https://localhost:7890']",
        "[hosts.x]\ngateways=['proxy+http://localhost']",
        "[hosts.x]\ngateways=['proxy+http://localhost:0']",
        "[hosts.x]\ngateways=['proxy+http://localhost:65536']",
        "[hosts.x]\ngateways=['proxy+http://user:password@localhost:7890']",
        "[hosts.x]\ngateways=['proxy+http://localhost:7890/path']",
        "[hosts.x]\ngateways=['proxy+http://localhost:7890?query']",
        "[hosts.x]\ngateways=['proxy+http://localhost:7890#fragment']",
        "[hosts.x]\ngateways=['proxy+http://::1:7890']",
        "[hosts.x]\ngateways=['proxy+http://[bad]:7890']",
        "[hosts.x]\ngateways=['proxy+http://localhost:+7890']",
        "[hosts.x]\ngateways=['proxy+http://localhost:7890//']",
        "[hosts.x]\ngateways=['proxy+http://localhost:7890']\nproxy_command=['nc','%h','%p']",
        "[hosts.x]\ngateway_strategy='chain'\ngateways=['ssh-host','proxy+http://localhost:7890']",
        "[hosts.x]\ngateways=['ssh-host/proxy+http://localhost:7890/other']",
        "[hosts.x]\nproxy='missing'",
        "[proxies.local]\nurl='http://localhost:7890'\n[hosts.x]\nproxy='local'\nproxy_command=['nc','%h','%p']",
        "[proxies.local]\nunknown='http://localhost:7890'",
        "[proxies.'bad/name']\nurl='http://localhost:7890'",
        "[templates.bad]\ngateway_strategy='chain'\ngateways=['direct']",
        "[templates.bad]\ngateway_strategy='chain'\n[defaults]\ninherits=['bad']\ngateways=['direct']",
        "[hosts.a]\ngateway_strategy='chain'\ngateways=['b','c']\n[hosts.b]\ngateways=['a']",
        "[hosts.x]\ninherits=['missing']",
        "[templates.a]\ninherits=['b']\n[templates.b]\ninherits=['a']",
        "[hosts.a]\ngateways=['b']\n[hosts.b]\ngateways=['a']",
        "[hosts.'*']\ngateways=['other']",
        "[hosts.'regex:(']",
        "[hosts.'bad name']",
        "[hosts.x]\nproxy_command=[]",
        "[hosts.x]\nrate_limit='0'",
        "[hosts.x.ssh]\nUser='wrong-layer'",
        "[hosts.x.ssh]\nConnectTimeout=3",
        "[hosts.x]\nconnect_timeout=2147483648",
        "[hosts.x.ssh]\nInclude='other.conf'",
        "[hosts.x.ssh]\nCompression=\"yes\\nHost evil\"",
        "[hosts.x.ssh]\nCompression=true\ncompression=false",
        "[hosts.a]\naliases=['x']\n[hosts.b]\naliases=['x']",
        "[hosts.a]\naliases=['b']\n[hosts.b]",
        "[hosts.a.hooks]\non_connect=[{driver='unknown',message='x'}]",
    ] {
        assert!(fixture(text).is_err(), "accepted invalid input:\n{text}");
    }
    Ok(())
}

#[test]
fn targets_cannot_inject_config_shell_or_options() -> Result<()> {
    let (_dir, config) = fixture("")?;
    for target in [
        "",
        "-oProxyCommand=evil",
        "x;touch y",
        "x\nHost evil",
        "a//b",
        "/a",
        "x$(id)",
        "x`id`",
        "a b",
    ] {
        assert!(config.resolve(target).is_err(), "accepted {target:?}");
    }
    assert_eq!(config.resolve("::1")?.hostname, "::1");
    Ok(())
}

#[test]
fn environment_expansion_is_strict_and_preserves_shell_dollars() -> Result<()> {
    common::isolated_home();
    assert_eq!(util::expand_env("${HOME}")?, std::env::var("HOME")?);
    assert_eq!(util::expand_env("$HOME $?")?, "$HOME $?");
    assert!(util::expand_env("${SSHWEAVE_NONEXISTENT_TEST_19654}").is_err());
    Ok(())
}

#[test]
fn byte_rates_accept_si_and_binary_units() -> Result<()> {
    for (input, expected) in [
        ("1KB/s", 1000),
        ("1.5 KiB", 1536),
        ("20", 20),
        ("2MB", 2_000_000),
    ] {
        assert_eq!(util::rate_bytes(input)?, expected);
    }
    for input in ["0", "-1", "infinity", "0.1b", "1XB", "1e100MB"] {
        assert!(util::rate_bytes(input).is_err());
    }
    Ok(())
}

#[test]
fn ssh_option_lists_and_booleans_are_rendered_without_loss() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[hosts.db.ssh]
IdentityFile = ["~/.ssh/key1", "~/.ssh/key2"]
LocalForward = ["8080 localhost:80", "9090 localhost:90"]
Compression = true
ServerAliveInterval = 30
"#,
    )?;
    let output = render::ssh_config(&config, &BTreeSet::new(), true)?;
    assert!(output.contains("  IdentityFile ~/.ssh/key1\n  IdentityFile ~/.ssh/key2\n"));
    assert!(
        output.contains("  LocalForward 8080 localhost:80\n  LocalForward 9090 localhost:90\n")
    );
    assert!(output.contains("  Compression yes\n"));
    assert!(output.contains("  ServerAliveInterval 30\n"));
    assert!(output.contains("connect --port %p --user %r -- %n"));
    Ok(())
}

#[test]
fn rendering_is_stable_and_materializes_regex_and_chains() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[hosts."regex:^srv([0-9]+)$"]
hostname = "node-{1}.internal"
user = "special"
[hosts."*.prod"]
hostname = "%n.internal"
"#,
    )?;
    let known = BTreeSet::from(["srv4/gate".into(), "web.prod".into()]);
    let output = render::ssh_config(&config, &known, false)?;
    assert_eq!(output, render::ssh_config(&config, &known, false)?);
    assert!(output.contains("Host srv4/gate\n  HostName \"node-4.internal\""));
    assert!(
        output.contains("Host *.prod !gate !srv4 !srv4/gate !web.prod\n  HostName \"%h.internal\"")
    );
    assert!(!output.contains("Host regex:"));
    assert!(output.contains("--no-rewrite"));
    Ok(())
}

#[test]
fn graphviz_includes_aliases_templates_and_gateway_edges() -> Result<()> {
    let (_dir, config) = fixture(
        r#"
[templates.base]
user = "ops"
[hosts.db]
inherits = ["base"]
gateways = ["direct", "bastion"]
aliases = ["database"]
"#,
    )?;
    let graph = render::graphviz(&config)?;
    assert!(graph.contains("\"db\" -> \"bastion\" [label=\"gateway (random)\"]"));
    assert!(graph.contains("\"db\" -> \"template:base\""));
    assert!(graph.contains("\"database\" -> \"db\""));
    Ok(())
}

#[tokio::test]
async fn managed_sync_is_atomic_private_and_idempotent() -> Result<()> {
    let (_dir, config) = fixture("[hosts.db]\nuser='ops'")?;
    assert!(store::sync(&config, true).await?);
    assert!(!store::sync(&config, true).await?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(config.output()?)?.permissions().mode() & 0o777,
            0o600
        );
    }
    fs::write(config.output()?, "# hand-written configuration\n")?;
    assert!(store::sync(&config, true).await.is_err());
    assert_eq!(
        fs::read_to_string(config.output()?)?,
        "# hand-written configuration\n"
    );
    Ok(())
}

#[tokio::test]
async fn known_hosts_preserve_full_gateway_suffixes_and_nested_routes() -> Result<()> {
    let (_dir, config) = fixture("[hosts.db]\ngateways=['gate/outer']")?;
    store::remember(&config, &["db".into()])?;
    assert_eq!(
        store::known_hosts(&config)?,
        BTreeSet::from([
            "db".into(),
            "gate/outer".into(),
            "gate".into(),
            "outer".into()
        ])
    );
    assert!(store::sync(&config, true).await?);
    fs::write(config.known_hosts()?, "not json")?;
    assert!(store::known_hosts(&config).is_err());
    Ok(())
}

#[tokio::test]
async fn config_write_hooks_observe_output_and_fail_explicitly() -> Result<()> {
    let (dir, config) = fixture(
        r#"
[defaults.hooks]
after_config_write = [{driver="exec", command="test -f {{ ssh_config_path | shellquote }}"}]
"#,
    )?;
    assert!(store::sync(&config, true).await?);
    let (_dir, config) = fixture(
        r#"
[defaults.hooks]
before_config_write = [{driver="exec", command="exit 13"}]
"#,
    )?;
    assert!(store::sync(&config, true).await.is_err());
    assert!(!config.output()?.exists());
    drop(dir);
    Ok(())
}

#[test]
fn configuration_sources_cannot_be_overwritten() -> Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("root.toml"),
        "[settings]\noutput='./root.toml'",
    )?;
    assert!(common::load(dir.path().join("root.toml")).is_err());
    fs::write(dir.path().join("part.toml"), "[hosts.db]")?;
    fs::write(
        dir.path().join("root.toml"),
        "includes=['part.toml']\n[settings]\noutput='part.toml'",
    )?;
    assert!(common::load(dir.path().join("root.toml")).is_err());
    Ok(())
}

#[tokio::test]
async fn concurrent_sync_does_not_deadlock_or_corrupt_output() -> Result<()> {
    let (_dir, config) = fixture("[hosts.db]")?;
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(store::sync(&config, true), store::sync(&config, true))
    })
    .await?;
    assert_ne!(a?, b?);
    assert!(fs::read_to_string(config.output()?)?.starts_with(render::HEADER));
    Ok(())
}

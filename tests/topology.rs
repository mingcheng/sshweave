//! Route topology previews.

mod common;

use anyhow::Result;
use sshweave::{config::Config, topology};
use std::{fs, process::Command};
use tempfile::TempDir;

fn fixture(text: &str) -> Result<(TempDir, Config)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\n\
             [defaults]\nuser='ops'\n{text}"
        ),
    )?;
    let config = common::load(path)?;
    Ok((dir, config))
}

fn render(config: &Config, target: &str) -> Result<String> {
    let mut output = Vec::new();
    topology::write(config, target, false, false, &mut output)?;
    Ok(String::from_utf8(output)?)
}

fn assert_order(output: &str, labels: &[&str]) {
    let mut remaining = output;
    for label in labels {
        let index = remaining
            .find(label)
            .unwrap_or_else(|| panic!("missing or out-of-order {label:?} in:\n{output}"));
        remaining = &remaining[index + label.len()..];
    }
}

#[test]
fn direct_alias_pattern_and_ipv6_endpoints() -> Result<()> {
    let (_dir, config) = fixture(
        "[hosts.database]\nhostname='2001:db8::1'\nport=2222\naliases=['db']\n\
         [hosts.'*.dev']\nhostname='%n.internal'\n",
    )?;
    let output = render(&config, "db")?;
    assert_order(
        &output,
        &[
            "SSH route topology: db [preview only]",
            "Local (this machine)",
            "Route 1 | direct (no SSH gateway)",
            "db  [SSH ops@[2001:db8::1]:2222]",
        ],
    );
    assert!(output.contains("└"));
    assert!(!output.contains('\u{1b}'));
    assert!(render(&config, "worker.dev")?.contains("worker.dev.internal"));
    assert!(render(&config, "unknown.example")?.contains("ops@unknown.example:22"));
    Ok(())
}

#[test]
fn ordered_and_random_candidates_are_not_misrepresented_as_a_chain() -> Result<()> {
    for strategy in ["sequential", "random"] {
        let (_dir, config) = fixture(&format!(
            "[hosts.db]\ngateway_strategy='{strategy}'\ngateways=['direct','a','b']\n\
             [hosts.a]\nhostname='192.0.2.1'\n\
             [hosts.b]\nhostname='192.0.2.2'\n"
        ))?;
        let output = render(&config, "db")?;
        assert_eq!(output, render(&config, "db")?);
        assert_order(
            &output,
            &[
                &format!("Route 1 | db: {strategy} #1 via direct"),
                "db  [SSH",
                &format!("Route 2 | db: {strategy} #2 via a"),
                "a  [SSH ops@192.0.2.1:22]",
                "db  [SSH",
                &format!("Route 3 | db: {strategy} #3 via b"),
                "b  [SSH ops@192.0.2.2:22]",
                "db  [SSH",
            ],
        );
        assert_eq!(output.matches("db  [SSH").count(), 3);
    }
    Ok(())
}

#[test]
fn chain_and_explicit_chain_follow_connection_order() -> Result<()> {
    let (_dir, config) = fixture(
        "[hosts.db]\ngateway_strategy='chain'\ngateways=['outer','inner']\n\
         [hosts.outer]\n[hosts.inner]\n[hosts.override]\n",
    )?;
    let output = render(&config, "db")?;
    assert!(output.contains("db: chain #1 via inner/outer"));
    assert_order(&output, &["outer  [SSH", "inner  [SSH", "db  [SSH"]);
    assert_eq!(output.matches("Route ").count(), 1);
    let output = render(&config, "db/override")?;
    assert!(output.contains("explicit chain"));
    assert_order(&output, &["override  [SSH", "db  [SSH"]);
    assert!(!output.contains("outer  [SSH"));
    Ok(())
}

#[test]
fn nested_fallbacks_expand_each_complete_path() -> Result<()> {
    let (_dir, config) = fixture(
        "[hosts.db]\ngateways=['inner']\n\
         [hosts.inner]\ngateway_strategy='sequential'\ngateways=['a','b']\n\
         [hosts.a]\n[hosts.b]\n",
    )?;
    let output = render(&config, "db")?;
    assert_order(
        &output,
        &[
            "Route 1",
            "a  [SSH",
            "inner  [SSH",
            "db  [SSH",
            "Route 2",
            "b  [SSH",
            "inner  [SSH",
            "db  [SSH",
        ],
    );
    assert!(output.contains("inner: sequential #2 via b"));
    Ok(())
}

#[test]
fn proxy_is_before_outermost_gateway_and_target_proxy_takes_precedence() -> Result<()> {
    let (_dir, config) = fixture(
        "[proxies.local]\nurl='socks5://127.0.0.1:1080'\n\
         [proxies.other]\nurl='http://127.0.0.1:8080'\n\
         [hosts.db]\nproxy='local'\ngateway_strategy='chain'\ngateways=['outer','inner']\n\
         [hosts.outer]\nproxy='other'\n[hosts.inner]\n",
    )?;
    let output = render(&config, "db")?;
    assert_order(
        &output,
        &[
            "SOCKS5 proxy local",
            "outer  [SSH",
            "inner  [SSH",
            "db  [SSH",
        ],
    );
    assert!(!output.contains("HTTP CONNECT proxy"));
    let output = render(&config, "outer")?;
    assert_order(&output, &["HTTP CONNECT proxy other", "outer  [SSH"]);
    Ok(())
}

#[test]
fn gateway_proxy_applies_when_target_has_none() -> Result<()> {
    let (_dir, config) = fixture(
        "[proxies.local]\nurl='http://127.0.0.1:8080'\n\
         [hosts.db]\ngateways=['outer']\n[hosts.outer]\nproxy='local'\n",
    )?;
    assert_order(
        &render(&config, "db")?,
        &["HTTP CONNECT proxy local", "outer  [SSH", "db  [SSH"],
    );
    Ok(())
}

#[test]
fn commands_are_annotated_but_not_executed() -> Result<()> {
    let (dir, config) = fixture(
        "[hosts.db]\ngateways=['gateway']\n\
         proxy_command=['/nonexistent-proxy']\nresolve_command=['/nonexistent-resolver']\n\
         [hosts.db.hooks]\nbefore_connect=[{driver='exec',command='exit 97'}]\n\
         [hosts.gateway]\nproxy_command=['/nonexistent-local-proxy']\n",
    )?;
    let output = render(&config, "db")?;
    assert_order(
        &output,
        &[
            "proxy_command [local; not executed]",
            "gateway  [SSH",
            "proxy_command [on preceding SSH gateway; not executed]",
            "db  [SSH",
            "[resolve_command not executed; endpoint is provisional]",
        ],
    );
    assert_eq!(fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

#[test]
fn cli_prints_ascii_without_writing_state_or_starting_ssh() -> Result<()> {
    let (dir, config) = fixture(
        "[hosts.db]\ngateways=['gateway']\nresolve_command=['/nonexistent-resolver']\n\
         [hosts.db.hooks]\nbefore_connect=[{driver='exec',command='exit 97'}]\n\
         [hosts.gateway]\n",
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_sshweave"))
        .env("HOME", dir.path())
        .env("PATH", dir.path())
        .env("SSHWEAVE_ROUTE_PROXY", "must-not-use-runtime-environment")
        .arg("-c")
        .arg(&config.source)
        .args(["topology", "db/gateway", "--ascii"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.is_ascii());
    assert!(!stdout.contains('\u{1b}'));
    assert_order(&stdout, &["gateway  [SSH", "db  [SSH"]);
    assert_eq!(fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

#[test]
fn invalid_targets_and_dynamic_cycles_fail_without_partial_output() -> Result<()> {
    let (_dir, mut config) = fixture("[hosts.a]\n[hosts.b]\n")?;
    for target in ["-invalid", "a/"] {
        let mut output = Vec::new();
        assert!(topology::write(&config, target, false, false, &mut output).is_err());
        assert!(output.is_empty());
    }
    config.document.hosts.get_mut("a").unwrap().gateways = Some(vec!["b/a".into()]);
    let mut output = Vec::new();
    let error = topology::write(&config, "a", false, false, &mut output).unwrap_err();
    assert!(error.to_string().contains("gateway cycle/depth limit"));
    assert!(output.is_empty());
    let target = std::iter::repeat_n("b", 33).collect::<Vec<_>>().join("/");
    let error = topology::write(&config, &target, false, false, &mut output).unwrap_err();
    assert!(error.to_string().contains("gateway cycle/depth limit"));
    assert!(output.is_empty());
    Ok(())
}

#[test]
fn terminal_control_characters_are_escaped() -> Result<()> {
    let (_dir, mut config) = fixture("[hosts.db]\n")?;
    config.document.hosts.get_mut("db").unwrap().user = Some("ops\u{1b}[31m".into());
    let output = render(&config, "db")?;
    assert!(!output.contains('\u{1b}'));
    assert!(output.contains(r"ops\u{1b}[31m"));
    Ok(())
}

#[test]
fn excessive_expansion_is_an_explicit_error() -> Result<()> {
    let (_dir, mut config) = fixture("[hosts.db]\ngateways=['a']\n[hosts.a]\n")?;
    config.document.hosts.get_mut("db").unwrap().gateways = Some(vec!["a".into(); 4097]);
    let mut output = Vec::new();
    let error = topology::write(&config, "db", false, false, &mut output).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("topology expansion exceeds 4096 nodes")
    );
    assert!(output.is_empty());
    Ok(())
}

//! End-to-end transport, wrapper, hook, and generated-configuration behavior.

mod common;

use anyhow::{Context, Result};
use sshweave::runtime::{self, RateLimiter};
use std::{fs, process::Stdio, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    time::{Instant, timeout},
};

fn fixture(text: &str) -> Result<(TempDir, std::path::PathBuf)> {
    let dir = tempfile::Builder::new()
        .prefix("sshweave integration ")
        .tempdir()?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!("[settings]\noutput='generated.conf'\nknown_hosts='known.json'\n{text}"),
    )?;
    Ok((dir, path))
}

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sshweave"));
    command
        .env("HOME", common::isolated_home())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn default_paths_keep_generated_state_in_config_directory() -> Result<()> {
    let home = tempfile::tempdir()?;
    let home_path = home.path().canonicalize()?;
    let directory = home_path.join(".config/sshweave");
    fs::create_dir_all(&directory)?;
    let source = directory.join("sshweave.toml");
    fs::write(&source, "[defaults]\nuser='ops'\n")?;
    let source = source.canonicalize()?;
    let directory = source.parent().context("missing source directory")?;
    let output = binary()
        .env("HOME", &home_path)
        .env_remove("SSHWEAVE_CONFIG")
        .arg("info")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(info["source"], source.to_string_lossy().as_ref());
    assert_eq!(
        info["output"],
        directory.join("sshweave.conf").to_string_lossy().as_ref()
    );
    assert_eq!(
        info["registry"],
        directory
            .join("sshweave_known_hosts.json")
            .to_string_lossy()
            .as_ref()
    );
    assert!(info["git_ssh_command"].as_str().unwrap().contains("-F"));
    assert!(info["include"].as_str().unwrap().starts_with("Include "));
    assert!(info["rewrite"].as_str().unwrap().contains("wrapper"));

    let output = binary()
        .env("HOME", &home_path)
        .env_remove("SSHWEAVE_CONFIG")
        .env("TMPDIR", home.path().join("nonexistent-temp"))
        .args(["connect", "database/gateway"])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("configuration updated after OpenSSH read it")
    );
    assert!(
        fs::read_to_string(directory.join("sshweave.conf"))?.starts_with(sshweave::render::HEADER)
    );
    let known: std::collections::BTreeSet<String> =
        serde_json::from_slice(&fs::read(directory.join("sshweave_known_hosts.json"))?)?;
    assert_eq!(
        known,
        ["database", "gateway", "database/gateway"]
            .map(String::from)
            .into_iter()
            .collect()
    );
    let files = fs::read_dir(directory)?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    assert_eq!(
        files,
        [
            "sshweave.toml",
            "sshweave.conf",
            "sshweave.conf.lock",
            "sshweave_known_hosts.json",
            "sshweave_known_hosts.json.lock",
        ]
        .map(std::ffi::OsString::from)
        .into_iter()
        .collect()
    );
    assert!(!home.path().join(".ssh").exists());
    Ok(())
}

#[tokio::test]
async fn config_environment_and_cli_override_default_path() -> Result<()> {
    let home = tempfile::tempdir()?;
    let (env_dir, env_path) = fixture("[hosts.from_env]")?;
    let (_cli_dir, cli_path) = fixture("[hosts.from_cli]")?;
    for (args, expected) in [
        (vec!["config", "list", "--json"], "from_env"),
        (
            vec![
                "-c",
                cli_path
                    .to_str()
                    .context("configuration path is not UTF-8")?,
                "config",
                "list",
                "--json",
            ],
            "from_cli",
        ),
    ] {
        let output = binary()
            .env("HOME", home.path())
            .env("SSHWEAVE_CONFIG", &env_path)
            .args(args)
            .output()
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["hosts"][0]["name"], expected);
    }
    let config = common::load(&env_path)?;
    assert_eq!(
        config.output()?,
        env_dir.path().canonicalize()?.join("generated.conf")
    );
    assert_eq!(
        config.known_hosts()?,
        env_dir.path().canonicalize()?.join("known.json")
    );
    assert!(!home.path().join(".config").exists());
    Ok(())
}

#[tokio::test]
async fn default_fragments_merge_with_explicit_includes_and_root() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config_dir = home.path().join(".config/sshweave");
    fs::create_dir_all(&config_dir)?;
    let source = config_dir.join("sshweave.toml");
    let fragments = config_dir.join("conf.d");
    fs::create_dir_all(&fragments)?;
    fs::write(
        fragments.join("a.toml"),
        "[settings]\noutput='ignored.conf'\n[templates.base]\nuser='fragment'\n\
         [hosts.db]\ninherits=['base']\nport=2201\nhostname='first.internal'",
    )?;
    fs::write(
        fragments.join("b.toml"),
        "includes=['../shared.toml']\n[hosts.db]\nport=2202\nhostname='last.internal'",
    )?;
    fs::write(
        fragments
            .parent()
            .context("missing parent")?
            .join("shared.toml"),
        "[hosts.shared]\nuser='shared'",
    )?;
    fs::write(fragments.join("ignored.txt"), "not TOML")?;
    fs::write(
        config_dir.join("extra.toml"),
        "[hosts.db]\nport=3300\nuser='explicit'",
    )?;
    let export = || {
        let mut command = binary();
        command
            .env("HOME", home.path())
            .env_remove("SSHWEAVE_CONFIG")
            .arg("-c")
            .arg(&source);
        command
    };

    for (includes, expected_user, expected_port, expected_includes) in [
        ("", "fragment", 2202, 1),
        ("includes=['extra.toml']\n", "explicit", 3300, 2),
        (
            "includes=['${HOME}/.config/sshweave/conf.d/*.toml','extra.toml']\n",
            "explicit",
            3300,
            2,
        ),
    ] {
        fs::write(
            &source,
            format!("{includes}[hosts.db]\ninherits=['base']\nconnect_timeout=7\n"),
        )?;
        let output = export().args(["config", "json"]).output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let document = &value["document"];
        assert_eq!(
            document["includes"].as_array().unwrap().len(),
            expected_includes
        );
        assert_eq!(
            document["includes"][0],
            "${HOME}/.config/sshweave/conf.d/*.toml"
        );
        assert_eq!(
            value["files"].as_array().unwrap().len(),
            if expected_includes == 1 { 4 } else { 5 }
        );
        assert_eq!(
            document["settings"]["output"],
            "~/.config/sshweave/sshweave.conf"
        );
        let output = export().args(["config", "resolve", "db"]).output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let host: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(host["user"], expected_user);
        assert_eq!(host["port"], expected_port);
        assert_eq!(host["hostname"], "last.internal");
        assert_eq!(host["options"]["connect_timeout"], 7);
    }

    fs::write(
        &source,
        "includes=['extra.toml','${HOME}/.config/sshweave/conf.d/*.toml']\n\
         [hosts.db]\nuser='root'",
    )?;
    let output = export().args(["config", "resolve", "db"]).output().await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let host: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(host["user"], "root");
    assert_eq!(host["port"], 2202);

    let (_dir, other) = fixture("[hosts.db]\nuser='only-root'\n")?;
    let output = binary()
        .env("HOME", home.path())
        .env_remove("SSHWEAVE_CONFIG")
        .arg("-c")
        .arg(&other)
        .args(["config", "json"])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(value["document"]["includes"].as_array().unwrap().is_empty());
    let output = binary()
        .env("HOME", home.path())
        .env_remove("SSHWEAVE_CONFIG")
        .arg("-c")
        .arg(&other)
        .args(["config", "resolve", "db"])
        .output()
        .await?;
    assert!(output.status.success());
    let host: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(host["user"], "only-root");
    Ok(())
}

#[tokio::test]
async fn default_fragments_allow_empty_directory_but_report_invalid_files() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config_dir = home.path().join(".config/sshweave");
    fs::create_dir_all(&config_dir)?;
    let source = config_dir.join("sshweave.toml");
    fs::write(&source, "[defaults]\nuser='ops'\n")?;
    let fragments = config_dir.join("conf.d");
    let check = || {
        let mut command = binary();
        command
            .env("HOME", home.path())
            .env_remove("SSHWEAVE_CONFIG")
            .arg("-c")
            .arg(&source)
            .args(["config", "check"]);
        command
    };
    for create_directory in [false, true] {
        if create_directory {
            fs::create_dir_all(&fragments)?;
        }
        let output = check().output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for (text, expected) in [
        ("invalid TOML", "parse"),
        ("includes=['missing.toml']", "matches no files"),
        ("includes=['./part.toml']", "include cycle"),
    ] {
        fs::write(fragments.join("part.toml"), text)?;
        let output = check().output().await?;
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[tokio::test]
async fn tcp_proxy_preserves_binary_data_and_half_close() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut data = Vec::new();
        socket.read_to_end(&mut data).await?;
        socket.write_all(b"reply\0").await?;
        socket.write_all(&data).await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let (_dir, path) = fixture(&format!("[hosts.test]\nhostname='127.0.0.1'\nport={port}"))?;
    let mut child = binary()
        .arg("--config")
        .arg(&path)
        .args(["connect", "--no-rewrite", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("stdin missing")?;
    stdin.write_all(b"input\0\xff").await?;
    drop(stdin);
    let output = timeout(Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"reply\0input\0\xff");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn remote_eof_exits_even_with_open_local_stdin() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"banner").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let (_dir, path) = fixture(&format!("[hosts.test]\nhostname='127.0.0.1'\nport={port}"))?;
    let mut child = binary()
        .arg("-c")
        .arg(&path)
        .args(["connect", "--no-rewrite", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let held_stdin = child.stdin.take();
    let output = timeout(Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"banner");
    drop(held_stdin);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn hooks_do_not_corrupt_protocol_and_disconnect_stats_are_accurate() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut data = Vec::new();
        socket.read_to_end(&mut data).await?;
        socket.write_all(b"hello").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let (_dir, path) = fixture(&format!(
        r#"
[hosts.test]
hostname="127.0.0.1"
port={port}
[hosts.test.hooks]
before_connect=[{{driver="exec", command="printf 'hook stdout'"}}, {{driver="write", message="before {{{{ host.name }}}}"}}]
on_disconnect=[{{driver="write", message="sent={{{{ stats.sent_bytes }}}} received={{{{ stats.received_bytes }}}}"}}]
"#
    ))?;
    let mut child = binary()
        .arg("-c")
        .arg(&path)
        .args(["connect", "--no-rewrite", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("stdin missing")?;
    stdin.write_all(b"abc").await?;
    drop(stdin);
    let output = timeout(Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"hello");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("hook stdout"));
    assert!(stderr.contains("before test"));
    assert!(stderr.contains("sent=3 received=5"));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn limiter_enforces_shared_bidirectional_byte_budget() -> Result<()> {
    let limiter = Arc::new(RateLimiter::new(1000)?);
    let started = Instant::now();
    let (a, b) = tokio::join!(
        runtime::copy_limited(
            std::io::Cursor::new(vec![0; 150]),
            tokio::io::sink(),
            Some(limiter.clone())
        ),
        runtime::copy_limited(
            std::io::Cursor::new(vec![0; 150]),
            tokio::io::sink(),
            Some(limiter)
        )
    );
    assert_eq!(a?, 150);
    assert_eq!(b?, 150);
    assert!(
        started.elapsed() >= Duration::from_millis(295),
        "limiter allowed an excessive burst"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "limiter stalled"
    );
    Ok(())
}

#[tokio::test]
async fn resolve_command_is_argv_based_and_rejects_bad_output() -> Result<()> {
    let (_dir, path) = fixture("[hosts.test]\nresolve_command=['printf', '%s', '127.0.0.1']")?;
    let config = common::load(&path)?;
    let mut host = config.resolve("test")?;
    runtime::prepare(&mut host, "").await?;
    assert_eq!(host.hostname, "127.0.0.1");
    let (_dir, path) = fixture("[hosts.test]\nresolve_command=['printf', 'two hosts']")?;
    let config = common::load(&path)?;
    assert!(
        runtime::prepare(&mut config.resolve("test")?, "")
            .await
            .is_err()
    );
    let (_dir, path) = fixture("[hosts.test]\nresolve_command=['false']")?;
    let config = common::load(&path)?;
    assert!(
        runtime::prepare(&mut config.resolve("test")?, "")
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn control_path_expansion_handles_literal_percent_and_makes_private_dirs() -> Result<()> {
    let (dir, path) = fixture(
        "[hosts.test]\ncontrol_master_mkdir=true\n[hosts.test.ssh]\nControlPath='${HOME}/.ssh/unused'",
    )?;
    let mut config = common::load(&path)?;
    config
        .document
        .hosts
        .get_mut("test")
        .context("test host missing")?
        .ssh
        .insert(
            "ControlPath".into(),
            sshweave::config::SshValue::Text(format!(
                "'{}/controls/%n/%%h-%p'",
                dir.path().display()
            )),
        );
    let host = config.resolve("test")?;
    assert_eq!(host.expand("%%h-%h-%p", "")?, "%h-test-22");
    runtime::prepare_control_path(&host)?;
    let parent = dir.path().join("controls/test");
    assert!(parent.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(parent)?.permissions().mode() & 0o777, 0o700);
    }
    Ok(())
}

#[cfg(unix)]
fn executable(path: &std::path::Path, source: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, source)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failed_gateway_falls_back_before_any_protocol_data_is_emitted() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ssh = dir.path().join("fake-ssh");
    executable(
        &ssh,
        "#!/bin/sh\nfor arg do last=\"$arg\"; done\nif [ \"$last\" = broken ]; then exit 7; fi\nprintf 'SSH-2.0-good\\r\\n'\ncat\n",
    )?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!(
            r#"
[settings]
output="generated.conf"
known_hosts="known.json"
ssh_binary="{}"
[hosts.db]
gateways=["broken","healthy"]
gateway_strategy="sequential"
connect_timeout=2
"#,
            ssh.display()
        ),
    )?;
    let output = timeout(
        Duration::from_secs(5),
        binary()
            .arg("-c")
            .arg(path)
            .args(["connect", "--no-rewrite", "db"])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SSH-2.0-good\r\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("broken"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn direct_eof_and_startup_timeout_try_the_next_route() -> Result<()> {
    for stall in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            if stall {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            drop(socket);
            Ok::<_, std::io::Error>(())
        });
        let directory = tempfile::tempdir()?;
        let ssh = directory.path().join("ssh");
        executable(&ssh, "#!/bin/sh\nprintf 'SSH-2.0-fallback\\r\\n'\ncat\n")?;
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nssh_binary='{}'\n\
                 [hosts.db]\nhostname='127.0.0.1'\nport={port}\n\
                 gateways=['direct','healthy']\ngateway_strategy='sequential'\nconnect_timeout=1\n\
                 [hosts.db.hooks]\non_connect_error=[{{driver='write',message='failed:{{{{ gateway }}}}'}}]\n",
                ssh.display()
            ),
        )?;
        let started = Instant::now();
        let output = timeout(
            Duration::from_secs(5),
            binary()
                .arg("-c")
                .arg(path)
                .args(["connect", "--no-rewrite", "db"])
                .stdin(Stdio::null())
                .output(),
        )
        .await??;
        if stall {
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            assert!(started.elapsed() >= Duration::from_millis(950));
        } else {
            server.await??;
        }
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"SSH-2.0-fallback\r\n");
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("failed:direct"));
        assert!(stderr.contains(if stall {
            "direct startup timeout"
        } else {
            "direct route closed before receiving destination data"
        }));
    }
    Ok(())
}

#[tokio::test]
async fn direct_startup_probe_replays_every_byte_and_preserves_half_close() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        stream.write_all(b"SSH-2.0-direct\r\n\0\xff").await?;
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await?;
        stream.write_all(&received).await?;
        stream.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let (_directory, path) = fixture(&format!(
        "[hosts.db]\nhostname='127.0.0.1'\nport={port}\n\
         gateways=['direct','unused']\ngateway_strategy='sequential'\nconnect_timeout=1\n\
         [hosts.unused]\nproxy_command=['/nonexistent-transport']\n"
    ))?;
    let mut child = binary()
        .arg("-c")
        .arg(path)
        .args(["connect", "--no-rewrite", "db"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing stdin")?;
    stdin.write_all(b"payload\0\xff").await?;
    drop(stdin);
    let output = timeout(Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"SSH-2.0-direct\r\n\0\xffpayload\0\xff");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn wrapper_rejects_jump_overrides_before_updating_state() -> Result<()> {
    let (_directory, path) = fixture("[hosts.db]\nhostname='127.0.0.1'\n")?;
    let config = common::load(&path)?;
    let baseline = binary()
        .arg("-c")
        .arg(&path)
        .args(["wrapper", "ssh", "-G", "db"])
        .output()
        .await?;
    assert!(baseline.status.success(), "{baseline:?}");
    assert!(String::from_utf8(baseline.stdout)?.contains("proxycommand "));
    // Configured hosts need no registry entry, so the registry may not exist.
    let registry = fs::read(config.known_hosts()?).ok();
    let generated = fs::read(config.output()?)?;
    for args in [
        vec!["-J", "jump.invalid", "-G", "new-target"],
        vec!["-Jjump.invalid", "-G", "new-target"],
        vec!["-vJjump.invalid", "-G", "new-target"],
        vec!["-o", "ProxyJump=jump.invalid", "-G", "new-target"],
    ] {
        let output = binary()
            .arg("-c")
            .arg(&path)
            .args(["wrapper", "ssh"])
            .args(args)
            .output()
            .await?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.contains("wrapper manages SSH routing"));
        assert_eq!(fs::read(config.known_hosts()?).ok(), registry);
        assert_eq!(fs::read(config.output()?)?, generated);
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn random_routes_preserve_fallback_success_and_report_exhaustion() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ssh = dir.path().join("fake-ssh");
    let log = dir.path().join("routes.log");
    executable(
        &ssh,
        r#"#!/bin/sh
for arg do last="$arg"; done
printf '%s\n' "$last" >> "$ROUTE_LOG"
if [ "$SUCCEED_LAST" = yes ] && [ "$(wc -l < "$ROUTE_LOG" | tr -d ' ')" = 3 ]; then
    printf 'SSH-2.0-random\r\n'
    cat
else
    exit 7
fi
"#,
    )?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nssh_binary='{}'\n\
         [hosts.db]\ngateways=['a','b','c']\ngateway_strategy='random'\n\
         [hosts.db.hooks]\non_connect_error=[{{driver='write',message='failed-route:{{{{ gateway }}}}'}}]\n",
            ssh.display()
        ),
    )?;
    for succeed_last in [false, true] {
        fs::write(&log, "")?;
        let output = timeout(
            Duration::from_secs(5),
            binary()
                .env("ROUTE_LOG", &log)
                .env("SUCCEED_LAST", if succeed_last { "yes" } else { "no" })
                .arg("-c")
                .arg(&path)
                .args(["connect", "--no-rewrite", "db"])
                .stdin(Stdio::null())
                .output(),
        )
        .await??;
        let text = fs::read_to_string(&log)?;
        let attempts: Vec<_> = text.lines().collect();
        let mut sorted = attempts.clone();
        sorted.sort();
        assert_eq!(sorted, ["a", "b", "c"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if succeed_last {
            assert!(output.status.success(), "{stderr}");
            assert_eq!(output.stdout, b"SSH-2.0-random\r\n");
        } else {
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(stderr.contains("all routes failed for db"));
        }
        for gateway in attempts.iter().take(if succeed_last { 2 } else { 3 }) {
            assert!(stderr.contains(&format!("failed-route:{gateway}")));
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn chain_transport_uses_all_hops_in_order_and_never_bypasses_failure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ssh = dir.path().join("fake-ssh");
    let log = dir.path().join("routes.log");
    // Emulate SSH's ProxyCommand invocation, leaving route planning to the real CLI.
    executable(
        &ssh,
        r#"#!/bin/sh
for arg do last="$arg"; done
printf '%s\n' "$*" >> "$ROUTE_LOG"
if [ "$FAIL_CHAIN" = yes ]; then exit 7; fi
exec "$SSHWEAVE_TEST_BINARY" -c "$SSHWEAVE_TEST_CONFIG" connect --no-rewrite "$last"
"#,
    )?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!(
            r#"
[settings]
output="generated.conf"
known_hosts="known.json"
ssh_binary="{}"
[hosts.db]
hostname="db.internal"
gateway_strategy="chain"
gateways=["outer", "inner"]
[hosts.inner]
hostname="inner.internal"
port=2222
user="ops"
[hosts.outer]
proxy_command=["printf", 'SSH-2.0-chain\r\n']
"#,
            ssh.display()
        ),
    )?;
    let config = common::load(&path)?;
    sshweave::store::sync(&config, false).await?;
    let expanded = Command::new("ssh")
        .args(["-G", "-F"])
        .arg(config.output()?)
        .arg("inner/outer")
        .output()
        .await?;
    assert!(
        expanded.status.success(),
        "{}",
        String::from_utf8_lossy(&expanded.stderr)
    );
    let expanded = String::from_utf8(expanded.stdout)?;
    assert!(expanded.contains("hostname inner.internal\n"));
    assert!(expanded.contains("port 2222\n"));
    assert!(expanded.contains("user ops\n"));
    for fail in [false, true] {
        fs::write(&log, "")?;
        let output = timeout(
            Duration::from_secs(5),
            binary()
                .env("ROUTE_LOG", &log)
                .env("SSHWEAVE_TEST_BINARY", env!("CARGO_BIN_EXE_sshweave"))
                .env("SSHWEAVE_TEST_CONFIG", &path)
                .env("FAIL_CHAIN", if fail { "yes" } else { "no" })
                .arg("-c")
                .arg(&path)
                .args(["connect", "--no-rewrite", "db"])
                .stdin(Stdio::null())
                .output(),
        )
        .await??;
        let text = fs::read_to_string(&log)?;
        let attempts: Vec<_> = text.lines().collect();
        assert!(attempts[0].ends_with("-W db.internal:22 -- inner/outer"));
        if fail {
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(attempts.len(), 1, "must not bypass a failed chain");
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"SSH-2.0-chain\r\n");
            assert_eq!(attempts.len(), 2);
            assert!(attempts[1].ends_with("-W inner.internal:2222 -- outer"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn dry_run_exposes_effective_strategy_and_normalized_chain() -> Result<()> {
    for (strategy, expected) in [
        ("", vec!["outer", "inner"]),
        ("gateway_strategy='sequential'", vec!["outer", "inner"]),
        ("gateway_strategy='chain'", vec!["inner/outer"]),
    ] {
        let (dir, path) = fixture(&format!(
            "[hosts.db]\ngateways=['outer','inner']\n{strategy}"
        ))?;
        let output = binary()
            .arg("-c")
            .arg(path)
            .args(["connect", "--dry-run", "db"])
            .output()
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(value["routes"], serde_json::json!(expected));
        assert_eq!(
            value["host"]["gateway_strategy"],
            if strategy.contains("chain") {
                "chain"
            } else {
                "sequential"
            }
        );
        assert!(!dir.path().join("generated.conf").exists());
        assert!(!dir.path().join("known.json").exists());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn named_proxy_is_applied_only_to_the_local_first_hop_of_an_ssh_chain() -> Result<()> {
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = proxy.accept().await?;
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            anyhow::ensure!(header.len() < 4096, "unexpected oversized CONNECT request");
            header.push(stream.read_u8().await?);
        }
        assert_eq!(
            header,
            b"CONNECT outer.internal:2222 HTTP/1.1\r\nHost: outer.internal:2222\r\n\r\n"
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\n\r\nSSH-2.0-proxied-chain\r\n")
            .await?;
        let mut upload = Vec::new();
        stream.read_to_end(&mut upload).await?;
        stream.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    });
    let dir = tempfile::tempdir()?;
    let ssh = dir.path().join("fake-ssh");
    let log = dir.path().join("routes.log");
    executable(
        &ssh,
        r#"#!/bin/sh
for arg do last="$arg"; done
printf '%s|%s\n' "$SSHWEAVE_ROUTE_PROXY" "$*" >> "$ROUTE_LOG"
exec "$SSHWEAVE_TEST_BINARY" -c "$SSHWEAVE_TEST_CONFIG" connect --no-rewrite "$last"
"#,
    )?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!(
            r#"
[settings]
output="generated.conf"
known_hosts="known.json"
ssh_binary="{}"
[proxies.selected]
url="http://127.0.0.1:{proxy_port}"
[proxies.other]
url="http://127.0.0.1:1"
[hosts.db]
proxy="selected"
hostname="db.internal"
gateway_strategy="chain"
gateways=["outer", "inner"]
[hosts.inner]
hostname="inner.internal"
[hosts.outer]
hostname="outer.internal"
port=2222
proxy="other"
"#,
            ssh.display()
        ),
    )?;
    let config = common::load(&path)?;
    sshweave::store::sync(&config, false).await?;
    let output = timeout(
        Duration::from_secs(5),
        binary()
            .env("ROUTE_LOG", &log)
            .env("SSHWEAVE_TEST_BINARY", env!("CARGO_BIN_EXE_sshweave"))
            .env("SSHWEAVE_TEST_CONFIG", &path)
            .arg("-c")
            .arg(&path)
            .args(["connect", "--no-rewrite", "db"])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SSH-2.0-proxied-chain\r\n");
    let log = fs::read_to_string(log)?;
    let routes: Vec<_> = log.lines().collect();
    assert_eq!(routes.len(), 2);
    assert!(routes[0].ends_with("-W db.internal:22 -- inner/outer"));
    assert!(routes[1].ends_with("-W inner.internal:22 -- outer"));
    assert!(
        routes
            .iter()
            .all(|line| line.starts_with("selected|") && line.contains("-o ControlPath=none"))
    );
    timeout(Duration::from_secs(5), server).await???;
    Ok(())
}

#[tokio::test]
async fn ping_probes_the_first_hop_without_bypassing_a_proxy() -> Result<()> {
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_port = proxy.local_addr()?.port();
    let destination = TcpListener::bind("127.0.0.1:0").await?;
    let destination_port = destination.local_addr()?.port();
    let gateway = TcpListener::bind("127.0.0.1:0").await?;
    let gateway_port = gateway.local_addr()?.port();
    let (dir, path) = fixture(&format!(
        "[proxies.local]\nurl='http://127.0.0.1:{proxy_port}'\n\
         [hosts.jump]\nhostname='127.0.0.1'\nport={gateway_port}\n\
         [hosts.proxied]\nhostname='192.0.2.55'\nport={destination_port}\nproxy='local'\n\
         [hosts.behind]\nhostname='192.0.2.55'\nport={destination_port}\ngateways=['jump']\n\
         [hosts.custom]\nproxy_command=['true']"
    ))?;
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["ping", "--count", "1", "proxied"])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("via proxy local"), "{text}");
    assert!(text.contains(&proxy_port.to_string()), "{text}");
    assert!(
        timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
    let _ = proxy.accept().await?;
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["ping", "--count", "1", "behind"])
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("via gateway jump"), "{text}");
    let _ = gateway.accept().await?;
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["ping", "--count", "1", "custom"])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not start proxy_command"));
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["connect", "--dry-run", "proxied"])
        .output()
        .await?;
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["host"]["options"]["proxy"], "local");
    assert_eq!(value["routes"], serde_json::json!(["direct"]));
    assert!(
        timeout(Duration::from_millis(20), proxy.accept())
            .await
            .is_err()
    );
    assert!(!dir.path().join("generated.conf").exists());
    assert!(!dir.path().join("known.json").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn proxy_startup_timeout_includes_waiting_after_stdout_closes() -> Result<()> {
    for close_stdout in ["", "exec 1>&-"] {
        let dir = tempfile::tempdir()?;
        let ssh = dir.path().join("fake-ssh");
        executable(
            &ssh,
            &format!(
                "#!/bin/sh\nfor arg do last=\"$arg\"; done\nif [ \"$last\" = stalled ]; then\n{close_stdout}\nexec sleep 30\nfi\nprintf 'SSH-2.0-good\\r\\n'\ncat\n"
            ),
        )?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            format!(
                "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nssh_binary='{}'\n[hosts.db]\ngateways=['stalled','healthy']\ngateway_strategy='sequential'\nconnect_timeout=1",
                ssh.display()
            ),
        )?;
        let output = timeout(
            Duration::from_secs(5),
            binary()
                .arg("-c")
                .arg(path)
                .args(["connect", "--no-rewrite", "db"])
                .stdin(Stdio::null())
                .output(),
        )
        .await??;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"SSH-2.0-good\r\n");
        assert!(String::from_utf8_lossy(&output.stderr).contains("proxy startup timeout"));
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn custom_proxy_command_is_a_monitored_byte_transport() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("proxy");
    executable(&script, "#!/bin/sh\nprintf 'SSH-2.0-custom\\r\\n'\ncat\n")?;
    let (_config_dir, path) = fixture(&format!(
        "[hosts.db]\nproxy_command=['{}']",
        script.display()
    ))?;
    let output = timeout(
        Duration::from_secs(5),
        binary()
            .arg("-c")
            .arg(path)
            .args(["connect", "--no-rewrite", "db"])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SSH-2.0-custom\r\n");
    Ok(())
}

#[tokio::test]
async fn dry_run_has_no_network_or_filesystem_side_effects() -> Result<()> {
    let (dir, path) = fixture("[hosts.db]\nhostname='192.0.2.100'\nresolve_command=['false']")?;
    let output = binary()
        .arg("-c")
        .arg(path)
        .args(["connect", "--dry-run", "db"])
        .output()
        .await?;
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["host"]["hostname"], "192.0.2.100");
    assert!(!dir.path().join("generated.conf").exists());
    assert!(!dir.path().join("known.json").exists());
    Ok(())
}

#[tokio::test]
async fn real_openssh_accepts_generated_configuration_and_invokes_proxy() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"SSH-2.0-sshweave-test\r\n").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let (_dir, path) = fixture(&format!(
        "[hosts.test]\nhostname='127.0.0.1'\nport={port}\nuser='fixture'"
    ))?;
    let mut config = common::load(&path)?;
    config.document.settings.binary = Some(env!("CARGO_BIN_EXE_sshweave").into());
    let control_path = format!(
        "{}/control/%C",
        path.parent().context("missing parent")?.display()
    );
    config
        .document
        .hosts
        .get_mut("test")
        .context("host missing")?
        .ssh
        .insert(
            "ControlPath".into(),
            sshweave::config::SshValue::Text(format!("\"{control_path}\"")),
        );
    sshweave::store::sync(&config, false).await?;
    let expanded = Command::new("ssh")
        .arg("-G")
        .arg("-F")
        .arg(config.output()?)
        .arg("test")
        .output()
        .await?;
    assert!(
        expanded.status.success(),
        "{}",
        String::from_utf8_lossy(&expanded.stderr)
    );
    let text = String::from_utf8(expanded.stdout)?;
    assert!(text.contains("hostname 127.0.0.1\n"));
    assert!(text.contains("user fixture\n"));
    let control = text
        .lines()
        .find_map(|l| l.strip_prefix("controlpath "))
        .context("ControlPath missing")?;
    assert_eq!(control, config.resolve("test")?.expand(&control_path, "")?);
    let proxy = text
        .lines()
        .find_map(|l| l.strip_prefix("proxycommand "))
        .context("proxy missing")?
        .replace("%n", "test")
        .replace("%p", &port.to_string())
        .replace("%r", "fixture");
    let output = timeout(
        Duration::from_secs(5),
        Command::new("/bin/sh")
            .arg("-c")
            .arg(proxy)
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"SSH-2.0-sshweave-test\r\n");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn ping_reports_success_and_failure_with_correct_exit_codes() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (_dir, path) = fixture(&format!("[hosts.test]\nhostname='127.0.0.1'\nport={port}"))?;
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["ping", "--count", "1", "test"])
        .output()
        .await?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 transmitted, 1 received"));
    drop(listener);
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["ping", "--count", "1", "test"])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("100.0% loss"));
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["ping", "--count", "0", "--interval", "0", "test"])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("--count 0 requires --interval"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn wrapper_and_alias_materialize_targets_and_preserve_arguments_and_exit_status() -> Result<()>
{
    for entry in ["wrapper", "alias"] {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("ssh");
        executable(
            &script,
            "#!/bin/sh\n[ \"$1\" = '-F' ] || exit 1\ngrep -q 'Host db7/gateway' \"$2\" || exit 2\nshift 2\nprintf '%s\\n' \"$@\"\nexit 17\n",
        )?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            format!(
                r#"
[settings]
output="generated.conf"
known_hosts="known.json"
ssh_binary="{}"
[hosts."regex:^db([0-9]+)$"]
hostname="node-{{1}}.internal"
"#,
                script.display()
            ),
        )?;
        let mut command = if entry == "alias" {
            let mut command = Command::new("/bin/sh");
            command
                .kill_on_drop(true)
                .env("SSHWEAVE_TEST_BINARY", env!("CARGO_BIN_EXE_sshweave"))
                .env("SSHWEAVE_CONFIG", &path)
                .args([
                    "-c",
                    "alias ssh='\"$SSHWEAVE_TEST_BINARY\" wrapper ssh'\nssh \"$@\"",
                    "sshweave-alias-test",
                ]);
            command
        } else {
            let mut command = binary();
            command.arg("-c").arg(&path).args(["wrapper", "ssh"]);
            command
        };
        let output = timeout(
            Duration::from_secs(5),
            command
                .args([
                    "-v",
                    "-p",
                    "2222",
                    "db7/gateway",
                    "printf",
                    "%s",
                    "hello world",
                ])
                .output(),
        )
        .await??;
        assert_eq!(
            output.status.code(),
            Some(17),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)?,
            "-v\n-p\n2222\ndb7/gateway\nprintf\n%s\nhello world\n",
            "{entry}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn socket_discovery_ignores_regular_files() -> Result<()> {
    use std::os::unix::net::UnixListener;
    let (dir, path) = fixture("[defaults.ssh]\nControlPath='controls/%h-%p'")?;
    fs::create_dir(dir.path().join("controls"))?;
    fs::write(dir.path().join("controls/not-a-socket"), "regular")?;
    let socket_path = dir.path().join("controls/host-22");
    let _listener = UnixListener::bind(&socket_path)?;
    let mut config = common::load(&path)?;
    config.document.defaults.ssh.insert(
        "ControlPath".into(),
        sshweave::config::SshValue::Text(format!(
            "'{}/controls/%h-%p'",
            dir.path().canonicalize()?.display()
        )),
    );
    assert_eq!(
        sshweave::sockets::paths(&config)?,
        vec![socket_path.canonicalize()?]
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn socket_discovery_handles_registered_slash_chain_directories() -> Result<()> {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir_in("/tmp")?;
    let source = dir.path().join("config.toml");
    fs::write(
        &source,
        format!(
            r#"
[settings]
known_hosts="known.json"
output="generated.conf"
[defaults.ssh]
ControlPath="'{}/%n/socket-%p'"
"#,
            dir.path().display()
        ),
    )?;
    let config = common::load(source)?;
    sshweave::store::remember(&config, &["db/gateway".into()])?;
    let parent = dir.path().join("db/gateway");
    fs::create_dir_all(&parent)?;
    let socket_path = parent.join("socket-22");
    let _socket = UnixListener::bind(&socket_path)?;
    assert_eq!(sshweave::sockets::paths(&config)?, vec![socket_path]);
    Ok(())
}

#[cfg(unix)]
#[test]
fn socket_discovery_treats_path_metacharacters_literally() -> Result<()> {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir_in("/tmp")?;
    let parent = dir.path().join("controls[1]*?");
    fs::create_dir(&parent)?;
    let socket_path = parent.join("test-%-22");
    let _socket = UnixListener::bind(&socket_path)?;
    let unrelated = dir.path().join("controls1xy");
    fs::create_dir(&unrelated)?;
    let _unrelated_socket = UnixListener::bind(unrelated.join("test-%-22"))?;
    let source = dir.path().join("config.toml");
    fs::write(
        &source,
        format!(
            "[hosts.test.ssh]\nControlPath=\"'{}/%g%n-%%-%p'\"",
            parent.display()
        ),
    )?;
    let config = common::load(source)?;
    assert_eq!(sshweave::sockets::paths(&config)?, vec![socket_path]);
    Ok(())
}

#[tokio::test]
async fn openssh_does_not_duplicate_additive_options_from_defaults_and_wildcards() -> Result<()> {
    let (_dir, path) = fixture(
        r#"
[defaults.ssh]
IdentityFile=["~/.ssh/base"]
LocalForward=["8080 localhost:80"]
[hosts."*.dev"]
user="dev"
[hosts."db.dev".ssh]
IdentityFile=["~/.ssh/specific"]
LocalForward=["9090 localhost:90"]
"#,
    )?;
    let config = common::load(&path)?;
    sshweave::store::sync(&config, false).await?;
    let output = Command::new("ssh")
        .arg("-G")
        .arg("-F")
        .arg(config.output()?)
        .arg("db.dev")
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout)?;
    assert_eq!(
        output
            .lines()
            .filter(|line| line.starts_with("identityfile "))
            .collect::<Vec<_>>(),
        vec!["identityfile ~/.ssh/specific"]
    );
    let forwards = output
        .lines()
        .filter(|line| line.starts_with("localforward "))
        .collect::<Vec<_>>();
    assert_eq!(forwards.len(), 1);
    assert!(forwards[0].contains("9090"));
    Ok(())
}

#[tokio::test]
async fn doctor_and_search_report_integration_and_resolved_names() -> Result<()> {
    let home = tempfile::tempdir()?;
    let (dir, path) = fixture(
        "[proxies.office]\nurl='http://127.0.0.1:9'\n\
         [templates.production]\nuser='deploy'\n\
         [defaults]\nuser='fallback'\n\
         [hosts.db]\ninherits=['production']\nhostname='10.1.0.5'\naliases=['database']\n\
         user='dbuser'\ncomment='primary'\nproxy='office'\nrate_limit='5 MiB/s'\n\
         gateways=['bastion']\ngateway_strategy='sequential'\n",
    )?;
    let run = |args: &[&str]| {
        let mut command = binary();
        command
            .env("HOME", home.path())
            .env_remove("SSHWEAVE_CONFIG")
            .arg("-c")
            .arg(&path)
            .args(args);
        command
    };
    for query in [
        "production",
        "office",
        "database",
        "primary",
        "10.1.0.5",
        "bastion",
        "sequential",
    ] {
        let output = run(&["config", "search", query]).output().await?;
        assert!(
            output.status.success(),
            "{query}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("db ->"),
            "{query}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let explained = String::from_utf8(
        run(&["config", "explain", "database"])
            .output()
            .await?
            .stdout,
    )?;
    assert!(explained.contains("matched: host db"), "{explained}");
    assert!(explained.contains("hostname: 10.1.0.5"), "{explained}");
    assert!(
        explained.contains("  from host db")
            && explained.contains("  hidden template production: deploy"),
        "{explained}"
    );
    assert!(
        explained.contains("  hidden defaults: fallback"),
        "{explained}"
    );
    assert!(
        explained.contains("combined send and receive, this connection only"),
        "{explained}"
    );
    let chained = String::from_utf8(
        run(&["config", "explain", "database/bastion"])
            .output()
            .await?
            .stdout,
    )?;
    assert!(
        chained.contains("explicit chain overrides configured gateways"),
        "{chained}"
    );

    let listed = String::from_utf8(run(&["config", "list"]).output().await?.stdout)?;
    assert!(listed.contains("database -> db"), "{listed}");
    assert!(
        listed.contains("combined send and receive, this connection only"),
        "{listed}"
    );

    let output = run(&["doctor"]).output().await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("GIT_SSH_COMMAND="), "{text}");
    assert!(text.contains("not written"), "{text}");
    assert!(text.contains("Host * ProxyCommand"), "{text}");

    let built = run(&["config", "build", "--write"]).output().await?;
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(String::from_utf8_lossy(&built.stderr).contains("Host * ProxyCommand"));
    let mut source = fs::read_to_string(&path)?;
    source.push_str("\n[hosts.extra]\nhostname='10.9.9.9'\n");
    fs::write(&path, source)?;
    let rebuilt = run(&["config", "build", "--write"]).output().await?;
    assert!(rebuilt.status.success());
    assert!(
        String::from_utf8_lossy(&rebuilt.stderr).contains("+ Host extra"),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    let generated = dir.path().join("generated.conf").canonicalize()?;
    let ssh_dir = home.path().join(".ssh");
    fs::create_dir_all(&ssh_dir)?;
    let include = format!("'{}'", generated.display());
    fs::write(
        ssh_dir.join("config"),
        format!("Host *\n  User x\n\nInclude {include}\n"),
    )?;
    let output = run(&["doctor"]).output().await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("after a Host or Match block"));
    fs::write(
        ssh_dir.join("config"),
        format!("Include {include}\n\nHost other\n  User x\n"),
    )?;
    let output = run(&["doctor"]).output().await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn hangup_from_openssh_ends_relay_and_runs_disconnect_hooks() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"banner").await?;
        let mut data = Vec::new();
        socket.read_to_end(&mut data).await?;
        Ok::<_, std::io::Error>(())
    });
    let (_dir, path) = fixture(&format!(
        r#"
[hosts.test]
hostname="127.0.0.1"
port={port}
[hosts.test.hooks]
on_disconnect=[{{driver="write", message="disconnected error={{{{ error }}}} received={{{{ stats.received_bytes }}}}"}}]
"#
    ))?;
    let mut child = binary()
        .arg("-c")
        .arg(&path)
        .args(["connect", "--no-rewrite", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let _held_stdin = child.stdin.take();
    let mut stdout = child.stdout.take().context("stdout missing")?;
    let mut banner = [0; 6];
    timeout(Duration::from_secs(5), stdout.read_exact(&mut banner)).await??;
    assert_eq!(&banner, b"banner");
    // OpenSSH sends SIGHUP to its ProxyCommand when the client exits.
    let pid = child.id().context("pid missing")?.to_string();
    assert!(
        Command::new("kill")
            .args(["-HUP", &pid])
            .status()
            .await?
            .success()
    );
    let status = timeout(Duration::from_secs(5), child.wait()).await??;
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .context("stderr missing")?
        .read_to_string(&mut stderr)
        .await?;
    assert!(status.success(), "{status:?}: {stderr}");
    assert!(
        stderr.contains("disconnected error=None received=6"),
        "{stderr}"
    );
    timeout(Duration::from_secs(5), server).await???;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failing_connect_error_hook_does_not_stop_route_fallback() -> Result<()> {
    let closed = TcpListener::bind("127.0.0.1:0").await?;
    let port = closed.local_addr()?.port();
    drop(closed);
    let directory = tempfile::tempdir()?;
    let ssh = directory.path().join("ssh");
    executable(&ssh, "#!/bin/sh\nprintf 'SSH-2.0-fallback\\r\\n'\ncat\n")?;
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nssh_binary='{}'\n\
             [hosts.db]\nhostname='127.0.0.1'\nport={port}\ngateways=['direct','healthy']\n\
             [hosts.db.hooks]\non_connect_error=[{{driver='exec',command='exit 3'}}]\n",
            ssh.display()
        ),
    )?;
    let output = timeout(
        Duration::from_secs(5),
        binary()
            .arg("-c")
            .arg(path)
            .args(["connect", "--no-rewrite", "db"])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    assert_eq!(output.stdout, b"SSH-2.0-fallback\r\n");
    assert!(stderr.contains("connect error hook failed"), "{stderr}");
    Ok(())
}

#[tokio::test]
async fn nested_hops_skip_configuration_rewrites() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"banner").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let (dir, path) = fixture(&format!("[hosts.test]\nhostname='127.0.0.1'\nport={port}"))?;
    let outer = binary()
        .arg("-c")
        .arg(&path)
        .args(["connect", "test"])
        .stdin(Stdio::null())
        .output()
        .await?;
    assert!(!outer.status.success());
    assert!(String::from_utf8(outer.stderr)?.contains("configuration updated"));
    fs::remove_file(dir.path().join("generated.conf"))?;

    let nested = timeout(
        Duration::from_secs(5),
        binary()
            .env("SSHWEAVE_ROUTE_STACK", r#"["outer"]"#)
            .arg("-c")
            .arg(&path)
            .args(["connect", "test"])
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    assert!(nested.status.success(), "{nested:?}");
    assert_eq!(nested.stdout, b"banner");
    assert!(!dir.path().join("generated.conf").exists());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn direct_dial_honors_address_family_and_bind_address() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, peer) = listener.accept().await?;
        socket.write_all(b"banner").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(peer)
    });
    let connect = |ssh: &str| {
        let (dir, path) = fixture(&format!(
            "[hosts.test]\nhostname='127.0.0.1'\nport={port}\n[hosts.test.ssh]\n{ssh}"
        ))
        .expect("fixture");
        let mut command = binary();
        command
            .arg("-c")
            .arg(&path)
            .args(["connect", "--no-rewrite", "test"])
            .stdin(Stdio::null());
        (dir, command)
    };
    let (_dir, mut command) = connect("AddressFamily='inet6'\n");
    let output = timeout(Duration::from_secs(5), command.output()).await??;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("no address allowed by AddressFamily"));

    let (_dir, mut command) = connect("BindAddress='::1'\n");
    let output = timeout(Duration::from_secs(5), command.output()).await??;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("BindAddress has no address for"));

    let (_dir, mut command) = connect("AddressFamily='inet'\nBindAddress='127.0.0.1'\n");
    let output = timeout(Duration::from_secs(5), command.output()).await??;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"banner");
    assert!(server.await??.ip().is_loopback());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn wrapper_uses_configured_clients_and_preserves_signal_exits() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let directory = tempfile::tempdir()?;
    let scp = directory.path().join("custom-scp");
    executable(&scp, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nkill -TERM $$\n")?;
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nscp_binary='{}'\n",
            scp.display()
        ),
    )?;
    let output = timeout(
        Duration::from_secs(5),
        binary()
            .arg("-c")
            .arg(&path)
            .args(["wrapper", "scp", "file", "db:/tmp/"])
            .output(),
    )
    .await??;
    assert_eq!(output.status.signal(), Some(15), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("-F\n"), "{stdout}");
    assert!(stdout.ends_with("file\ndb:/tmp/\n"), "{stdout}");
    assert!(!directory.path().join("known.json").exists());
    Ok(())
}

#[tokio::test]
async fn explain_reports_every_matching_rule() -> Result<()> {
    let (_dir, path) = fixture(
        "[hosts.\"*.prod\"]\nuser='produser'\n[hosts.\"db*\"]\nport=2022\n[hosts.\"db*\".ssh]\nIdentityFile='~/.ssh/db'\n\
         [hosts.\"regex:^(db[a-z]+)\\\\.prod$\"]\nhostname='{1}.internal'\n",
    )?;
    let output = binary()
        .arg("-c")
        .arg(&path)
        .args(["config", "explain", "dbx.prod"])
        .output()
        .await?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(output.status.success(), "{stdout}");
    for expected in [
        "dbx.prod -> produser@dbx.internal:2022\n",
        "matched: host *.prod\nmatched: host db*\nmatched: host regex:",
        "hostname: {1}.internal\n  from host regex:^(db[a-z]+)\\.prod$\n  resolved as dbx.internal\n",
        "port: 2022\n  from host db*\n",
        "ssh.IdentityFile: ~/.ssh/db\n  from host db*\n",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    assert!(!stdout.contains("built-in default"), "{stdout}");
    Ok(())
}

#[tokio::test]
async fn rendering_inputs_include_referenced_environment_variables() -> Result<()> {
    let (_dir, path) = fixture("[hosts.db]\nhostname='${SSHWEAVE_TEST_REGION}.example.net'\n")?;
    for (region, expected) in [
        ("east", "Updated"),
        ("east", "Unchanged"),
        ("west", "Updated"),
    ] {
        let output = binary()
            .env("SSHWEAVE_TEST_REGION", region)
            .arg("-c")
            .arg(&path)
            .args(["config", "build", "--write"])
            .output()
            .await?;
        let stderr = String::from_utf8(output.stderr)?;
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains(expected), "{region}: {stderr}");
    }
    Ok(())
}

/// Returns whether a process with `pid` still exists.
#[cfg(unix)]
async fn process_alive(pid: &str) -> Result<bool> {
    Ok(Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .await?
        .success())
}

/// Waits until `path` holds a process ID written by a test helper.
#[cfg(unix)]
async fn wait_for_pid(path: &std::path::Path) -> Result<String> {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = fs::read_to_string(path)
                && !pid.trim().is_empty()
            {
                return pid.trim().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("helper did not record its PID")
}

#[cfg(unix)]
#[tokio::test]
async fn hook_timeouts_and_warn_policy_bound_slow_or_failing_hooks() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"banner").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(())
    });
    let run = |hooks: &str| {
        let (dir, path) = fixture(&format!(
            "[hosts.test]\nhostname='127.0.0.1'\nport={port}\n[hosts.test.hooks]\n{hooks}"
        ))
        .expect("fixture");
        let mut command = binary();
        command
            .arg("-c")
            .arg(&path)
            .args(["connect", "--no-rewrite", "test"])
            .stdin(Stdio::null());
        (dir, command)
    };
    let (_dir, mut command) =
        run("before_connect=[{driver='exec', command='sleep 10', timeout=1}]\n");
    let started = Instant::now();
    let output = timeout(Duration::from_secs(5), command.output()).await??;
    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(String::from_utf8(output.stderr)?.contains("hook timed out after 1s"));

    let (_dir, mut command) = run(
        "before_connect=[{driver='exec', command='exit 4', on_error='warn'}, \
         {driver='write', message='still running'}]\n",
    );
    let output = timeout(Duration::from_secs(5), command.output()).await??;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    assert_eq!(output.stdout, b"banner");
    assert!(stderr.contains("hook failed; continuing"), "{stderr}");
    assert!(stderr.contains("still running"), "{stderr}");
    server.await??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn signals_interrupt_slow_disconnect_hooks_and_kill_their_commands() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"banner").await?;
        let mut data = Vec::new();
        socket.read_to_end(&mut data).await?;
        Ok::<_, std::io::Error>(())
    });
    let (dir, path) = fixture(&format!(
        "[hosts.test]\nhostname='127.0.0.1'\nport={port}\n[hosts.test.hooks]\n\
         on_disconnect=[{{driver='exec', timeout=0, command='echo $$ > hook.pid; exec sleep 30'}}]\n"
    ))?;
    let mut child = binary()
        .current_dir(dir.path())
        .arg("-c")
        .arg(&path)
        .args(["connect", "--no-rewrite", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let _held_stdin = child.stdin.take();
    let mut stdout = child.stdout.take().context("stdout missing")?;
    let mut banner = [0; 6];
    timeout(Duration::from_secs(5), stdout.read_exact(&mut banner)).await??;
    let pid = child.id().context("pid missing")?.to_string();
    assert!(
        Command::new("kill")
            .args(["-HUP", &pid])
            .status()
            .await?
            .success()
    );
    let hook_pid = wait_for_pid(&dir.path().join("hook.pid")).await?;
    assert!(process_alive(&hook_pid).await?);
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await?
            .success()
    );
    let status = timeout(Duration::from_secs(5), child.wait()).await??;
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .context("stderr missing")?
        .read_to_string(&mut stderr)
        .await?;
    assert!(!status.success());
    assert!(
        stderr.contains("SIGTERM received during on_disconnect hooks"),
        "{stderr}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!process_alive(&hook_pid).await?, "hook command survived");
    timeout(Duration::from_secs(5), server).await???;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn signals_during_transport_setup_kill_transport_processes() -> Result<()> {
    let (dir, path) = fixture(
        "[hosts.test]\nhostname='127.0.0.1'\nconnect_timeout=0\n\
         proxy_command=['sh', '-c', 'echo $$ > proxy.pid; exec sleep 30']\n",
    )?;
    let mut child = binary()
        .current_dir(dir.path())
        .arg("-c")
        .arg(&path)
        .args(["connect", "--no-rewrite", "test"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let proxy_pid = wait_for_pid(&dir.path().join("proxy.pid")).await?;
    let pid = child.id().context("pid missing")?.to_string();
    assert!(
        Command::new("kill")
            .args(["-HUP", &pid])
            .status()
            .await?
            .success()
    );
    let status = timeout(Duration::from_secs(5), child.wait()).await??;
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .context("stderr missing")?
        .read_to_string(&mut stderr)
        .await?;
    assert!(!status.success());
    assert!(
        stderr.contains("SIGHUP received during transport setup"),
        "{stderr}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!process_alive(&proxy_pid).await?, "proxy_command survived");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn scp_and_sftp_wrappers_use_the_configured_ssh_binary() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let client = directory.path().join("client");
    executable(&client, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n")?;
    let ssh = directory.path().join("custom-ssh");
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nssh_binary='{}'\n\
             scp_binary='{client}'\nsftp_binary='{client}'\n",
            ssh.display(),
            client = client.display()
        ),
    )?;
    let run = |args: &[&str]| {
        let mut command = binary();
        command.arg("-c").arg(&path).arg("wrapper").args(args);
        command
    };
    for args in [&["scp", "file", "db:/tmp/"][..], &["sftp", "db"][..]] {
        let output = timeout(Duration::from_secs(5), run(args).output()).await??;
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            stdout.contains(&format!("-S\n{}\n", ssh.display())),
            "{args:?}: {stdout}"
        );
    }
    let output = timeout(
        Duration::from_secs(5),
        run(&["scp", "-S", "/usr/bin/other", "file", "db:/tmp/"]).output(),
    )
    .await??;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(!stdout.contains(&ssh.display().to_string()), "{stdout}");
    assert!(stdout.contains("-S\n/usr/bin/other\n"), "{stdout}");
    Ok(())
}

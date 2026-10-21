use anyhow::{Context, Result};
use sshweave::{
    config::Config,
    runtime::{self, RateLimiter},
};
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
    command.kill_on_drop(true);
    command
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
    let config = Config::load(&path)?;
    let mut host = config.resolve("test")?;
    runtime::prepare(&mut host, "").await?;
    assert_eq!(host.hostname, "127.0.0.1");
    let (_dir, path) = fixture("[hosts.test]\nresolve_command=['printf', 'two hosts']")?;
    let config = Config::load(&path)?;
    assert!(
        runtime::prepare(&mut config.resolve("test")?, "")
            .await
            .is_err()
    );
    let (_dir, path) = fixture("[hosts.test]\nresolve_command=['false']")?;
    let config = Config::load(&path)?;
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
    let mut config = Config::load(&path)?;
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
                "[settings]\noutput='generated.conf'\nknown_hosts='known.json'\nssh_binary='{}'\n[hosts.db]\ngateways=['stalled','healthy']\nconnect_timeout=1",
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
    let mut config = Config::load(&path)?;
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
    let mut config = Config::load(&path)?;
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
    let config = Config::load(source)?;
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
    let config = Config::load(source)?;
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
    let config = Config::load(&path)?;
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

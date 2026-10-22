//! HTTP CONNECT and SOCKS5 proxy transports.

mod common;

use anyhow::{Context, Result};
use sshweave::proxy::Endpoint;
use std::{fs, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    time::timeout,
};

async fn request_header(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(header.len() < 4096, "oversized test request");
        header.push(stream.read_u8().await?);
    }
    Ok(header)
}

async fn connect(config: &str, payload: &[u8]) -> Result<std::process::Output> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    fs::write(
        &path,
        format!("[settings]\noutput='generated.conf'\nknown_hosts='known.json'\n{config}"),
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_sshweave"))
        .env("HOME", common::isolated_home())
        .arg("-c")
        .arg(&path)
        .args(["connect", "--no-rewrite", "db"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing proxy test stdin")?;
    stdin.write_all(payload).await?;
    drop(stdin);
    Ok(timeout(Duration::from_secs(5), child.wait_with_output()).await??)
}

#[test]
fn proxy_endpoints_parse_ipv4_ipv6_and_dns_without_implicit_defaults() -> Result<()> {
    assert!(Endpoint::parse("direct").is_err());
    assert!(Endpoint::parse("bastion").is_err());
    for (url, host, port) in [
        ("http://127.0.0.1:7890", "127.0.0.1", 7890),
        ("socks5://localhost:1080/", "localhost", 1080),
        ("http://[::1]:8080", "::1", 8080),
    ] {
        let endpoint = Endpoint::parse(url)?;
        assert_eq!(endpoint.host, host);
        assert_eq!(endpoint.port, port);
    }
    Ok(())
}

#[test]
fn invalid_proxy_urls_are_rejected_before_connecting() -> Result<()> {
    for url in [
        "https://localhost:7890",
        "socks4://localhost:1080",
        "proxy+http://localhost:7890",
        "http://localhost",
        "http://localhost:0",
        "http://localhost:65536",
        "http://user:password@localhost:7890",
        "http://localhost:7890/path",
        "http://localhost:7890?query",
        "http://localhost:7890#fragment",
        "http://::1:7890",
        "http://[bad]:7890",
        "http://localhost:+7890",
        "http://localhost:7890//",
        "http://local host:7890",
        "http://localhost:7890\r\n",
        "http://localhost:7890\0",
    ] {
        assert!(Endpoint::parse(url).is_err(), "accepted invalid proxy URL");
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("config.toml");
        fs::write(
            &source,
            format!("[proxies.invalid]\nurl={}", serde_json::to_string(url)?),
        )?;
        assert!(
            common::load(&source).is_err(),
            "config check accepted invalid proxy"
        );
    }
    Ok(())
}
#[tokio::test]
async fn http_connect_preserves_coalesced_banner_binary_data_and_half_close() -> Result<()> {
    for (hostname, authority) in [
        ("remote.invalid", "remote.invalid:2222"),
        ("2001:db8::1", "[2001:db8::1]:2222"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_port = listener.local_addr()?.port();
        let payload: Vec<u8> = (0..=255).collect();
        let expected = payload.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            assert_eq!(
                request_header(&mut stream).await?,
                format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes()
            );
            stream
                .write_all(
                    b"HTTP/1.1 200 Connection established\r\nX-Test: yes\r\n\r\nSSH-2.0-http\r\n",
                )
                .await?;
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await?;
            assert_eq!(received, expected);
            stream.write_all(&received).await?;
            stream.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let output = connect(
        &format!("[proxies.local]\nurl='http://127.0.0.1:{proxy_port}'\n[hosts.db]\nproxy='local'\nhostname='{hostname}'\nport=2222"),
        &payload,
    ).await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            [b"SSH-2.0-http\r\n".as_slice(), &payload].concat()
        );
        timeout(Duration::from_secs(5), server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn socks5_supports_remote_dns_ipv4_ipv6_and_all_bound_address_types() -> Result<()> {
    for (hostname, address, bound) in [
        (
            "remote.invalid",
            [vec![3, 14], b"remote.invalid".to_vec()].concat(),
            vec![3, 3, b'b', b'n', b'd'],
        ),
        ("192.0.2.1", vec![1, 192, 0, 2, 1], vec![1, 127, 0, 0, 1]),
        (
            "2001:db8::1",
            [
                vec![4],
                "2001:db8::1"
                    .parse::<std::net::Ipv6Addr>()?
                    .octets()
                    .to_vec(),
            ]
            .concat(),
            [vec![4], vec![0; 16]].concat(),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_port = listener.local_addr()?.port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await?;
            assert_eq!(greeting, [5, 1, 0]);
            stream.write_all(&[5, 0]).await?;
            let expected = [vec![5, 1, 0], address, 2222_u16.to_be_bytes().to_vec()].concat();
            let mut request = vec![0; expected.len()];
            stream.read_exact(&mut request).await?;
            assert_eq!(request, expected);
            let reply = [
                vec![5, 0, 0],
                bound,
                vec![0, 0],
                b"SSH-2.0-socks\r\n".to_vec(),
            ]
            .concat();
            for byte in reply {
                stream.write_all(&[byte]).await?;
            }
            let mut data = Vec::new();
            stream.read_to_end(&mut data).await?;
            assert_eq!(data, b"\0hello\xff");
            stream.write_all(&data).await?;
            stream.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let output = connect(
            &format!("[proxies.local]\nurl='socks5://127.0.0.1:{proxy_port}'\n[hosts.db]\nproxy='local'\nhostname='{hostname}'\nport=2222"),
            b"\0hello\xff",
        ).await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"SSH-2.0-socks\r\n\0hello\xff");
        timeout(Duration::from_secs(5), server).await???;
    }
    Ok(())
}

#[tokio::test]
async fn malformed_or_rejected_proxy_handshakes_fail_without_bypassing_proxy() -> Result<()> {
    for (scheme, response, expected_error) in [
        (
            "http",
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n".to_vec(),
            "status 407",
        ),
        ("http", b"invalid 200\r\n\r\n".to_vec(), "response version"),
        (
            "http",
            b"HTTP/1.1 200\rbroken\r\n\r\n".to_vec(),
            "status line",
        ),
        (
            "http",
            b"HTTP/1.1 abc Broken\r\n\r\n".to_vec(),
            "status code",
        ),
        ("http", vec![b'x'; 16 * 1024 + 1], "headers exceed 16 KiB"),
        (
            "http",
            b"HTTP/1.1 200 OK\r\n".to_vec(),
            "read HTTP CONNECT response",
        ),
        ("socks5", vec![5, 0xff], "no-authentication"),
        ("socks5", vec![5, 2], "no-authentication"),
        ("socks5", vec![5, 0, 5, 5, 0, 1], "status 5"),
        ("socks5", vec![5, 0, 4, 0, 0, 1], "invalid SOCKS5 CONNECT"),
        ("socks5", vec![5, 0, 5, 0, 1, 1], "invalid SOCKS5 CONNECT"),
        ("socks5", vec![5, 0, 5, 0, 0, 9], "bound address type"),
        (
            "socks5",
            vec![5, 0, 5, 0, 0, 3, 0],
            "empty SOCKS5 bound hostname",
        ),
        ("socks5", vec![5, 0, 5, 0, 0, 1, 127], "bound address"),
    ] {
        let proxy = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_port = proxy.local_addr()?.port();
        let destination = TcpListener::bind("127.0.0.1:0").await?;
        let destination_port = destination.local_addr()?.port();
        let bad = tokio::spawn(async move {
            let (mut stream, _) = proxy.accept().await?;
            if scheme == "http" {
                request_header(&mut stream).await?;
                stream.write_all(&response).await?;
            } else {
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).await?;
                stream.write_all(&response[..2]).await?;
                if response.len() > 2 {
                    let mut request = [0; 10];
                    stream.read_exact(&mut request).await?;
                    stream.write_all(&response[2..]).await?;
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        let output = connect(
            &format!("[proxies.local]\nurl='{scheme}://127.0.0.1:{proxy_port}'\n[hosts.db]\nproxy='local'\nhostname='127.0.0.1'\nport={destination_port}\ngateways=['direct']"),
            b"",
        ).await?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        timeout(Duration::from_secs(5), bad).await???;
        assert!(
            timeout(Duration::from_millis(20), destination.accept())
                .await
                .is_err(),
            "proxy failure must not open a direct connection"
        );
    }
    Ok(())
}

#[tokio::test]
async fn proxy_timeout_covers_handshake_and_destination_banner() -> Result<()> {
    for send_handshake in [false, true] {
        let proxy = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_port = proxy.local_addr()?.port();
        let stalled = tokio::spawn(async move {
            let (mut stream, _) = proxy.accept().await?;
            request_header(&mut stream).await?;
            if send_handshake {
                stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await?;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(stream);
            Ok::<_, anyhow::Error>(())
        });
        let output = connect(
            &format!("[proxies.local]\nurl='http://127.0.0.1:{proxy_port}'\n[hosts.db]\nproxy='local'\nhostname='remote.invalid'\nconnect_timeout=1"),
            b"",
        ).await?;
        stalled.abort();
        assert!(stalled.await.unwrap_err().is_cancelled());
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("proxy startup timeout"));
    }
    Ok(())
}

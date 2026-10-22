//! Unauthenticated HTTP CONNECT and SOCKS5 proxy support.
//!
//! The destination hostname is sent to the proxy unresolved, so DNS happens on
//! the proxy side.

use crate::util;
use anyhow::{Context, Result, bail, ensure};
use std::net::{IpAddr, Ipv6Addr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Maximum accepted size of HTTP CONNECT response headers.
const MAX_HTTP_HEADER: usize = 16 * 1024;

/// SOCKS protocol version byte.
const SOCKS_VERSION: u8 = 5;
/// SOCKS5 "no authentication required" method.
const SOCKS_NO_AUTH: u8 = 0;
/// SOCKS5 CONNECT command.
const SOCKS_CONNECT: u8 = 1;
/// SOCKS5 address types.
const SOCKS_ATYP_IPV4: u8 = 1;
const SOCKS_ATYP_DOMAIN: u8 = 3;
const SOCKS_ATYP_IPV6: u8 = 4;

/// Proxy protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP `CONNECT` tunnel.
    Http,
    /// SOCKS5 `CONNECT` without authentication.
    Socks5,
}

/// A parsed proxy URL.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Proxy protocol.
    pub protocol: Protocol,
    /// Proxy host name or IP address (IPv6 without brackets).
    pub host: String,
    /// Proxy port.
    pub port: u16,
}

impl Endpoint {
    /// Parses `http://host:port` or `socks5://host:port`.
    ///
    /// Credentials, paths, queries, and fragments are rejected; an explicit
    /// port is required.
    pub fn parse(url: &str) -> Result<Self> {
        ensure!(url.len() <= 1024, "proxy URL is too long");
        let (scheme, authority) = url
            .split_once("://")
            .context("proxy URL requires scheme://host:port")?;
        let protocol = match scheme {
            "http" => Protocol::Http,
            "socks5" => Protocol::Socks5,
            _ => bail!("unsupported proxy protocol; expected http or socks5"),
        };
        let authority = authority.strip_suffix('/').unwrap_or(authority);
        ensure!(
            !authority.contains(['/', '?', '#', '@', '%', '\\'])
                && !authority.chars().any(char::is_whitespace),
            "proxy URL must contain only host:port; authentication, paths, queries and fragments are not supported"
        );
        let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
            let (host, port) = ipv6
                .split_once("]:")
                .context("IPv6 proxy requires [address]:port")?;
            host.parse::<Ipv6Addr>()
                .context("invalid IPv6 proxy address")?;
            (host, port)
        } else {
            let (host, port) = authority
                .rsplit_once(':')
                .context("proxy URL requires an explicit port")?;
            ensure!(
                !host.is_empty()
                    && !host.starts_with('-')
                    && host
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
                "invalid proxy hostname"
            );
            (host, port)
        };
        ensure!(
            !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()),
            "invalid proxy port"
        );
        let port: u16 = port.parse().context("invalid proxy port")?;
        ensure!(port > 0, "proxy port must be positive");
        Ok(Self {
            protocol,
            host: host.into(),
            port,
        })
    }

    /// Asks the proxy, over an already connected `stream`, to open a tunnel to
    /// `hostname:port`.
    pub async fn negotiate(&self, stream: &mut TcpStream, hostname: &str, port: u16) -> Result<()> {
        ensure!(
            !hostname.is_empty()
                && hostname.is_ascii()
                && (hostname.parse::<IpAddr>().is_ok()
                    || hostname
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))),
            "proxy destination must be an IP address or ASCII hostname"
        );
        match self.protocol {
            Protocol::Http => http_connect(stream, hostname, port).await,
            Protocol::Socks5 => socks5_connect(stream, hostname, port).await,
        }
    }
}

/// Performs an HTTP CONNECT handshake and accepts any 2xx response.
async fn http_connect(stream: &mut TcpStream, hostname: &str, port: u16) -> Result<()> {
    let address = util::authority(hostname, port);
    stream
        .write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await?;
    // Read byte-by-byte so no tunnel data after the headers is consumed.
    let mut header = Vec::new();
    loop {
        ensure!(
            header.len() < MAX_HTTP_HEADER,
            "HTTP CONNECT response headers exceed 16 KiB"
        );
        header.push(
            stream
                .read_u8()
                .await
                .context("read HTTP CONNECT response")?,
        );
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let line_end = header
        .windows(2)
        .position(|part| part == b"\r\n")
        .context("missing HTTP CONNECT status line")?;
    let status = std::str::from_utf8(&header[..line_end]).context("invalid HTTP CONNECT status")?;
    ensure!(
        !status.contains(['\r', '\n']),
        "invalid HTTP CONNECT status line"
    );
    let mut parts = status.split_ascii_whitespace();
    ensure!(
        matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")),
        "invalid HTTP CONNECT response version"
    );
    let code = parts.next().context("missing HTTP CONNECT status code")?;
    ensure!(
        code.len() == 3 && code.bytes().all(|c| c.is_ascii_digit()),
        "invalid HTTP CONNECT status code"
    );
    let code: u16 = code.parse()?;
    ensure!(
        (200..300).contains(&code),
        "HTTP CONNECT rejected with status {code}"
    );
    Ok(())
}

/// Performs a SOCKS5 no-authentication CONNECT handshake (RFC 1928).
async fn socks5_connect(stream: &mut TcpStream, hostname: &str, port: u16) -> Result<()> {
    stream.write_all(&[SOCKS_VERSION, 1, SOCKS_NO_AUTH]).await?;
    let mut greeting = [0; 2];
    stream
        .read_exact(&mut greeting)
        .await
        .context("read SOCKS5 greeting")?;
    ensure!(
        greeting == [SOCKS_VERSION, SOCKS_NO_AUTH],
        "SOCKS5 proxy does not accept no-authentication method"
    );
    let mut request = vec![SOCKS_VERSION, SOCKS_CONNECT, 0];
    match hostname.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(SOCKS_ATYP_IPV4);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(SOCKS_ATYP_IPV6);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let length =
                u8::try_from(hostname.len()).context("SOCKS5 hostname exceeds 255 bytes")?;
            request.extend_from_slice(&[SOCKS_ATYP_DOMAIN, length]);
            request.extend_from_slice(hostname.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut reply = [0; 4];
    stream
        .read_exact(&mut reply)
        .await
        .context("read SOCKS5 CONNECT reply")?;
    ensure!(
        reply[0] == SOCKS_VERSION && reply[2] == 0,
        "invalid SOCKS5 CONNECT reply"
    );
    ensure!(
        reply[1] == 0,
        "SOCKS5 CONNECT rejected with status {}",
        reply[1]
    );
    // Discard the bound address so the stream is positioned at tunnel data.
    let length = match reply[3] {
        SOCKS_ATYP_IPV4 => 4,
        SOCKS_ATYP_IPV6 => 16,
        SOCKS_ATYP_DOMAIN => {
            let length = stream.read_u8().await?;
            ensure!(length > 0, "empty SOCKS5 bound hostname");
            usize::from(length)
        }
        _ => bail!("invalid SOCKS5 bound address type"),
    };
    let mut bound = vec![0; length + 2];
    stream
        .read_exact(&mut bound)
        .await
        .context("read SOCKS5 bound address")?;
    Ok(())
}

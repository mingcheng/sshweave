use anyhow::{Context, Result, bail, ensure};
use std::net::{IpAddr, Ipv6Addr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Socks5,
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
}

impl Endpoint {
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

async fn http_connect(stream: &mut TcpStream, hostname: &str, port: u16) -> Result<()> {
    let address = if hostname.contains(':') {
        format!("[{hostname}]:{port}")
    } else {
        format!("{hostname}:{port}")
    };
    stream
        .write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await?;
    let mut header = Vec::new();
    loop {
        ensure!(
            header.len() < 16 * 1024,
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

async fn socks5_connect(stream: &mut TcpStream, hostname: &str, port: u16) -> Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut greeting = [0; 2];
    stream
        .read_exact(&mut greeting)
        .await
        .context("read SOCKS5 greeting")?;
    ensure!(
        greeting == [5, 0],
        "SOCKS5 proxy does not accept no-authentication method"
    );
    let mut request = vec![5, 1, 0];
    match hostname.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let length =
                u8::try_from(hostname.len()).context("SOCKS5 hostname exceeds 255 bytes")?;
            request.extend_from_slice(&[3, length]);
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
        reply[0] == 5 && reply[2] == 0,
        "invalid SOCKS5 CONNECT reply"
    );
    ensure!(
        reply[1] == 0,
        "SOCKS5 CONNECT rejected with status {}",
        reply[1]
    );
    let length = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
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

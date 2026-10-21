use crate::{
    config::{Config, Event, GatewayStrategy},
    hooks, proxy,
    resolver::ResolvedHost,
    util,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rand::{Rng, seq::SliceRandom};
use serde::Serialize;
use serde_json::json;
use std::{
    pin::Pin,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    sync::Mutex,
    time::{Instant, sleep_until, timeout},
};

type Reader = Pin<Box<dyn AsyncRead + Send>>;
type Writer = Pin<Box<dyn AsyncWrite + Send>>;

struct Connection {
    reader: Reader,
    writer: Option<Writer>,
    child: Option<Child>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub connected_at: u64,
    pub disconnected_at: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub duration_seconds: f64,
    pub average_bytes_per_second: f64,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub struct RateLimiter {
    bytes_per_second: u64,
    next: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(bytes_per_second: u64) -> Result<Self> {
        ensure!(bytes_per_second > 0, "rate must be positive");
        Ok(Self {
            bytes_per_second,
            next: Mutex::new(Instant::now()),
        })
    }

    pub async fn acquire(&self, bytes: usize) {
        let deadline = {
            let mut next = self.next.lock().await;
            *next = (*next).max(Instant::now())
                + Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
            *next
        };
        sleep_until(deadline).await;
    }
}

pub async fn copy_limited<R, W>(
    mut reader: R,
    mut writer: W,
    limiter: Option<Arc<RateLimiter>>,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    copy_counted(&mut reader, &mut writer, limiter, &AtomicU64::new(0)).await
}

async fn copy_counted<R, W>(
    mut reader: R,
    mut writer: W,
    limiter: Option<Arc<RateLimiter>>,
    counter: &AtomicU64,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let buffer_size = limiter.as_ref().map_or(16 * 1024, |limit| {
        (limit.bytes_per_second / 10).clamp(1, 16 * 1024) as usize
    });
    let mut buffer = vec![0; buffer_size];
    let mut count = 0;
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            writer.shutdown().await?;
            return Ok(count);
        }
        if let Some(limiter) = &limiter {
            limiter.acquire(size).await;
        }
        writer.write_all(&buffer[..size]).await?;
        writer.flush().await?;
        count += size as u64;
        counter.store(count, Ordering::Relaxed);
    }
}

pub async fn dial(hostname: &str, port: u16, seconds: u64) -> Result<TcpStream> {
    let connect = TcpStream::connect((hostname, port));
    let stream = if seconds == 0 {
        connect.await?
    } else {
        timeout(Duration::from_secs(seconds), connect)
            .await
            .context("TCP connection timeout")??
    };
    stream.set_nodelay(true)?;
    Ok(stream)
}

pub async fn prepare(host: &mut ResolvedHost, gateway: &str) -> Result<()> {
    if let Some(args) = &host.options.resolve_command {
        let args = args
            .iter()
            .map(|arg| host.expand(arg, gateway))
            .collect::<Result<Vec<_>>>()?;
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let seconds = host.options.connect_timeout.unwrap_or(10);
        let output = if seconds == 0 {
            command.output().await?
        } else {
            timeout(Duration::from_secs(seconds), command.output())
                .await
                .context("resolve_command timeout")??
        };
        ensure!(
            output.status.success(),
            "resolve_command exited with {}",
            output.status
        );
        let hostname =
            String::from_utf8(output.stdout).context("resolve_command output must be UTF-8")?;
        let hostname = hostname.trim();
        ensure!(
            !hostname.is_empty()
                && !hostname.starts_with('-')
                && !hostname.chars().any(char::is_whitespace),
            "resolve_command must return exactly one hostname or IP address"
        );
        util::single_line(hostname)?;
        host.hostname = hostname.into();
    }
    prepare_control_path(host)
}

pub fn prepare_control_path(host: &ResolvedHost) -> Result<()> {
    if host.options.control_master_mkdir == Some(true)
        && let Some(value) = host.options.control_path()?
    {
        let expanded = host.expand(&value, "")?;
        let path = std::path::Path::new(&expanded);
        let parent = path.parent().context("ControlPath has no parent")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
            #[cfg(not(unix))]
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

async fn child_connection(mut command: Command, seconds: u64) -> Result<Connection> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().context("start proxy transport")?;
    let mut reader = child.stdout.take().context("proxy stdout missing")?;
    let writer = child.stdin.take().context("proxy stdin missing")?;
    // A spawned ssh process is not a successful route until the destination speaks.
    // Retain this byte so startup detection never consumes protocol data.
    let mut first = [0u8; 1];
    let startup = async {
        let size = reader
            .read(&mut first)
            .await
            .context("read proxy startup")?;
        if size == 0 {
            let status = child.wait().await?;
            bail!("proxy exited before receiving destination data: {status}");
        }
        Ok::<_, anyhow::Error>(())
    };
    if seconds == 0 {
        startup.await?;
    } else {
        timeout(Duration::from_secs(seconds), startup)
            .await
            .context("proxy startup timeout")??;
    }
    Ok(Connection {
        reader: Box::pin(std::io::Cursor::new(first.to_vec()).chain(reader)),
        writer: Some(Box::pin(writer)),
        child: Some(child),
    })
}

async fn open(
    config: &Config,
    host: &ResolvedHost,
    gateway: &str,
    stack: &[String],
    proxy_name: Option<&str>,
) -> Result<Connection> {
    let seconds = host.options.connect_timeout.unwrap_or(10);
    if gateway == "direct"
        && let Some(proxy_name) = proxy_name
    {
        ensure!(
            host.options.proxy_command.is_none(),
            "proxy and proxy_command cannot be combined on the same transport"
        );
        let definition = config
            .document
            .proxies
            .get(proxy_name)
            .with_context(|| format!("unknown proxy {proxy_name:?}"))?;
        let proxy = proxy::Endpoint::parse(&definition.url)?;
        let startup = async {
            let mut stream = dial(&proxy.host, proxy.port, 0)
                .await
                .with_context(|| format!("connect proxy {proxy_name:?}"))?;
            proxy
                .negotiate(&mut stream, &host.hostname, host.port)
                .await?;
            let first = stream
                .read_u8()
                .await
                .context("read destination data after proxy handshake")?;
            let (reader, writer) = stream.into_split();
            Ok::<_, anyhow::Error>(Connection {
                reader: Box::pin(std::io::Cursor::new(vec![first]).chain(reader)),
                writer: Some(Box::pin(writer)),
                child: None,
            })
        };
        return if seconds == 0 {
            startup.await
        } else {
            timeout(Duration::from_secs(seconds), startup)
                .await
                .context("proxy startup timeout")?
        };
    }
    let args = host
        .options
        .proxy_command
        .as_ref()
        .map(|args| {
            args.iter()
                .map(|arg| host.expand(arg, gateway))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    if gateway == "direct" {
        if let Some(args) = args {
            let mut command = Command::new(&args[0]);
            command.args(&args[1..]);
            return child_connection(command, seconds).await;
        }
        let stream = dial(&host.hostname, host.port, seconds)
            .await
            .with_context(|| format!("connect {}:{}", host.hostname, host.port))?;
        let (reader, writer) = stream.into_split();
        return Ok(Connection {
            reader: Box::pin(reader),
            writer: Some(Box::pin(writer)),
            child: None,
        });
    }
    util::valid_target(gateway)?;
    let mut command = Command::new(config.ssh_binary()?);
    command
        .arg("-F")
        .arg(config.output()?)
        .arg("-o")
        .arg(format!("ConnectTimeout={seconds}"))
        .arg("-o")
        .arg("ExitOnForwardFailure=yes");
    if let Some(proxy_name) = proxy_name {
        command.env("SSHWEAVE_ROUTE_PROXY", proxy_name);
        command
            .arg("-o")
            .arg("ControlMaster=no")
            .arg("-o")
            .arg("ControlPath=none");
    }
    if let Some(args) = args {
        command.arg("--").arg(gateway).arg(shell_words::join(args));
    } else {
        let address = if host.hostname.contains(':') {
            format!("[{}]:{}", host.hostname, host.port)
        } else {
            format!("{}:{}", host.hostname, host.port)
        };
        command.arg("-W").arg(address).arg("--").arg(gateway);
    }
    command.env("SSHWEAVE_ROUTE_STACK", serde_json::to_string(stack)?);
    child_connection(command, seconds).await
}

async fn relay(connection: &mut Connection, host: &ResolvedHost) -> Result<Stats> {
    let limiter = host
        .options
        .rate_limit
        .as_ref()
        .map(|rate| RateLimiter::new(util::rate_bytes(rate)?).map(Arc::new))
        .transpose()?;
    let start = Instant::now();
    let mut stats = Stats {
        connected_at: now()?,
        ..Stats::default()
    };
    hooks::invoke(
        &host.options,
        Event::OnConnect,
        &json!({"host":host, "stats":stats}),
    )
    .await?;
    let sent = AtomicU64::new(0);
    let received = AtomicU64::new(0);
    let writer = connection
        .writer
        .take()
        .context("transport writer missing")?;
    let transfer = async {
        let upload = copy_counted(tokio::io::stdin(), writer, limiter.clone(), &sent);
        let download = copy_counted(
            &mut connection.reader,
            tokio::io::stdout(),
            limiter,
            &received,
        );
        tokio::pin!(upload, download);
        tokio::select! {
            result = &mut download => { stats.received_bytes = result?; }
            result = &mut upload => {
                stats.sent_bytes = result?;
                stats.received_bytes = download.await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = async {
        if let Some(child) = &mut connection.child {
            // Closing the read side or stdin may leave ssh alive; reap it explicitly.
            match child.try_wait()? {
                Some(status) => ensure!(status.success(), "proxy transport exited with {status}"),
                None => {
                    child.kill().await?;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stats.sent_bytes = sent.load(Ordering::Relaxed);
    stats.received_bytes = received.load(Ordering::Relaxed);
    stats.disconnected_at = now()?;
    stats.duration_seconds = start.elapsed().as_secs_f64();
    stats.average_bytes_per_second =
        (stats.sent_bytes + stats.received_bytes) as f64 / stats.duration_seconds.max(f64::EPSILON);
    let hook = hooks::invoke(&host.options, Event::OnDisconnect,
        &json!({"host":host, "stats":stats, "error":transfer.as_ref().err().map(ToString::to_string)}))        .await;
    if let Err(error) = &hook {
        tracing::error!(error = %format!("{error:#}"), "disconnect hook failed");
    }
    transfer?;
    cleanup?;
    hook?;
    Ok(stats)
}

fn attempt_routes(host: &ResolvedHost, rng: &mut impl Rng) -> Vec<String> {
    let mut routes = host.routes();
    if host.gateway_strategy == GatewayStrategy::Random {
        routes.shuffle(rng);
    }
    routes
}

pub async fn connect(
    config: &Config,
    target: &str,
    port: Option<u16>,
    user: Option<&str>,
) -> Result<()> {
    let mut host = config.resolve(target)?;
    if let Some(port) = port {
        ensure!(port > 0, "port must be positive");
        host.port = port;
    }
    if let Some(user) = user {
        host.user = user.into();
    }
    let mut stack: Vec<String> = match std::env::var("SSHWEAVE_ROUTE_STACK") {
        Ok(value) => serde_json::from_str(&value).context("invalid SSHWEAVE_ROUTE_STACK")?,
        Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        stack.len() < 32 && !stack.iter().any(|s| s == target),
        "gateway cycle: {} -> {target}",
        stack.join(" -> ")
    );
    stack.push(target.into());
    let proxy_name = match std::env::var("SSHWEAVE_ROUTE_PROXY") {
        Ok(name) => Some(name),
        Err(std::env::VarError::NotPresent) => host.options.proxy.clone(),
        Err(error) => return Err(error.into()),
    };
    if let Some(name) = &proxy_name {
        ensure!(
            config.document.proxies.contains_key(name),
            "unknown route proxy {name:?}"
        );
    }
    host.options.proxy = proxy_name.clone();
    let routes = attempt_routes(&host, &mut rand::rng());
    let mut errors = Vec::new();
    for gateway in routes {
        let mut attempt = host.clone();
        let result = async {
            prepare(&mut attempt, &gateway).await?;
            hooks::invoke(
                &attempt.options,
                Event::BeforeConnect,
                &json!({"host":attempt, "gateway":gateway}),
            )
            .await?;
            open(config, &attempt, &gateway, &stack, proxy_name.as_deref()).await
        }
        .await;
        match result {
            Ok(mut connection) => {
                let stats = relay(&mut connection, &attempt).await?;
                tracing::debug!(?stats, "connection closed");
                return Ok(());
            }
            Err(error) => {
                hooks::invoke(
                    &attempt.options,
                    Event::OnConnectError,
                    &json!({"host":attempt, "gateway":gateway, "error":format!("{error:#}")}),
                )
                .await?;
                tracing::warn!(%gateway, error = %format!("{error:#}"), "route failed");
                errors.push(format!("{gateway}: {error:#}"));
            }
        }
    }
    Err(anyhow!(
        "all routes failed for {target}:\n{}",
        errors.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Host;
    use rand::{SeedableRng, rngs::StdRng};
    use std::collections::BTreeSet;

    #[test]
    fn random_routes_shuffle_all_candidates_without_losing_fallbacks() {
        let mut host = ResolvedHost {
            name: "db".into(),
            pattern: None,
            hostname: "db.internal".into(),
            user: "ops".into(),
            port: 22,
            gateways: vec!["direct".into(), "a".into(), "b".into()],
            gateway_strategy: GatewayStrategy::Random,
            options: Host::default(),
        };
        let mut orders = BTreeSet::new();
        for seed in 0..64 {
            let routes = attempt_routes(&host, &mut StdRng::seed_from_u64(seed));
            let mut sorted = routes.clone();
            sorted.sort();
            assert_eq!(sorted, ["a", "b", "direct"]);
            orders.insert(routes);
        }
        assert_eq!(
            orders.len(),
            6,
            "random strategy must not retain a fixed order"
        );
        host.gateway_strategy = GatewayStrategy::Sequential;
        assert_eq!(
            attempt_routes(&host, &mut StdRng::seed_from_u64(0)),
            host.gateways
        );
        host.gateway_strategy = GatewayStrategy::Chain;
        host.gateways = vec!["outer".into(), "inner".into()];
        assert_eq!(
            attempt_routes(&host, &mut StdRng::seed_from_u64(0)),
            ["inner/outer"]
        );
        host.gateways.clear();
        for strategy in [
            GatewayStrategy::Random,
            GatewayStrategy::Sequential,
            GatewayStrategy::Chain,
        ] {
            host.gateway_strategy = strategy;
            assert_eq!(
                attempt_routes(&host, &mut StdRng::seed_from_u64(0)),
                ["direct"]
            );
        }
    }
}

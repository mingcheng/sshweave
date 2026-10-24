//! Connection runtime used by `sshweave connect` (OpenSSH's ProxyCommand).
//!
//! For each candidate route sshweave opens a byte stream — direct TCP, an
//! HTTP/SOCKS5 proxy, a local `proxy_command`, or `ssh -W` through a gateway —
//! waits for the destination to send its first byte, and then relays
//! stdin/stdout until both directions close. Native TCP without alternative
//! routes remains a general byte stream and does not require a server-first protocol.

use crate::{
    config::{Config, Event, GatewayStrategy, Host},
    hooks, proxy,
    resolver::{MAX_GATEWAY_DEPTH, ResolvedHost, Resolver},
    util,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rand::{Rng, seq::SliceRandom};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
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
    net::{TcpSocket, TcpStream},
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
    sync::Mutex,
    task::JoinSet,
    time::{Instant, sleep, sleep_until, timeout},
};

/// Timeout in seconds used when `connect_timeout` is not configured.
const DEFAULT_CONNECT_TIMEOUT: u64 = 10;

/// Delay before racing the next resolved address while earlier attempts are
/// still pending (RFC 8305 "Connection Attempt Delay").
const CONNECTION_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Relay buffer size without a rate limit.
const BUFFER_SIZE: usize = 16 * 1024;

/// Environment variable carrying the JSON list of targets on the current route,
/// used to detect gateway cycles across nested sshweave processes.
pub const ROUTE_STACK_ENV: &str = "SSHWEAVE_ROUTE_STACK";

/// Environment variable forcing the proxy of the outermost hop in nested processes.
const ROUTE_PROXY_ENV: &str = "SSHWEAVE_ROUTE_PROXY";

type Reader = Pin<Box<dyn AsyncRead + Send>>;
type Writer = Pin<Box<dyn AsyncWrite + Send>>;

/// An established transport to the destination.
struct Connection {
    reader: Reader,
    writer: Option<Writer>,
    /// Transport process to reap when the relay ends, if any.
    child: Option<Child>,
}

impl Connection {
    /// Wraps a stream, replaying `first` (the startup probe byte) before `reader`.
    fn new(
        first: Option<u8>,
        reader: impl AsyncRead + Send + 'static,
        writer: impl AsyncWrite + Send + 'static,
        child: Option<Child>,
    ) -> Self {
        let prefix = std::io::Cursor::new(first.map(|byte| vec![byte]).unwrap_or_default());
        Self {
            reader: Box::pin(prefix.chain(reader)),
            writer: Some(Box::pin(writer)),
            child,
        }
    }
}

/// Transfer statistics exposed to `on_connect` and `on_disconnect` hooks.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    /// Unix timestamp (seconds) when the relay started.
    pub connected_at: u64,
    /// Unix timestamp (seconds) when the relay ended.
    pub disconnected_at: u64,
    /// Bytes sent from local stdin to the destination.
    pub sent_bytes: u64,
    /// Bytes received from the destination.
    pub received_bytes: u64,
    /// Relay duration in seconds.
    pub duration_seconds: f64,
    /// Combined throughput over the relay duration.
    pub average_bytes_per_second: f64,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// Awaits `future`, failing with `message` after `seconds`; `0` disables the timeout.
async fn with_timeout<T>(
    seconds: u64,
    message: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    if seconds == 0 {
        future.await
    } else {
        timeout(Duration::from_secs(seconds), future)
            .await
            .context(message)?
    }
}

/// A token-bucket style limiter shared by both relay directions.
pub struct RateLimiter {
    bytes_per_second: u64,
    next: Mutex<Instant>,
}

impl RateLimiter {
    /// Creates a limiter allowing `bytes_per_second` (must be positive).
    pub fn new(bytes_per_second: u64) -> Result<Self> {
        ensure!(bytes_per_second > 0, "rate must be positive");
        Ok(Self {
            bytes_per_second,
            next: Mutex::new(Instant::now()),
        })
    }

    /// Reserves `bytes` of budget and sleeps until it is available.
    #[allow(clippy::cast_precision_loss)]
    pub async fn acquire(&self, bytes: usize) {
        let deadline = {
            let mut next = self.next.lock().await;
            *next = (*next).max(Instant::now())
                + Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
            *next
        };
        sleep_until(deadline).await;
    }

    /// Chunk size giving roughly ten writes per second, capped at [`BUFFER_SIZE`].
    fn chunk_size(&self) -> usize {
        usize::try_from(self.bytes_per_second / 10)
            .unwrap_or(BUFFER_SIZE)
            .clamp(1, BUFFER_SIZE)
    }
}

/// Copies `reader` to `writer` until EOF, then shuts down `writer`.
/// Returns the number of bytes copied.
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

/// Like [`copy_limited`], also publishing progress through `counter` so partial
/// counts survive cancellation.
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
    let buffer_size = limiter
        .as_deref()
        .map_or(BUFFER_SIZE, RateLimiter::chunk_size);
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

/// Address family restriction from OpenSSH's `AddressFamily` option.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AddressFamily {
    /// IPv4 or IPv6 (`any`, the default).
    #[default]
    Any,
    /// IPv4 only (`inet`).
    Inet,
    /// IPv6 only (`inet6`).
    Inet6,
}

impl AddressFamily {
    /// Parses an OpenSSH `AddressFamily` value.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "any" => Ok(Self::Any),
            "inet" => Ok(Self::Inet),
            "inet6" => Ok(Self::Inet6),
            _ => bail!("AddressFamily must be any, inet, or inet6"),
        }
    }

    fn allows(self, address: &SocketAddr) -> bool {
        match self {
            Self::Any => true,
            Self::Inet => address.is_ipv4(),
            Self::Inet6 => address.is_ipv6(),
        }
    }
}

/// Local socket preferences for sshweave's own TCP connections, taken from the
/// OpenSSH `AddressFamily` and `BindAddress` options of the dialed host.
#[derive(Clone, Debug, Default)]
pub struct DialOptions {
    /// Allowed destination address family.
    pub family: AddressFamily,
    /// Local address or hostname to bind before connecting.
    pub bind: Option<String>,
}

impl DialOptions {
    /// Reads `AddressFamily` and `BindAddress` from a host's SSH options.
    pub fn from_host(host: &Host) -> Result<Self> {
        fn single(host: &Host, key: &str) -> Result<Option<String>> {
            let Some(value) = host.ssh_option(key) else {
                return Ok(None);
            };
            let mut lines = value.lines();
            ensure!(lines.len() == 1, "{key} requires one value");
            let value = util::expand_env(&lines.remove(0))?;
            let mut words = shell_words::split(&value).with_context(|| format!("parse {key}"))?;
            ensure!(words.len() == 1, "{key} requires one value");
            Ok(Some(words.remove(0)))
        }
        Ok(Self {
            family: single(host, "AddressFamily")?
                .map(|value| AddressFamily::parse(&value))
                .transpose()?
                .unwrap_or_default(),
            bind: single(host, "BindAddress")?,
        })
    }

    async fn bind_addresses(&self) -> Result<Vec<IpAddr>> {
        let Some(bind) = &self.bind else {
            return Ok(Vec::new());
        };
        if let Ok(address) = bind.parse::<IpAddr>() {
            return Ok(vec![address]);
        }
        let addresses: Vec<IpAddr> = tokio::net::lookup_host((bind.as_str(), 0))
            .await
            .with_context(|| format!("resolve BindAddress {bind}"))?
            .map(|address| address.ip())
            .collect();
        ensure!(!addresses.is_empty(), "BindAddress {bind} has no addresses");
        Ok(addresses)
    }
}

/// Orders addresses by alternating families, starting with the resolver's
/// first preference (RFC 8305 section 4).
fn interleave(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let first_v6 = addresses.first().is_some_and(SocketAddr::is_ipv6);
    let (v6, v4): (VecDeque<_>, VecDeque<_>) = addresses.into_iter().partition(SocketAddr::is_ipv6);
    let (mut first, mut second) = if first_v6 { (v6, v4) } else { (v4, v6) };
    let mut ordered = Vec::with_capacity(first.len() + second.len());
    while !first.is_empty() || !second.is_empty() {
        ordered.extend(first.pop_front());
        ordered.extend(second.pop_front());
    }
    ordered
}

async fn connect_address(address: SocketAddr, binds: Arc<Vec<IpAddr>>) -> Result<TcpStream> {
    let socket = if address.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    if !binds.is_empty() {
        let local = binds
            .iter()
            .find(|ip| ip.is_ipv4() == address.is_ipv4())
            .with_context(|| format!("BindAddress has no address for {address}"))?;
        socket
            .bind(SocketAddr::new(*local, 0))
            .with_context(|| format!("bind {local}"))?;
    }
    Ok(socket.connect(address).await?)
}

/// Connects to the first reachable resolved address. A new attempt starts
/// whenever the previous one fails or after [`CONNECTION_ATTEMPT_DELAY`], so
/// one unreachable address family cannot consume the whole timeout.
async fn connect_any(hostname: &str, port: u16, options: &DialOptions) -> Result<TcpStream> {
    let binds = Arc::new(options.bind_addresses().await?);
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((hostname, port))
        .await
        .with_context(|| format!("resolve {hostname}"))?
        .filter(|address| options.family.allows(address))
        .collect();
    ensure!(
        !resolved.is_empty(),
        "{hostname} has no address allowed by AddressFamily"
    );
    let mut pending = interleave(resolved).into_iter();
    let mut attempts = JoinSet::new();
    let mut last_error = None;
    let mut next = pending.next();
    loop {
        if let Some(address) = next.take() {
            attempts.spawn(connect_address(address, binds.clone()));
        }
        let more = pending.len() > 0;
        tokio::select! {
            joined = attempts.join_next() => match joined {
                Some(result) => match result.context("connection attempt task")? {
                    // Dropping the JoinSet aborts the remaining attempts.
                    Ok(stream) => return Ok(stream),
                    Err(error) => {
                        last_error = Some(error);
                        next = pending.next();
                        if next.is_none() && attempts.is_empty() {
                            break;
                        }
                    }
                },
                None => break,
            },
            () = sleep(CONNECTION_ATTEMPT_DELAY), if more => next = pending.next(),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no addresses for {hostname}")))
}

/// Opens a TCP connection with `TCP_NODELAY`; `seconds == 0` disables the timeout.
pub async fn dial(hostname: &str, port: u16, seconds: u64) -> Result<TcpStream> {
    dial_with(hostname, port, seconds, &DialOptions::default()).await
}

/// Like [`dial`], honoring an address family restriction and local bind address.
pub async fn dial_with(
    hostname: &str,
    port: u16,
    seconds: u64,
    options: &DialOptions,
) -> Result<TcpStream> {
    let stream = with_timeout(
        seconds,
        "TCP connection timeout",
        connect_any(hostname, port, options),
    )
    .await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Runs per-attempt preparation: `resolve_command` (which may replace the
/// hostname) and ControlPath directory creation.
pub async fn prepare(host: &mut ResolvedHost, gateway: &str) -> Result<()> {
    if let Some(args) = &host.options.resolve_command {
        let args = args
            .iter()
            .map(|arg| host.expand(arg, gateway))
            .collect::<Result<Vec<_>>>()?;
        let (program, args) = args.split_first().context("resolve_command is empty")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let seconds = host
            .options
            .connect_timeout
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let output = with_timeout(seconds, "resolve_command timeout", async {
            Ok(command.output().await?)
        })
        .await?;
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

/// Creates the ControlPath parent directory when `control_master_mkdir` is set.
pub fn prepare_control_path(host: &ResolvedHost) -> Result<()> {
    if host.options.control_master_mkdir == Some(true)
        && let Some(value) = host.options.control_path()?
    {
        let expanded = host.expand(&value, "")?;
        let parent = std::path::Path::new(&expanded)
            .parent()
            .context("ControlPath has no parent")?;
        util::create_private_dir(parent)?;
    }
    Ok(())
}

/// Spawns a transport process and waits until the destination sends data.
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
    with_timeout(seconds, "proxy startup timeout", async {
        let size = reader
            .read(&mut first)
            .await
            .context("read proxy startup")?;
        if size == 0 {
            let status = child.wait().await?;
            bail!("proxy exited before receiving destination data: {status}");
        }
        Ok(())
    })
    .await?;
    Ok(Connection::new(Some(first[0]), reader, writer, Some(child)))
}

/// Connects through a named HTTP/SOCKS5 proxy and waits for destination data.
async fn proxy_connection(
    config: &Config,
    host: &ResolvedHost,
    proxy_name: &str,
    seconds: u64,
) -> Result<Connection> {
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
    with_timeout(seconds, "proxy startup timeout", async {
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
        Ok(Connection::new(Some(first), reader, writer, None))
    })
    .await
}

async fn tcp_connection(host: &ResolvedHost, seconds: u64, probe: bool) -> Result<Connection> {
    let message = if probe {
        "direct startup timeout"
    } else {
        "TCP connection timeout"
    };
    with_timeout(seconds, message, async {
        let options = DialOptions::from_host(&host.options)?;
        let mut stream = dial_with(&host.hostname, host.port, 0, &options)
            .await
            .with_context(|| format!("connect {}:{}", host.hostname, host.port))?;
        let first = if probe {
            Some(
                stream
                    .read_u8()
                    .await
                    .context("direct route closed before receiving destination data")?,
            )
        } else {
            None
        };
        let (reader, writer) = stream.into_split();
        Ok(Connection::new(first, reader, writer, None))
    })
    .await
}

/// Opens one route attempt to `host` via `gateway` (`direct` or a target).
async fn open(
    config: &Config,
    host: &ResolvedHost,
    gateway: &str,
    stack: &[String],
    proxy_name: Option<&str>,
    probe_direct: bool,
) -> Result<Connection> {
    let seconds = host
        .options
        .connect_timeout
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
    if gateway == "direct"
        && let Some(proxy_name) = proxy_name
    {
        return proxy_connection(config, host, proxy_name, seconds).await;
    }
    let proxy_command = host
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
        if let Some(args) = proxy_command {
            let (program, args) = args.split_first().context("proxy_command is empty")?;
            let mut command = Command::new(program);
            command.args(args);
            return child_connection(command, seconds).await;
        }
        return tcp_connection(host, seconds, probe_direct).await;
    }
    // Gateway hop: let OpenSSH reach the gateway (itself routed by sshweave),
    // then forward to the destination or run proxy_command there.
    util::valid_target(gateway)?;
    let mut command = Command::new(config.ssh_binary()?);
    command
        .arg("-F")
        .arg(config.output()?)
        .arg("-o")
        .arg(format!("ConnectTimeout={seconds}"))
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .env(ROUTE_STACK_ENV, serde_json::to_string(stack)?);
    if let Some(proxy_name) = proxy_name {
        // A shared control master could bypass the proxy, so disable multiplexing.
        command.env(ROUTE_PROXY_ENV, proxy_name).args([
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
        ]);
    }
    if let Some(args) = proxy_command {
        command.arg("--").arg(gateway).arg(shell_words::join(args));
    } else {
        command
            .arg("-W")
            .arg(util::authority(&host.hostname, host.port))
            .arg("--")
            .arg(gateway);
    }
    child_connection(command, seconds).await
}

/// Relays stdin/stdout over `connection` until both directions finish, running
/// `on_connect`/`on_disconnect` hooks around the transfer.
/// Termination signals handled for the whole connection lifetime.
///
/// Once registered, these signals no longer terminate the process by default,
/// so every long wait must also watch them. Dropping the interrupted work kills
/// its child processes (`kill_on_drop`).
struct Signals {
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> Result<Self> {
        Ok(Self {
            hangup: signal(SignalKind::hangup())?,
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Waits for the next termination signal and returns its name.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.hangup.recv() => "SIGHUP",
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
        }
    }

    /// Runs `work` unless a termination signal arrives first, which yields
    /// an [`Interrupted`] error.
    async fn guard<T>(
        &mut self,
        stage: &'static str,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::select! {
            result = work => result,
            signal = self.recv() => Err(Interrupted { signal, stage }.into()),
        }
    }
}

/// A termination signal stopped work in progress.
#[derive(Debug)]
struct Interrupted {
    signal: &'static str,
    stage: &'static str,
}

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} received during {}", self.signal, self.stage)
    }
}

impl std::error::Error for Interrupted {}

async fn relay(
    connection: &mut Connection,
    host: &ResolvedHost,
    signals: &mut Signals,
) -> Result<Stats> {
    let limiter = host
        .options
        .rate_limit
        .as_deref()
        .map(|rate| RateLimiter::new(util::rate_bytes(rate)?).map(Arc::new))
        .transpose()?;
    let start = Instant::now();
    let mut stats = Stats {
        connected_at: now()?,
        ..Stats::default()
    };
    signals
        .guard(
            "on_connect hooks",
            hooks::invoke(
                &host.options,
                Event::OnConnect,
                &json!({"host": host, "stats": stats}),
            ),
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
        // The session ends when the destination closes. If local stdin closes
        // first (a half-close), keep draining the destination until it does.
        tokio::select! {
            result = &mut download => { result?; }
            result = &mut upload => {
                result?;
                download.await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    // OpenSSH sends SIGHUP to its ProxyCommand when it exits, which is a normal
    // end of the session. Handle termination signals so cleanup and
    // on_disconnect hooks still run instead of the process being killed.
    let transfer = tokio::select! {
        result = transfer => result,
        signal = signals.recv() => match signal {
            "SIGHUP" => {
                tracing::debug!("SIGHUP received; closing transport");
                Ok(())
            }
            other => Err(anyhow!("{other} received; closing transport")),
        },
    };
    let cleanup = async {
        if let Some(child) = &mut connection.child {
            // Closing the read side or stdin may leave ssh alive; reap it explicitly.
            match child.try_wait()? {
                Some(status) => ensure!(status.success(), "proxy transport exited with {status}"),
                None => child.kill().await?,
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stats.sent_bytes = sent.load(Ordering::Relaxed);
    stats.received_bytes = received.load(Ordering::Relaxed);
    stats.disconnected_at = now()?;
    stats.duration_seconds = start.elapsed().as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let total_bytes = (stats.sent_bytes + stats.received_bytes) as f64;
    stats.average_bytes_per_second = total_bytes / stats.duration_seconds.max(f64::EPSILON);
    let error = transfer.as_ref().err().map(ToString::to_string);
    // A further signal abandons slow disconnect hooks instead of being ignored.
    let hook = signals
        .guard(
            "on_disconnect hooks",
            hooks::invoke(
                &host.options,
                Event::OnDisconnect,
                &json!({"host": host, "stats": stats, "error": error}),
            ),
        )
        .await;
    if let Err(error) = &hook {
        tracing::error!(error = %format!("{error:#}"), "disconnect hook failed");
    }
    transfer?;
    cleanup?;
    hook?;
    Ok(stats)
}

/// Returns the routes to attempt, shuffled for the random strategy.
fn attempt_routes(host: &ResolvedHost, rng: &mut impl Rng) -> Vec<String> {
    let mut routes = host.routes();
    if host.gateway_strategy == GatewayStrategy::Random {
        routes.shuffle(rng);
    }
    routes
}

/// Reads an optional environment variable, rejecting non-Unicode values.
fn optional_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {name}")),
    }
}

/// Connects to `target`, trying each route until one succeeds, and relays
/// stdin/stdout until the session ends.
///
/// `port` and `user` override resolved values (OpenSSH passes `%p`/`%r`).
pub async fn connect(
    config: &Config,
    target: &str,
    port: Option<u16>,
    user: Option<&str>,
) -> Result<()> {
    connect_with(&config.resolver()?, target, port, user).await
}

/// Like [`connect`], reusing an already compiled [`Resolver`].
pub async fn connect_with(
    resolver: &Resolver<'_>,
    target: &str,
    port: Option<u16>,
    user: Option<&str>,
) -> Result<()> {
    let config = resolver.config();
    let mut host = resolver.resolve(target)?;
    if let Some(port) = port {
        ensure!(port > 0, "port must be positive");
        host.port = port;
    }
    if let Some(user) = user {
        host.user = user.into();
    }
    let mut stack: Vec<String> = match optional_env(ROUTE_STACK_ENV)? {
        Some(value) => {
            serde_json::from_str(&value).with_context(|| format!("invalid {ROUTE_STACK_ENV}"))?
        }
        None => Vec::new(),
    };
    ensure!(
        stack.len() < MAX_GATEWAY_DEPTH && !stack.iter().any(|s| s == target),
        "gateway cycle: {} -> {target}",
        stack.join(" -> ")
    );
    stack.push(target.into());
    // A proxy configured on the outermost hop applies to the whole route.
    let proxy_name = optional_env(ROUTE_PROXY_ENV)?.or_else(|| host.options.proxy.clone());
    if let Some(name) = &proxy_name {
        ensure!(
            config.document.proxies.contains_key(name),
            "unknown route proxy {name:?}"
        );
    }
    host.options.proxy.clone_from(&proxy_name);
    let mut signals = Signals::new()?;
    let mut errors = Vec::new();
    let routes = attempt_routes(&host, &mut rand::rng());
    let probe_direct = routes.len() > 1;
    for gateway in routes {
        let mut attempt = host.clone();
        // A signal (e.g. OpenSSH giving up) drops the attempt, which kills
        // resolve_command, hook, proxy_command, or gateway processes.
        let work = async {
            prepare(&mut attempt, &gateway).await?;
            hooks::invoke(
                &attempt.options,
                Event::BeforeConnect,
                &json!({"host": attempt, "gateway": gateway}),
            )
            .await?;
            open(
                config,
                &attempt,
                &gateway,
                &stack,
                proxy_name.as_deref(),
                probe_direct,
            )
            .await
        };
        let result = match signals.guard("transport setup", work).await {
            Err(error) if error.is::<Interrupted>() => {
                return Err(error.context(format!("connect {target}")));
            }
            result => result,
        };
        match result {
            Ok(mut connection) => {
                let stats = relay(&mut connection, &attempt, &mut signals).await?;
                tracing::debug!(?stats, "connection closed");
                return Ok(());
            }
            Err(error) => {
                // A failing notification must not prevent the remaining routes.
                let context =
                    json!({"host": attempt, "gateway": gateway, "error": format!("{error:#}")});
                let hook = hooks::invoke(&attempt.options, Event::OnConnectError, &context);
                if let Err(hook_error) = signals.guard("on_connect_error hooks", hook).await {
                    if hook_error.is::<Interrupted>() {
                        return Err(hook_error);
                    }
                    tracing::error!(%gateway, error = %format!("{hook_error:#}"), "connect error hook failed");
                }
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
    fn interleave_alternates_address_families_from_first_preference() {
        let parse = |items: &[&str]| -> Vec<SocketAddr> {
            items.iter().map(|item| item.parse().unwrap()).collect()
        };
        assert_eq!(
            interleave(parse(&[
                "[::1]:22",
                "[::2]:22",
                "10.0.0.1:22",
                "10.0.0.2:22"
            ])),
            parse(&["[::1]:22", "10.0.0.1:22", "[::2]:22", "10.0.0.2:22"])
        );
        assert_eq!(
            interleave(parse(&["10.0.0.1:22", "[::1]:22", "[::2]:22"])),
            parse(&["10.0.0.1:22", "[::1]:22", "[::2]:22"])
        );
        assert!(AddressFamily::parse("INET6").is_ok_and(|f| f == AddressFamily::Inet6));
        assert!(AddressFamily::parse("ipv4").is_err());
    }

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

# sshweave configuration and usage guide

**English** | [简体中文](README_CN.md) · [Product overview](../README.md)

This guide covers installation, configuration, routing, commands, and maintenance.
sshweave compiles configuration, selects routes, and transports byte streams.
System OpenSSH handles authentication, host-key verification, and sessions;
sshweave does not reimplement the SSH protocol.

## Contents

- [Build and get started](#build-and-get-started)
- [TOML configuration](#toml-configuration)
- [Hooks](#hooks)
- [Commands](#commands)
- [Feature comparison with assh](#feature-comparison-with-assh)
- [Architecture and maintenance](#architecture-and-maintenance)

## Build and get started

Requirements: Rust 1.88+, a Unix system (macOS/Linux), and OpenSSH. Desktop
notifications also require `osascript` on macOS or `notify-send` on Linux.
Ordinary connections do not depend on nc/socat.
Run the build, installation, example-copying, and maintenance commands below from
the repository root, not from the examples directory.

```sh
cargo build --release --locked
cargo install --path . --locked
mkdir -p ~/.config/sshweave
cp examples/sshweave.toml ~/.config/sshweave/sshweave.toml
# First adapt the example domains, usernames, and key paths to your environment.
sshweave config check
sshweave config build --write
sshweave wrapper ssh database
```

Use `sshweave wrapper ssh` for the recommended workflow: it resolves patterns,
instantiates gateway chains, creates control-socket directories, and updates
configuration **before** OpenSSH reads it. SSH arguments, remote commands, and
exit codes are passed through.

```sh
sshweave wrapper ssh -p 2222 -v database uname -a
sshweave wrapper ssh worker-eu-42
sshweave wrapper ssh database/bastion/outer-gateway
sshweave wrapper scp file.txt database:/tmp/
sshweave wrapper sftp database
sshweave wrapper rsync -az source/ database:/srv/
```

### Shell aliases

Optionally add these aliases to Bash/Zsh to keep using familiar commands:

```sh
# Add to ~/.bashrc or ~/.zshrc; macOS Bash login shells can source ~/.bashrc
# from ~/.bash_profile.
alias ssh='sshweave wrapper ssh'
alias scp='sshweave wrapper scp'
alias sftp='sshweave wrapper sftp'
alias rsync='sshweave wrapper rsync'

ssh -p 2222 -v database uname -a
ssh database/bastion/outer-gateway
scp -P 2222 file.txt database:/tmp/
sftp -P 2222 database
rsync -az source/ database:/srv/

# Bypass the alias temporarily and invoke the original program.
command ssh database
command scp file.txt database:/tmp/
command sftp database
command rsync -az source/ database:/srv/
```

For a custom configuration, set
`export SSHWEAVE_CONFIG="$HOME/.config/sshweave/sshweave.toml"`; all aliases above
will use it. Alternatively, use
`alias ssh='sshweave -c /absolute/path/sshweave.toml wrapper ssh'`.
The other aliases can likewise put `-c /absolute/path/sshweave.toml` before
`wrapper`.

Arguments after the program name belong to that program: SSH uses `-p` for a
port, while SCP/SFTP use `-P`. After `wrapper ssh`, `-v`, `-c`, `-V`, and `-h`
belong to OpenSSH, not sshweave. Put sshweave options before `wrapper`;
use `sshweave help wrapper` for wrapper help.

The wrapper directly launches `settings.ssh_binary` (default: `ssh` from PATH),
or `scp` / `sftp` / `rsync` from PATH, without a shell, so aliases are not
expanded again. Aliases affect only the current shell, not SSH calls made by
Git or GUI applications. For those applications, use the Include setup below.

### OpenSSH Include and automatic updates

You can also add this at the top of your **hand-maintained** `~/.ssh/config`:

```sshconfig
Include ~/.config/sshweave/sshweave.conf
```

Then use `ssh`, `scp`, `sftp`, `rsync`, or OpenSSH-based Git/GUI applications
directly. OpenSSH uses the first effective value, so Include placement affects
precedence. The generated file ends with a `Host *` block enabling sshweave's
ProxyCommand for any destination. Put hand-written settings requiring a special
proxy before the Include, or use a dedicated configuration with `ssh -F`.

**Automatic-update boundary:** when ProxyCommand runs, the outer OpenSSH process
has already read its configuration. After a configuration change, or on first
use of a new glob/regex host or `host/gateway` chain, sshweave atomically updates
the file and explicitly asks you to rerun SSH rather than continuing with stale
authentication settings. The wrapper avoids this retry.
`--no-rewrite` / `automatic_rewrite = false` disables ProxyCommand's automatic
updates; explicit builds and wrapper invocations still generate up-to-date
configuration as required.

sshweave only overwrites generated files carrying its own marker. It **does not
automatically modify your hand-written `~/.ssh/config`**.
`config build` writes to stdout by default; prefer `--write`. Do not redirect
stdout into the same file being read or managed.

## TOML configuration

See [sshweave.toml](sshweave.toml) for a complete working configuration structure.

The default configuration file is `$HOME/.config/sshweave/sshweave.toml`.
Generated OpenSSH configuration, the instantiated-host cache, lock files, and
atomic-write temporary files live under `$HOME/.config/sshweave/`.
The example also stores control sockets in its `control/` subdirectory.
`-c` / `SSHWEAVE_CONFIG` overrides the configuration file location;
`settings.output`, `settings.known_hosts`, and SSH `ControlPath` can still
explicitly point elsewhere. Relative configuration paths are resolved against
the TOML file's directory.

Old locations are not migrated automatically. Move existing configuration
manually and update explicit paths and the Include in `~/.ssh/config`.
SSH private keys and OpenSSH's own `known_hosts` stay in their original locations.

`${HOME}/.config/sshweave/conf.d/*.toml` is loaded automatically without an
includes declaration. This path is added to the root configuration's effective
includes list, visible with `config json`. This fixed directory is loaded even
when using `-c` / `SSHWEAVE_CONFIG`; a missing directory or no matches is allowed.

```toml
# Optional: additional configuration outside the default conf.d directory.
includes = ["extra.d/*.toml", "${HOME}/.config/sshweave/shared.toml"]

[settings]
output = "~/.config/sshweave/sshweave.conf"
known_hosts = "~/.config/sshweave/sshweave_known_hosts.json"
ssh_binary = "ssh"
automatic_rewrite = true
# binary = "/absolute/path/to/sshweave"  # Defaults to the current executable's absolute path.

[defaults]
user = "${USER}"
port = 22
connect_timeout = 10

[templates.production]
user = "deploy"

[templates.production.ssh]
IdentityFile = ["~/.ssh/id_ed25519", "~/.ssh/company_key"]
IdentitiesOnly = true

[hosts.gateway]
hostname = "gateway.example.net"
aliases = ["gate"]

[hosts.database]
inherits = ["production"]
hostname = "10.0.0.10"
gateways = ["direct", "gateway"]
gateway_strategy = "sequential"
rate_limit = "5 MiB/s"

[hosts.database.ssh]
LocalForward = ["8080 localhost:80", "9090 localhost:90"]
```

### Loading, inheritance, and matching

- Includes are relative to the including file and support glob patterns, `~`,
  and `${VAR}`. Matches are sorted and deduplicated by canonical path.
  The default conf.d may be empty; other unmatched includes, missing files,
  cycles, and invalid TOML are errors.
- Entries with the same name merge by field: root file > later include >
  earlier include. The default conf.d is prepended, so explicit includes can
  override it. If explicitly declared, that default path keeps its declared
  position. Within conf.d, files are sorted by name and later files take
  precedence. Only the root file supplies settings; other files supply
  defaults/templates/hosts/proxies/includes.
- Inheritance can be nested, with a protective depth limit of 64:
  the host itself > first parent > subsequent parents. Templates are searched
  before hosts; use `template:name` / `host:name` to disambiguate.
  Cycles and missing parents are errors; unused templates are validated too.
- Host resolution: exact host name > exact alias > glob/regex matches in
  declaration order > defaults. Later matches only fill unset fields.
  Templates are not directly connectable hosts. Undeclared ordinary host names
  can still connect directly with defaults applied.
- Globs support `*`, `?`, and character sets. Regexes require an explicit
  `regex:` prefix and must match the entire name. A regex hostname can use
  `{1}`, `{2}`, and `{named_capture}` substitutions.
- `aliases` are entry names for a host and are not inherited. Without a
  hostname, a literal alias points to the canonical host name; a pattern rule
  uses the input name.
- Lists replace rather than append: a child's `ssh.IdentityFile` replaces the
  parent's list. `gateways = []` disables inherited gateways; `[]` for a hook
  event disables that event. SSH option names are case-insensitive, and
  case-variant duplicates within an entry are errors.
- Validation, export, and resolution do not run commands. `${VAR}` expands in
  paths, hostname, user, SSH options, and runtime command arguments; missing
  variables are errors. `$VAR` is left untouched to avoid interfering with shell
  commands. `~otheruser` is unsupported.

```toml
[hosts."*.dev"]
hostname = "%n.example.net"

[hosts."regex:^db-(?P<region>[a-z]+)-([0-9]+)$"]
hostname = "{region}-{2}.internal"
user = "dbadmin"
```

### SSH options and runtime commands

Put ordinary OpenSSH options in `[hosts.NAME.ssh]`, `[templates.NAME.ssh]`, or
`[defaults.ssh]`. Values can be strings, integers, booleans (rendered as yes/no),
or string arrays (one directive per item). Use a comma-separated string for
algorithm lists and arrays for repeatable options such as `IdentityFile` and
`LocalForward`.

Strings are OpenSSH **argument expressions**. Quote paths containing spaces
using OpenSSH syntax, for example `IdentityFile = '"~/.ssh/company key"'`.
Newlines and NUL characters cannot inject additional directives.
sshweave does not maintain an SSH-option allowlist that could become outdated;
the local OpenSSH installation checks unknown options.
`Host`, `Match`, and `Include` are not allowed in ssh tables.
Use typed fields for Hostname/User/Port, runtime fields and gateways for
ProxyCommand/ProxyJump, and `connect_timeout` for ConnectTimeout
(passed to both OpenSSH and the proxy runtime).

```toml
[hosts.dynamic]
resolve_command = ["my-inventory", "address", "%n"]
connect_timeout = 10
proxy_command = ["nc", "%h", "%p"]
control_master_mkdir = true

[hosts.dynamic.ssh]
ControlMaster = "auto"
ControlPath = "~/.config/sshweave/control/%C"
```

- `resolve_command` is an argv array, executed without a shell. Its stdout must
  contain exactly one hostname/IP. Nonzero exit, invalid UTF-8, empty output,
  multiple hosts, and timeout are errors.
- `proxy_command` is also an argv array. It runs locally for direct connections
  or as a remote command on the gateway, with shell-quoted arguments. Choose
  nc, socat, or a custom transport explicitly. Without it, native TCP / `ssh -W`
  is used without implicitly trying additional tools.
- Command arguments support `%h` (resolved hostname), `%n` (original target),
  `%p`, `%r`, `%g` (current gateway), `%u`, `%i` (local UID), `%d`, `%l`, `%L`,
  `%C`, and `%%`. ControlPath supports the same local-host tokens.
  `%C` uses OpenSSH's SHA-1 combination. Ordinary ssh options rendered for
  OpenSSH retain OpenSSH's own token semantics.
- `connect_timeout` is in seconds, defaults to 10, and can be disabled with 0.
  It covers TCP connection establishment (including DNS), resolve_command, and
  proxy startup. A proxy process is considered established only after receiving
  the destination's first byte, suitable for SSH services that send a banner
  first; that byte is preserved. If a proxy closes stdout without exiting, the
  startup timeout also bounds the wait for exit, so later routes are not blocked
  indefinitely.
- With `control_master_mkdir = true`, missing ControlPath parent directories
  are created with mode 0700.
- Target names accept ordinary DNS/IP/alias characters, not shell expressions,
  whitespace, or option injection. Put usernames in `user` or OpenSSH's
  `user@host`, not in host keys.

### Gateways and failover

`gateway_strategy` can be set on hosts, templates, and defaults, with the same
inheritance, include-merge, and pattern rules as other fields. It is inherited
independently of `gateways`.
**The default strategy is `random`**, so list order is not guaranteed.
Configurations depending on an order, such as direct-first connections, must
explicitly set `gateway_strategy = "sequential"`.

| Strategy           | Meaning of `gateways`                                                  | On failure                               |
| ------------------ | ---------------------------------------------------------------------- | ---------------------------------------- |
| `random` (default) | Shuffle all candidate routes for each connection; try each item once   | Try remaining candidates                 |
| `sequential`       | Try candidate routes in list order                                     | Try subsequent candidates                |
| `chain`            | Traverse every jump host in order: local → first → second → … → target | Fail the whole chain; never bypass a hop |

```toml
[hosts.balanced]
hostname = "10.0.0.10"
gateways = ["gateway-a", "gateway-b"]  # Defaults to random.

[hosts.fallback]
hostname = "10.0.0.10"
gateway_strategy = "sequential"
gateways = ["direct", "gateway-a", "gateway-b"]

[hosts.private]
hostname = "10.0.0.20"
gateway_strategy = "chain"
gateways = ["outer", "inner"]  # Local → outer → inner → private.
```

An empty list, including `gateways = []`, means no SSH jump hosts for every
strategy and disables inherited gateways. A separately configured `proxy` still
applies. `direct` is a direct candidate for random/sequential. Each chain item
must be a single host name or alias: neither `direct` nor `/` chains are allowed.
Gateways may have their own strategies and gateways.
A random/sequential candidate can itself be a complete `inner/outer` chain.

An ad-hoc chain `destination/inner/outer` connects locally to outer, then inner,
then destination. It overrides both the destination's gateways **and strategy**;
explicit chain order is never randomized. The `/` syntax is written from the
destination outwards, the reverse of the traversal order in a chain list.

Compilation expands static chains and required suffixes so that each hop uses
its own authentication settings. The wrapper or automatic updates instantiate
dynamic glob/regex chains. `config resolve` and `config list --json` expose the
resolved `gateway_strategy`. The `routes` in `connect --dry-run` show deterministic
candidate lists or normalized chains such as `inner/outer`, without shuffling,
commands, or networking. Random ordering happens only at connection time and
does not trigger configuration rewrites. Graphviz gateway edges show the
effective strategy; chain edges point to normalized chains.

### Terminal route topology (no connection)

```sh
sshweave topology database
sshweave topology database/inner/outer
sshweave topology worker-eu-42 --ascii
sshweave -c ./sshweave.toml topology database > route.txt
```

The `ptree` terminal-tree library renders complete candidate paths from the
local machine to the target, without Graphviz or a full-screen interface.
Aliases, glob/regex, inheritance, ad-hoc chains, and nested gateways are supported.
Each SSH node shows its name and `user@hostname:port`; HTTP/SOCKS5 proxies appear
before the first hop. For example, `gateways = ["outer", "inner"]` with the chain
strategy produces:

```text
SSH route topology: database [preview only]
└── Local (this machine)
    └── Route 1 | database: chain #1 via inner/outer
        └── outer  [SSH ops@gateway.example.net:22]
            └── inner  [SSH ops@10.0.0.2:22]
                └── database  [SSH ops@10.0.0.10:22]
```

Candidates appear as separate Route branches, not as one chain.
`sequential` falls back in configuration order; `random` is displayed in fixed
configuration order for comparison, **not in actual connection-attempt order**.
`chain` traverses every jump host. Nested candidates expand into complete paths.
Cycles, excessive depth, or more than 4096 expanded nodes produce explicit
errors rather than silent truncation.

This command performs static configuration resolution only. It **does not open
SSH/TCP connections, probe availability, execute hooks, `resolve_command`, or
`proxy_command`, or write generated configuration, host caches, or control-socket
directories**. Dynamic nodes are marked as having addresses not yet confirmed by
their command. Custom transports show only whether their command runs locally or
on a gateway; their internal network paths cannot be inferred.
The diagram describes sshweave routes, not IP traceroute, and does not merge
external hand-written OpenSSH configuration or existing multiplexed sessions.
Terminal output is automatically colored; redirected output has no ANSI colors.
`NO_COLOR`, `TERM=dumb`, and `--ascii` are supported.

### Standalone HTTP / SOCKS5 proxies

Define proxies centrally in top-level `proxies` and reference them by name with
the host's `proxy` field. **Do not put proxy URLs or `proxy+...` in `gateways`**:
gateways accept only `direct` and SSH hosts/chains.
`direct` means no SSH jump host; it does not disable a configured proxy.

```toml
[proxies.local-http]
url = "http://127.0.0.1:7890"

[proxies.local-socks]
url = "socks5://127.0.0.1:7890"

[hosts.proxied]
hostname = "server.internal"
proxy = "local-http"  # Local → HTTP CONNECT proxy → server.internal.

[hosts.proxied-chain]
hostname = "database.internal"
proxy = "local-socks"
gateway_strategy = "chain"
gateways = ["outer", "inner"]
# Local → SOCKS5 proxy → outer → inner → database.internal.
```

- Unauthenticated HTTP CONNECT and SOCKS5 TCP CONNECT are supported without
  external nc/curl processes. URLs require an explicit port (1–65535) and support
  DNS, IPv4, and `[IPv6]`. One trailing `/` is allowed, but paths, queries,
  fragments, credentials, and other schemes are not. Unsupported authentication
  produces an explicit error, not a silent fallback.
- `proxy` supports defaults, templates, includes, aliases, and pattern
  inheritance like other host fields. Named proxy definitions use root >
  later include > earlier include precedence. `config check` rejects missing
  references, invalid URLs, and unknown fields.
- The target's selected proxy is used only from the local machine to the target
  or outermost SSH gateway. Nested gateways keep that same selection rather
  than overriding it with an intermediate gateway's `proxy`; no remote proxy
  client is required.
- Proxy failure is explicit; sshweave **never bypasses the proxy with a direct
  connection**. Gateway strategies can try other candidates, but each keeps the
  selected proxy. `connect_timeout` covers proxy DNS/TCP establishment, handshake,
  and the destination's first byte; 0 disables it. HTTP/SOCKS5 handshake bytes
  never enter the SSH stream.
- HTTP CONNECT and SOCKS5 delegate destination DNS resolution to the proxy,
  not the local machine. Targets support ASCII hostnames, IPv4, and IPv6.
- The same transport segment cannot combine `proxy` and `proxy_command`; this
  is an explicit configuration error. A remote `proxy_command` executed through
  an SSH gateway can still be combined with a local first-hop proxy.
- Proxy definitions are not written as SSH hosts into generated configuration or
  the host cache. `config json` exports them; `host.options.proxy` in
  `config resolve` / `connect --dry-run` shows the reference name. Graphviz
  displays proxies as separate nodes.
- Internal gateway SSH connections disable multiplexing when a proxy is in use,
  avoiding reuse of old direct gateway connections. Existing outer OpenSSH
  target sessions still follow their multiplexing settings; close their control
  connections after switching proxies if you need a new end-to-end route.

### Transport failure and rate limiting

Route failover happens only during connection establishment, before the
destination's first byte. Once session forwarding starts, errors are returned
without reconnecting and mixing in another SSH byte stream.
Static gateway cycles are rejected at load time; dynamic chains also have
cross-process visit-stack and depth protection.

`rate_limit = "5 MiB/s"` caps the **combined send and receive traffic** for one
proxy connection, not 5 MiB in each direction. B/KB/MB/GB and KiB/MiB/GiB are
supported, with an optional `/s`. This is not a global quota across processes or
sessions; a defaults value is applied separately to each session.

## Hooks

Six events are available: `before_connect`, `on_connect`, `on_connect_error`,
`on_disconnect`, `before_config_write`, and `after_config_write`.
The first four belong to hosts/templates/defaults. Configuration-write events
use the resolved, inherited defaults. Hooks within an event run in order;
failures are reported, not silently ignored.

```toml
[defaults.hooks]
before_config_write = [
  { driver = "write", message = "Writing {{ ssh_config_path }}" },
]

[hosts.database.hooks]
before_connect = [
  { driver = "write", message = "Connecting {{ host.name }} via {{ gateway }}" },
]
on_connect_error = [
  { driver = "write", message = "Route {{ gateway }} failed: {{ error }}" },
]
on_disconnect = [
  { driver = "exec", command = "printf '%s\\n' \"$SSHWEAVE_CONTEXT\" >> \"$HOME/sshweave-events.jsonl\"" },
]
```

| Driver | Field   | Behavior                                                          |
| ------ | ------- | ----------------------------------------------------------------- |
| write  | message | Write to stderr without contaminating SSH stdout                  |
| exec   | command | Run `/bin/sh -c`, wait, and report nonzero exit                   |
| daemon | command | Run in a background process group; log failures to stderr         |
| notify | message | Send a desktop notification with system commands; report failures |

Templates use MiniJinja, not Go templates. Undefined variables are errors.
Connection context contains `host` (name/hostname/user/port/options, etc.),
`gateway` (connection-attempt events), `stats` (connect/disconnect events), or
`error` (failure/disconnect events).
Stats include connected_at/disconnected_at (Unix seconds), sent_bytes,
received_bytes, duration_seconds, and average_bytes_per_second.
`on_connect` means the TCP/proxy transport is established, not that SSH
authentication has completed. `on_disconnect` follows the proxy stream's
lifetime, not an individual multiplexed session.

exec/daemon also receive the JSON environment variable `SSHWEAVE_CONTEXT`.
When inserting text into shell templates, use `{{ host.name | shellquote }}` or
read the JSON environment variable.
Configuration is **trusted executable input**; do not load untrusted TOML/hooks.

## Commands

```sh
sshweave -c ./sshweave.toml config check
sshweave config build                         # OpenSSH configuration to stdout
sshweave config build --write                 # Atomically update settings.output
sshweave config build --no-automatic-rewrite
sshweave config json                          # Merged configuration
sshweave config json --expand                 # Resolved concrete hosts
sshweave config list
sshweave config list --json
sshweave config search production
sshweave config resolve database/bastion
sshweave config graphviz | dot -Tsvg > hosts.svg
sshweave connect --dry-run database            # Routes only; no connections, hooks, or commands
sshweave topology database                    # Terminal diagram; no connection
sshweave ping --count 4 --interval 0.5 database # TCP latency/success; no gateways or proxies
sshweave sockets list
sshweave sockets master database
sshweave sockets flush                        # ssh -O exit, not just socket-file removal
sshweave info
sshweave completions zsh
```

Global options: `--config`/`-c`, `--verbose`/`-v`, and `--debug`.
`SSHWEAVE_CONFIG`, `SSHWEAVE_DEBUG`, and standard `RUST_LOG` are supported.
Arguments such as `-v` after the wrapped program name (ssh/scp/sftp/rsync)
are passed to that program; put sshweave global options before `wrapper`.

The wrapper manages `-F` and rsync's remote shell. Conflicting configuration
files or rsync `-e` are unsupported; attached `-e` arguments and grouped short
options are checked too. Valid rsync `-F` is passed through normally.
For more complex OpenSSH/rsync invocations, use the generated file with native
commands.

ping defaults to 4 probes; `--count 0` runs continuously. Ctrl+C prints statistics
for completed probes. With no successful probes, it returns failure.
sockets lists only actual Unix sockets, ignoring ordinary files in the same
directory. Literal `*`, `?`, and `[]` in ControlPath are not expanded as globs;
only dynamic host tokens are used for matching.

## Architecture and maintenance

```text
main.rs      clap CLI, application use cases, subprocess exit codes
config.rs    TOML model, include loading, validation, inheritance
resolver.rs  Pure host resolution, patterns/aliases/captures, gateway validation, tokens
render.rs    Deterministic OpenSSH / Graphviz compilation
topology.rs  Read-only route topology, full candidate expansion, terminal trees
store.rs     Host registry, file locks, 0600 temporary files, fsync + atomic replacement
runtime.rs   TCP/proxy transport, startup detection, bidirectional relay, limits, stats
proxy.rs     Proxy URL validation, unauthenticated HTTP CONNECT / SOCKS5 handshakes
hooks.rs     Templates, events, drivers (always isolated from protocol stdout)
sockets.rs   Control-socket discovery and lifecycle
util.rs      Strict environment/path expansion, input and unit validation
```

Generated configuration emits concrete hosts first and excludes instantiated
hosts from wildcard rules, avoiding duplicate accumulated IdentityFile or
LocalForward options in OpenSSH. The registry is not OpenSSH's `known_hosts`
host-key database: it stores instantiated names, not private keys or credentials.
Existing user SSH files are not deleted or cleaned up.

Run these checks from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Tests cover merging, precedence, patterns, cycles, invalid input, file protection,
atomic updates, concurrent rebuilds, hook errors, byte-level transport,
half-closes, early remote disconnects, shared rate-limit thresholds,
random/sequential gateway failover, chain expansion and overrides, wrapper
arguments/exit codes, and real `ssh -G` and generated ProxyCommand execution.
Proxy tests cover HTTP/SOCKS5 handshakes, remote DNS, IPv4/IPv6, binary streams and
half-closes, malformed/rejected responses, timeouts, no direct proxy bypass,
and combinations of proxies with multiple SSH jump hosts.
Integration tests use local TCP and controlled proxy processes, not external
SSH servers or login credentials.
CI runs these checks on Linux/macOS with stable Rust and the minimum supported
Rust 1.88. External gateway authentication, real multihop SSH sessions, and
desktop notifications still require validation in the deployment environment.

### Commits and pushes

The repository is `git@github.com:mingcheng/sshweave.git`, with `main` as its
default branch. Before committing, run the checks above and inspect staged
changes with `git status --short` and `git diff --cached --check`.
Commit `Cargo.lock` alongside application source so local builds and CI use the
same dependency versions.

The repository uses UTF-8 and LF; use `cargo fmt` for Rust formatting.
Build directories (including symlinks), local editor settings, environment files,
and common private-key files are ignored. Ignore rules do not replace manual
review: never commit real SSH configuration, host inventories, or credentials.
For a first push, use `git push -u origin main` after creating a local commit,
confirming that the remote repository exists, and configuring GitHub SSH
authentication.

Released under the MIT license. The design is inspired by assh, with its
copyright attribution retained; see [LICENSE](../LICENSE).

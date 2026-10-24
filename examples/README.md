# sshweave configuration and usage guide

**English** | [简体中文](README_CN.md) · [Product overview](../README.md)

This guide covers configuring hosts, choosing routes, and using sshweave with
SSH, SCP, SFTP, and rsync. OpenSSH handles authentication and host-key verification.

## Contents

- [Contents](#contents)
- [Install and get started](#install-and-get-started)
  - [Release archives](#release-archives)
- [Daily use](#daily-use)
  - [Shell aliases](#shell-aliases)
  - [OpenSSH Include and automatic updates](#openssh-include-and-automatic-updates)
- [TOML configuration](#toml-configuration)
  - [Loading, inheritance, and matching](#loading-inheritance-and-matching)
  - [SSH options and runtime commands](#ssh-options-and-runtime-commands)
- [Gateways and failover](#gateways-and-failover)
  - [Terminal route topology (no connection)](#terminal-route-topology-no-connection)
  - [Standalone HTTP / SOCKS5 proxies](#standalone-http--socks5-proxies)
  - [Timeouts and rate limits](#timeouts-and-rate-limits)
- [Hooks](#hooks)
- [Commands](#commands)
  - [Connection sharing](#connection-sharing)
  - [Managing materialized targets](#managing-materialized-targets)

## Install and get started

Use macOS or Linux with OpenSSH installed. Install `rsync` separately if needed.
To install with Cargo, Rust 1.88+ is required:

```sh
cargo install sshweave --locked
```

From a local source checkout, use `cargo install --path . --locked` at the
repository root instead. To install without Rust, use a [release archive](#release-archives).

Create the configuration directory:

```sh
mkdir -p ~/.config/sshweave
```

Save [sshweave.toml](sshweave.toml) as `~/.config/sshweave/sshweave.toml`, or merge
it into an existing configuration. Replace the example domains, addresses,
usernames, and key paths before connecting. Then validate, preview, and connect:

```sh
sshweave config check
sshweave topology database
sshweave config build --write
sshweave wrapper ssh database
```

Run `config check` after editing configuration. It checks configuration, not
remote availability; `topology` is also an offline preview.

### Release archives

[Published releases](https://github.com/mingcheng/sshweave/releases) can include
`sshweave-VERSION-TARGET.tar.gz` and matching `.sha256` files for:

| Target                      | Platform                                    |
| --------------------------- | ------------------------------------------- |
| `x86_64-unknown-linux-gnu`  | Linux x86_64, built on Ubuntu 24.04         |
| `aarch64-unknown-linux-gnu` | Linux ARM64, built on Ubuntu 24.04          |
| `x86_64-apple-darwin`       | macOS Intel, deployment target 13.0         |
| `aarch64-apple-darwin`      | macOS Apple Silicon, deployment target 13.0 |

The binary still requires OpenSSH but not Rust. Linux archives
require a compatible glibc; use a source build on older systems or musl/Alpine.
macOS archives are not Developer ID signed or notarized. Environments enforcing
those requirements need an approved signing/distribution process or source
installation; do not disable system security checks.

Download the archive and its checksum from the same release. Replace `VERSION`
and `TARGET` below with the selected values:

```sh
# Linux:
sha256sum -c sshweave-VERSION-TARGET.tar.gz.sha256
# macOS:
shasum -a 256 -c sshweave-VERSION-TARGET.tar.gz.sha256
tar -xzf sshweave-VERSION-TARGET.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 sshweave-VERSION-TARGET/sshweave "$HOME/.local/bin/sshweave"
```

Add `$HOME/.local/bin` to PATH if needed. Archives include this guide, the
example TOML, the license, and Bash/Fish/Zsh completions. Install completion
files in your shell's completion directory.

## Daily use

The recommended wrapper workflow updates configuration **before** OpenSSH reads
it and passes through program arguments, remote commands, and exit codes:

```sh
sshweave wrapper ssh -p 2222 -v database uname -a
sshweave wrapper scp file.txt database:/tmp/
sshweave wrapper sftp database
sshweave wrapper rsync -az source/ database:/srv/
```

The wrapper uses the generated file with `-F`, not your usual `~/.ssh/config`.
Put required host settings in TOML, or use the [Include workflow](#openssh-include-and-automatic-updates)
to combine generated and hand-written configuration.

Arguments after the program name belong to that program: SSH uses `-p` for a
port, while SCP/SFTP use `-P`. Put sshweave options before `wrapper`, for example
`sshweave --debug wrapper ssh database`; use `sshweave help wrapper` for help.

The wrapper manages SSH configuration, routing, and rsync's remote shell.
Conflicting SSH/SCP/SFTP `-F`/`-J`, routing overrides through `-o`, and rsync
`-e`/`--rsh` are rejected. Use TOML `gateways` or a `target/gateway` chain instead
of `-J`. For other configuration or routing overrides, use native commands with
the generated file.

### Shell aliases

Optionally add these aliases to `~/.bashrc` or `~/.zshrc`:

```sh
alias ssh='sshweave wrapper ssh'
alias scp='sshweave wrapper scp'
alias sftp='sshweave wrapper sftp'
alias rsync='sshweave wrapper rsync'
```

Then use `ssh database`, `scp file.txt database:/tmp/`, and similar commands.
Use `command ssh database` to bypass an alias temporarily. Bash login shells may
need to source `~/.bashrc` from `~/.bash_profile`.
Set `SSHWEAVE_CONFIG` if the aliases should use a custom TOML file.
Aliases affect only the current shell, not Git or GUI applications.

### OpenSSH Include and automatic updates

After `sshweave config build --write`, add this near the top of your
**hand-maintained** `~/.ssh/config`:

```sshconfig
Include ~/.config/sshweave/sshweave.conf
```

This enables native SSH/SCP/SFTP/rsync and OpenSSH-based Git/GUI applications.
OpenSSH uses the first effective value, so Include placement affects precedence.
The generated `Host *` applies sshweave routing to unlisted destinations too.
Put hand-written settings requiring a different proxy before the Include, or use
a dedicated configuration with `ssh -F`.

After configuration changes or the first use of a new pattern/chain target,
this workflow may update the generated file and ask you to rerun SSH. The wrapper
avoids that retry. Setting `settings.automatic_rewrite = false` disables these
automatic updates; explicit builds and wrappers still refresh configuration.

sshweave only overwrites generated files carrying its own marker. It **does not
automatically modify your hand-written `~/.ssh/config`**.
`config build` writes to stdout by default; prefer `--write`. Do not redirect
stdout into the same file being read or managed.

## TOML configuration

See [sshweave.toml](sshweave.toml) for a complete working configuration structure.

The default file is `~/.config/sshweave/sshweave.toml`. Use `-c` before the
subcommand or set `SSHWEAVE_CONFIG` to select another file. Relative
configuration paths resolve from the TOML file's directory.

`~/.config/sshweave/conf.d/*.toml` is loaded automatically only when the root
file is `~/.config/sshweave/sshweave.toml`. The directory may be empty or absent.
A file selected with `-c` or `SSHWEAVE_CONFIG` does not load conf.d unless it
lists that include. Use `includes` for other files.
Generated configuration and the target cache use the paths below by default.
Update your OpenSSH Include if you change `settings.output`.

The following examples are configuration fragments. Add them to your file or
merge them into existing tables; do not duplicate TOML table headers.

```toml
# includes = ["extra.d/*.toml"]

[settings]
output = "~/.config/sshweave/sshweave.conf"
registry = "~/.config/sshweave/sshweave_known_hosts.json"
ssh_binary = "ssh"
automatic_rewrite = true

[defaults]
user = "${USER}"
port = 22
connect_timeout = 10

[templates.production]
user = "deploy"

[templates.production.ssh]
IdentityFile = ["~/.ssh/id_ed25519", "~/.ssh/company_key"]
IdentitiesOnly = true

[hosts.bastion]
hostname = "bastion.example.net"
aliases = ["gate"]

[hosts.database]
inherits = ["production"]
hostname = "10.0.0.10"
gateways = ["direct", "bastion"]
gateway_strategy = "sequential"
rate_limit = "5 MiB/s"

[hosts.database.ssh]
LocalForward = ["8080 localhost:80", "9090 localhost:90"]
```

### Loading, inheritance, and matching

- Includes support glob patterns, `~`, and `${VAR}`. Files are sorted by name.
  Unlike the default conf.d, explicit include patterns must match existing files.
  Only the root file supplies `[settings]`.
- Same-name entries merge by field: root file > later include > earlier include.
  The automatic conf.d is loaded before explicit includes unless you list it
  yourself at a different position.
- Inheritance: host settings > first parent > subsequent parents. Parents may
  inherit too. Templates are searched before hosts; use `template:name` or
  `host:name` to disambiguate. Missing parents and cycles are errors.
- Matching: exact host > exact alias > glob/regex rules in declaration order >
  defaults. Later matches only fill unset fields. Unlisted names still use
  defaults; templates are not directly connectable hosts.
- Globs support `*`, `?`, and character sets. Regexes require an explicit
  `regex:` prefix and must match the entire name. A regex hostname can use
  `{1}`, `{2}`, and `{named_capture}` substitutions.
- `aliases` are not inherited. Without `hostname`, a literal alias points to
  the host's configured name; a pattern rule uses the requested name.
- Lists replace rather than append: a child's `ssh.IdentityFile` replaces the
  parent's list. `gateways = []` disables inherited gateways; `[]` for a hook
  event disables that event.
- `${VAR}` expands in paths, hostname, user, SSH options, and command arguments;
  missing variables are errors. `$VAR` is left for shell commands.
  `~otheruser` is unsupported.

```toml
[hosts."*.dev"]
hostname = "%n.example.net"

[hosts."regex:^worker-(?P<region>[a-z]+)-([0-9]+)$"]
inherits = ["production"]
hostname = "worker-{2}.{region}.internal"
gateways = ["bastion"]
```

### SSH options and runtime commands

Put ordinary OpenSSH options in `[hosts.NAME.ssh]`, `[templates.NAME.ssh]`, or
`[defaults.ssh]`. Values can be strings, integers, booleans (rendered as yes/no),
or string arrays (one directive per item). Use a comma-separated string for
algorithm lists and arrays for repeatable options such as `IdentityFile` and
`LocalForward`.

Quote paths containing spaces using OpenSSH syntax, for example
`IdentityFile = '"~/.ssh/company key"'`. Option names are case-insensitive;
duplicates are errors. Your installed OpenSSH determines which options it supports.
Do not put `Host`, `Match`, or `Include` in SSH tables. Use `hostname`, `user`,
`port`, and `connect_timeout` for those settings, and `gateways`, `proxy`, or
`proxy_command` for routing rather than SSH `ProxyJump`/`ProxyCommand`.

For dynamic addresses or a custom transport, optionally configure command arrays:

```toml
[hosts.dynamic]
resolve_command = ["my-inventory", "address", "%n"]
connect_timeout = 10
proxy_command = ["nc", "%h", "%p"]
```

- `resolve_command` runs without a shell and must print exactly one hostname/IP.
  Failed commands, invalid output, and timeouts are errors.
- `proxy_command` runs locally for direct access or on the SSH gateway when one
  is used. Install the selected tool where it runs. Ordinary connections do not
  need `nc` or `socat`.
- Common tokens are `%h` (hostname), `%n` (requested target), `%p` (port),
  `%r` (SSH user), and `%g` (gateway). Local tokens include `%u` (user),
  `%i` (UID), `%d` (home), `%l`/`%L` (full/short hostname), `%C` (connection hash),
  and `%%` (literal percent). Ordinary SSH options use OpenSSH's token rules.
- Put usernames in `user` or the command-line `user@host`, not in TOML host keys.

## Gateways and failover

Set `gateways` and `gateway_strategy` on hosts, templates, or defaults.
They are inherited independently. **The default strategy is `sequential`**,
so candidates are tried in list order. Set `gateway_strategy = "random"` to
shuffle them on each connection.

| Strategy               | Meaning of `gateways`                                                  | On failure                               |
| ---------------------- | ---------------------------------------------------------------------- | ---------------------------------------- |
| `sequential` (default) | Try candidate routes in list order                                     | Try subsequent candidates                |
| `random`               | Shuffle all candidate routes for each connection; try each item once   | Try remaining candidates                 |
| `chain`                | Traverse every jump host in order: local → first → second → … → target | Fail the whole chain; never bypass a hop |

```toml
[hosts.balanced]
hostname = "10.0.0.20"
gateways = ["bastion", "edge"]

[hosts.fallback]
hostname = "10.0.0.10"
gateway_strategy = "sequential"
gateways = ["direct", "bastion"]

[hosts.private]
hostname = "10.0.0.30"
gateway_strategy = "chain"
gateways = ["edge", "bastion"]
```

Here `bastion` and `edge` are the hosts from the [sample configuration](sshweave.toml).
`gateways = []` removes inherited jump hosts. For `chain`, every item must be a
single SSH host name or alias; `direct` and `/` chains are not allowed.
Random/sequential candidates may be `direct`, a host, or a chain such as
`bastion/edge`. Gateways can have their own gateway settings.

For an ad-hoc route, write the destination first, then gateways from inner to outer:

```sh
sshweave wrapper ssh database/bastion/edge
```

This connects local → edge → bastion → database and overrides the destination's
configured gateways and strategy. The `/` order is the reverse of a `chain` list.
Failover only applies during connection establishment; it does not retry SSH
authentication failures or resume interrupted sessions.

### Terminal route topology (no connection)

```sh
sshweave topology database
sshweave topology database/bastion/edge
sshweave topology worker-eu-42 --ascii
```

The tree shows candidate paths, gateways, proxies, and each host's
`user@hostname:port`. Random candidates appear in configuration order, not actual
connection-attempt order. Use `--ascii` for plain branches or `NO_COLOR` to disable color.

This is a configuration preview: it does not connect, test availability, execute
hooks or commands, or write files. It does not inspect paths inside custom
commands, external OpenSSH configuration, or existing shared connections.

### Standalone HTTP / SOCKS5 proxies

Define proxies in `[proxies.NAME]` and reference the name with `proxy`.
**Do not put proxy URLs in `gateways`.**

```toml
[proxies.local-http]
url = "http://127.0.0.1:7890"

[proxies.local-socks]
url = "socks5://127.0.0.1:7890"

[hosts.proxied]
hostname = "server.internal"
proxy = "local-http"

[hosts.proxied-chain]
hostname = "database.internal"
proxy = "local-socks"
gateway_strategy = "chain"
gateways = ["edge", "bastion"]
```

- Only unauthenticated `http://` and `socks5://` proxies are supported. Include
  an explicit port (1–65535); use brackets around IPv6 addresses. URLs cannot
  contain credentials, queries, fragments, or paths other than a trailing `/`.
- `proxy` supports the same defaults, templates, and inheritance as other host
  settings. Missing proxy references and invalid URLs fail `config check`.
- The destination's proxy is used from the local machine to the target or
  outermost SSH gateway, including nested routes. Destination DNS is resolved
  by the proxy; no proxy client is needed on the gateway.
- `direct` and `gateways = []` remove SSH jumps, not the selected proxy.
  Proxy failures **never cause a silent direct connection**.
- Do not combine `proxy` and `proxy_command` on the same transport segment.
  A local proxy can still be combined with a command running on an SSH gateway.
- After changing proxies, close existing [shared connections](#connection-sharing)
  if you need subsequent sessions to use the new route.

### Timeouts and rate limits

`connect_timeout` is in seconds, defaults to 10, and is disabled with 0. It covers
connection setup, including DNS, `resolve_command`, and proxy startup.

`rate_limit = "5 MiB/s"` limits the **combined send and receive traffic** of each
connection. B/KB/MB/GB and KiB/MiB/GiB are supported, with optional `/s`.
A value in defaults applies separately to each connection, not as a global quota.

## Hooks

Six events are available: `before_connect`, `on_connect`, `on_connect_error`,
`on_disconnect`, `before_config_write`, and `after_config_write`.
Put connection events in hosts/templates/defaults and configuration-write events
in defaults. Write events run only when generated content changes. Hooks within
an event run in order; failures are reported.

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

| Driver | Field   | Behavior                                    |
| ------ | ------- | ------------------------------------------- |
| write  | message | Print a message to stderr                   |
| exec   | command | Run a shell command and wait for completion |
| daemon | command | Run a command in the background             |
| notify | message | Send a desktop notification                 |

Templates use Jinja-style syntax; undefined variables are errors. Available
values depend on the event: `host`, `gateway`, `stats`, and `error`.
Stats include `sent_bytes`, `received_bytes`, `duration_seconds`,
`average_bytes_per_second`, and `connected_at`/`disconnected_at` (Unix seconds).
Desktop notifications require `osascript` on macOS or `notify-send` on Linux.

`on_connect` means transport establishment, not successful SSH authentication.
A `notify` hook for that event is titled `sshweave: transport established`.
`on_disconnect` runs when the transport closes, not when each multiplexed SSH
session ends.

exec/daemon also receive the JSON environment variable `SSHWEAVE_CONTEXT`.
When inserting text into shell templates, use `{{ host.name | shellquote }}` or
read the JSON environment variable.
Configuration is **trusted executable input**; do not load untrusted TOML/hooks.

## Commands

| Task                                                | Command                                           |
| --------------------------------------------------- | ------------------------------------------------- |
| Validate a chosen configuration                     | `sshweave -c ./sshweave.toml config check`        |
| Print generated OpenSSH configuration               | `sshweave config build`                           |
| Update the managed configuration file               | `sshweave config build --write`                   |
| Generate configuration without automatic updates    | `sshweave config build --no-automatic-rewrite`    |
| List hosts, aliases, and patterns                   | `sshweave config list`                            |
| List resolved hosts as JSON                         | `sshweave config list --json`                     |
| Search hosts                                        | `sshweave config search production`               |
| Inspect a host or chain                             | `sshweave config resolve database/bastion`        |
| Show which entry supplied each field                | `sshweave config explain database`                |
| Export merged configuration                         | `sshweave config json`                            |
| Export resolved concrete hosts                      | `sshweave config json --expand`                   |
| Preview routes as JSON, without connecting          | `sshweave connect --dry-run database`             |
| Export relationships as Graphviz DOT                | `sshweave config graphviz`                        |
| Probe TCP latency of the first route hop            | `sshweave ping --count 4 --interval 0.5 database` |
| Check Include placement and print Git's SSH command | `sshweave doctor`                                 |
| Show version, paths, and loaded files               | `sshweave info`                                   |
| Generate shell completions                          | `sshweave completions zsh`                        |

Global options: `--config`/`-c`, `--verbose`/`-v`, and `--debug`.
`SSHWEAVE_CONFIG`, `SSHWEAVE_DEBUG`, and standard `RUST_LOG` are supported.
Use `sshweave <command> --help` for available options, except wrapper help,
which uses `sshweave help wrapper`.

`ping` times a TCP handshake to the first hop of the selected route: the
proxy, or the outermost gateway. It does not start `proxy_command`, authenticate,
or dial past that hop. `random` routes are probed in configuration order. `-p`
overrides the port only when the probe is the destination itself. It defaults to
4 probes; `--count 0` runs until Ctrl+C. No successful probes means a failure exit
status. `doctor` prints the Include line and `GIT_SSH_COMMAND`; an Include placed
after a `Host` or `Match` block is an error. `config build --write` lists added
and removed `Host` lines before replacing a managed file. SVG export with `sshweave config graphviz | dot -Tsvg > hosts.svg`
requires the optional Graphviz `dot` command.

### Connection sharing

To reuse connections and manage them with `sockets`, configure `ControlPath`:

```toml
[defaults]
control_master_mkdir = true

[defaults.ssh]
ControlMaster = "auto"
ControlPersist = "5m"
ControlPath = "~/.config/sshweave/control/%C"
```

`control_master_mkdir` creates missing control-socket directories when needed.
The [sample configuration](sshweave.toml) already enables this setup.

```sh
sshweave sockets list
sshweave sockets master database
sshweave sockets flush
```

`master` opens background SSH connections and may require authentication.
`flush` closes managed control connections and can interrupt sessions using
them. Existing shared connections may keep an old route after configuration
changes; close them when ready to reconnect.

### Managing materialized targets

Names used for dynamic hosts and chains are remembered in `settings.registry`
(`known_hosts` is still accepted as an alias).
This is a target cache, **not OpenSSH's host-key database**.
Use names from `cache list` in place of `old-worker` below:

```sh
sshweave cache list
sshweave cache remove old-worker --dry-run
sshweave cache remove old-worker
sshweave config build --write
```

Removal uses exact names, not patterns; unknown names are errors.
`--dry-run` previews changes without writing files or running hooks.
To clear all cached names, preview with `sshweave cache clear --dry-run`, then
use `sshweave cache clear` and rebuild. Add `--json` to `cache list` for scripts.

Removal updates only the cache. Rebuild or use a wrapper to refresh generated
configuration; configured hosts and gateways still needed by other routes remain.
Reconnecting can add a removed name again. No names are evicted automatically.
OpenSSH host keys, source TOML, private keys, and control sockets are not changed.

Generated configuration supports up to 4096 distinct targets, including configured
names, aliases, cached targets, and gateway-chain components. Remove unused cached
names if they cause this limit to be reached.

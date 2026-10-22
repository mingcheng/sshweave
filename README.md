<center>
<img src="./sshweave.webp" width="250" alt="sshweave logo">
<h1>sshweave</h1>
<p>Less SSH configuration. More reliable connections.</p>
<p>
English | <a href="README_CN.md">简体中文</a>
</p>
</center>

sshweave keeps your SSH hosts and connection routes organized as your infrastructure
grows. Manage shared settings in one place, reach hosts across private networks,
and keep using the tools you already know.

Written in Rust, sshweave turns TOML configuration into OpenSSH configuration and
provides its `ProxyCommand` transport. It does not reimplement the SSH protocol:
OpenSSH still handles authentication, host-key verification, and sessions. No
sshweave agent is required on remote hosts.

## Why sshweave?

- **Less repetition:** share defaults, templates, and inherited settings across
  hosts, and split configuration into reusable TOML includes.
- **Easier access:** use aliases, glob patterns, and regular expressions to manage
  both fixed hosts and dynamically named machines.
- **Flexible connections:** combine direct access, SSH jump hosts, and HTTP
  CONNECT/SOCKS5 proxies, with alternative routes or fixed multi-hop chains.
- **Clearer routes:** inspect resolved settings and preview complete connection
  paths in the terminal without connecting.
- **Familiar workflow:** keep using SSH, SCP, SFTP, and rsync through wrappers or
  an OpenSSH `Include`.
- **Operational tools:** add lifecycle hooks, limit per-connection traffic, and
  manage OpenSSH multiplexing sockets.
- **Stay in control:** manage generated configuration without automatically
  overwriting your hand-written SSH configuration.

## Who is it for?

Developers and operators managing multiple environments, private-network hosts,
or jump hosts who want a consistent SSH workflow without repetitive configuration.

## Get started

### Install

Runtime requirements: macOS or Linux with OpenSSH installed. Install `rsync`
separately if you want to use its wrapper.

The [`sshweave` crate](https://crates.io/crates/sshweave) provides the `sshweave`
command-line executable. Install it with Cargo (Rust 1.88+ required):

```sh
cargo install sshweave --locked
sshweave --help
```

Tagged releases also build macOS/Linux archives for x86_64 and ARM64, including
SHA-256 checksums, documentation, and shell completions. See
[GitHub Releases](https://github.com/mingcheng/sshweave/releases) for published
artifacts and the [installation guide](examples/README.md#release-archives) for
platform requirements and verification. Release binaries do not require Rust.

### Configure hosts

Create the configuration directory:

```sh
mkdir -p ~/.config/sshweave
```

Save the following as `~/.config/sshweave/sshweave.toml`, or merge it into an
existing configuration rather than overwriting it. Replace the example domain,
address, and usernames with your own before connecting.

```toml
[defaults]
user = "deploy"
connect_timeout = 3

[hosts.bastion]
hostname = "bastion.example.net"
user = "ops"

[hosts.database]
hostname = "10.0.0.10"
gateways = ["direct", "bastion"]
gateway_strategy = "sequential"
```

This tries the database directly first, then through the bastion. The default
gateway strategy is `random`; set `sequential` explicitly when order matters.
For a fixed chain through every jump host, use `chain` with SSH gateway names
only, without a `direct` entry; see
[gateways and failover](examples/README.md#gateways-and-failover).

### Validate and connect

```sh
sshweave config check
sshweave topology database
sshweave config build --write
sshweave wrapper ssh database
```

`config check` validates configuration, not remote availability. `topology`
previews routes without opening connections or executing hooks or commands.
`config build --write` writes `~/.config/sshweave/sshweave.conf` by default;
without `--write`, it prints to stdout.

The wrapper updates generated configuration **before** OpenSSH reads it, so
newly matched hosts and configuration changes take effect on the first attempt.
No change to `~/.ssh/config` is needed for this workflow.

The wrapper selects the generated file with `-F` instead of loading your usual
`~/.ssh/config`; put required host settings in TOML. To combine generated and
hand-written configuration, or use OpenSSH-based Git/GUI applications directly,
follow the [OpenSSH Include guide](examples/README.md#openssh-include-and-automatic-updates).

For a different TOML file, set `SSHWEAVE_CONFIG` or pass `-c` before the subcommand,
for example `sshweave -c ./sshweave.toml wrapper ssh database`.

## Everyday usage

Pass program arguments and remote commands after the wrapped program name:

```sh
sshweave wrapper ssh -v database uname -a
sshweave wrapper scp file.txt database:/tmp/
sshweave wrapper sftp database
sshweave wrapper rsync -az source/ database:/srv/
```

Put sshweave's own options before `wrapper`, for example
`sshweave --debug wrapper ssh database`. The wrapper manages SSH configuration
and routing, so conflicting options such as SSH `-F`/`-J` and rsync `-e` are
rejected; configure `gateways` in TOML instead.

| Task | Command |
| --- | --- |
| List hosts, aliases, and patterns | `sshweave config list` |
| Inspect a resolved host | `sshweave config resolve database` |
| Preview routes as JSON without connecting | `sshweave connect database --dry-run` |
| List OpenSSH control sockets (`ssh.ControlPath` required) | `sshweave sockets list` |
| List materialized target names, not SSH host keys | `sshweave cache list` |
| Show version, paths, and configuration statistics | `sshweave info` |
| Generate Zsh completions | `sshweave completions zsh` |

See the [command reference](examples/README.md#commands) for more commands and
wrapper restrictions, or the [shell alias examples](examples/README.md#shell-aliases)
to keep using familiar command names.

## Important behavior

- **Configuration ownership:** sshweave only overwrites generated files carrying
  its own marker. It does not automatically edit `~/.ssh/config`, move private
  keys, or replace OpenSSH's `known_hosts` database.
- **Failover scope:** alternative routes are tried only during transport
  establishment, before the destination's first byte. sshweave does not resume
  interrupted SSH sessions or retry SSH authentication failures.
- **Proxy support:** HTTP CONNECT and SOCKS5 currently support unauthenticated
  proxies only. A configured proxy is never silently bypassed with a direct
  connection.
- **Trusted configuration:** hooks and runtime commands can execute programs.
  Only load configuration files you trust.

## Documentation

- **[Setup and configuration guide](examples/README.md):** installation,
  inheritance, routing, proxies, hooks, commands, and maintenance.
- **[Complete example configuration](examples/sshweave.toml):** templates,
  patterns, gateway strategies, named proxies, and hooks in one TOML file.
- **[中文使用指南](examples/README_CN.md):** the configuration and usage guide
  in Simplified Chinese.

The English documentation is the reference version; the Chinese documentation
follows the same content.

## Credits and license

Author: mingcheng <mingcheng@apache.org>.

Inspired by [moul/assh](https://github.com/moul/assh). Released under the [MIT license](LICENSE).

<h1 align="center">
<img src="./sshweave.webp" width="250" alt="sshweave logo" />
<br/>
sshweave
</h1>
<p align="center">Less SSH configuration. More reliable connections.</p>
<p align="center">
English | <a href="README_CN.md">简体中文</a>
</p>

sshweave keeps your SSH hosts and connection routes organized as your infrastructure
grows. Manage shared settings in one place, reach hosts across private networks,
and keep using the tools you already know.

sshweave turns TOML configuration into OpenSSH configuration and
provides its `ProxyCommand` transport. It does not reimplement the SSH protocol:
OpenSSH still handles authentication, host-key verification, and sessions. So, no sshweave agent or daemon is required on remote hosts.

## Why sshweave?

- **Less repetition:** share defaults, templates, and inherited settings across
  hosts, and split configuration into reusable TOML includes.
- **Easier access:** use aliases, glob patterns, and regular expressions to manage
  both fixed hosts and dynamically named machines.
- **Flexible connections:** combine direct access, SSH jump hosts, and HTTP
  CONNECT/SOCKS5 proxies, with alternative routes or fixed multi-hop chains.
- **Clearer routes:** inspect resolved settings and preview complete connection
  paths in the terminal without connecting.
- **Familiar workflow:** keep using SSH, SCP, SFTP, and rsync through wrappers or an OpenSSH `Include`.
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

sshweave was written in Rust and is available as a Rust crate. The [crate](https://crates.io/crates/sshweave) provides the `sshweave`
command-line executable. Install it with Cargo:

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

This tries the database directly first, then through the bastion. Omitting
`gateway_strategy` does the same thing: the default is `sequential`. Set
`random` only when the candidates should be shuffled. For a fixed chain through
every jump host, use `chain` with SSH gateway names only, without a `direct`
entry; see [gateways and failover](examples/README.md#gateways-and-failover).

For the more advanced configuration options and examples, see the [examples guide](examples/README.md).

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

This project was inspired by [moul/assh](https://github.com/moul/assh). Thanks to the author for the inspiration.

And this project is maintained by mingcheng <mingcheng@apache.org> and released under the [MIT license](LICENSE).

Feel free to contribute or report issues on the project's GitHub repository.

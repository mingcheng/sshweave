# sshweave

**English** | [简体中文](README_CN.md)

**Less SSH configuration. More reliable connections.**

sshweave keeps your SSH hosts and connection routes organized as your infrastructure
grows. Manage shared settings in one place, reach hosts across private networks,
and keep using the tools you already know.

## Why sshweave?

- **Less repetition:** reuse common settings across hosts and environments instead
  of maintaining duplicate configuration.
- **Easier access:** give hosts memorable names and manage groups of machines
  consistently, including dynamically named hosts.
- **Flexible connections:** reach destinations directly, through jump hosts, or
  through network proxies, with alternative routes when a connection fails.
- **Clearer routes:** preview how a connection will reach its destination before
  connecting.
- **Familiar workflow:** keep using SSH, SCP, SFTP, and rsync, with OpenSSH handling
  authentication and host verification.
- **Stay in control:** manage generated configuration without automatically
  overwriting your hand-written SSH configuration.

## Who is it for?

Developers and operators managing multiple environments, private-network hosts,
or jump hosts who want a consistent SSH workflow without repetitive configuration.

## Get started

The [`sshweave` crate](https://crates.io/crates/sshweave) provides the `sshweave` command-line executable. Install it
with Cargo (Rust 1.88+ required):

```sh
cargo install sshweave --locked
sshweave --help
```

Runtime requirements: macOS or Linux with OpenSSH installed.

See the **[setup and configuration guide](examples/README.md)** for installation,
examples, and everyday usage. [中文使用指南](examples/README_CN.md) is also available.

## Credits and license

Author: mingcheng <mingcheng@apache.org>.

Inspired by [moul/assh](https://github.com/moul/assh). Released under the [MIT license](LICENSE).

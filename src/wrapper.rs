//! SSH-compatible argument handling and process invocation for the CLI.

use anyhow::{Context, Result, ensure};
use sshweave::{config::Config, runtime, store, util};
use std::process::Command;

/// Programs supported by `sshweave wrapper`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Program {
    Ssh,
    Scp,
    Sftp,
    Rsync,
}

impl Program {
    fn short_options_with_value(self) -> &'static str {
        match self {
            Self::Ssh => "BbcDEeFIiJLlmOopQRSWw",
            Self::Scp => "cDFiJloPSX",
            Self::Sftp => "BbcDFiJloPRSs",
            Self::Rsync => "BefMT@",
        }
    }
}

const RSYNC_LONG_OPTIONS_WITH_VALUE: &[&str] = &[
    "--exclude",
    "--include",
    "--filter",
    "--files-from",
    "--exclude-from",
    "--include-from",
    "--log-file",
    "--out-format",
    "--chmod",
    "--chown",
    "--bwlimit",
    "--timeout",
    "--contimeout",
    "--password-file",
    "--rsync-path",
    "--remote-option",
];

/// Locates destinations without interpreting or modifying the remote command.
fn targets(program: Program, args: &[String]) -> Result<Vec<String>> {
    let mut positionals = Vec::new();
    let mut skip = false;
    let mut pending_option = None;
    let mut options = true;
    let takes_value = program.short_options_with_value();
    for arg in args {
        if skip {
            if pending_option == Some('o') {
                check_ssh_override(arg)?;
            }
            skip = false;
            pending_option = None;
            continue;
        }
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg.starts_with('-') && arg != "-" {
            if program == Program::Rsync && arg.starts_with("--") {
                ensure!(
                    !["-e", "--rsh", "--config"]
                        .iter()
                        .any(|key| arg == key || arg.starts_with(&format!("{key}="))),
                    "wrapper manages rsync remote shell"
                );
                skip = RSYNC_LONG_OPTIONS_WITH_VALUE.contains(&arg.as_str());
            } else {
                // OpenSSH also accepts attached values and clusters such as -vvvp2222.
                let flags = &arg[1..];
                for (offset, flag) in flags.char_indices() {
                    if takes_value.contains(flag) {
                        ensure!(flag != 'F', "wrapper manages the SSH -F configuration");
                        ensure!(
                            flag != 'J',
                            "wrapper manages SSH routing; conflicting option -J (use configured gateways)"
                        );
                        ensure!(
                            !(program == Program::Rsync && flag == 'e'),
                            "wrapper manages rsync remote shell"
                        );
                        skip = offset + flag.len_utf8() == flags.len();
                        if flag == 'o' && !skip {
                            check_ssh_override(&flags[offset + 1..])?;
                        }
                        if skip {
                            pending_option = Some(flag);
                        }
                        break;
                    }
                }
            }
            continue;
        }
        positionals.push(arg.as_str());
        if program == Program::Ssh {
            break;
        }
    }
    ensure!(!skip, "missing option argument");
    let mut targets = Vec::new();
    for arg in positionals {
        if let Some(candidate) = destination(program, arg)? {
            let candidate = candidate.rsplit('@').next().context("missing host")?;
            util::valid_target(candidate)?;
            targets.push(candidate.into());
        }
    }
    Ok(targets)
}

fn check_ssh_override(value: &str) -> Result<()> {
    let key = value.split(['=', ' ', '\t']).next().unwrap_or(value);
    ensure!(
        !["proxycommand", "proxyjump", "include"].contains(&key.to_ascii_lowercase().as_str()),
        "wrapper manages SSH routing; conflicting option {value:?}"
    );
    Ok(())
}

/// Extracts a host from a destination or URI; local paths return `None`.
fn destination(program: Program, value: &str) -> Result<Option<&str>> {
    let uri = value.split_once("://");
    let authority = if let Some((scheme, rest)) = uri {
        if !["ssh", "scp", "sftp"].contains(&scheme) {
            return Ok(None);
        }
        rest.split('/').next().context("URI authority is missing")?
    } else {
        value
    };
    let host = authority.rsplit('@').next().context("missing host")?;
    if let Some(bracketed) = host.strip_prefix('[') {
        let (host, tail) = bracketed
            .split_once(']')
            .context("unclosed IPv6 destination")?;
        if uri.is_some() || matches!(program, Program::Ssh | Program::Sftp) || tail.starts_with(':')
        {
            return Ok(Some(host));
        }
        return Ok(None);
    }
    if uri.is_some() {
        return Ok(host.split(':').next());
    }
    if program == Program::Ssh {
        return Ok(Some(host));
    }
    if program == Program::Sftp {
        return Ok(host.split(':').next());
    }
    Ok(host
        .split_once(':')
        .map(|(host, _)| host)
        .filter(|h| !h.contains('/')))
}

/// Registers targets, refreshes configuration, and replaces this process with
/// the wrapped program so signals and exit statuses pass through unchanged.
pub(crate) async fn run(config: &Config, program: Program, args: Vec<String>) -> Result<i32> {
    ensure!(!args.is_empty(), "wrapper requires program arguments");
    let targets = targets(program, &args)?;
    let resolver = config.resolver()?;
    // Like `connect`, register only names that need their own stanza.
    let materialized: Vec<String> = targets
        .iter()
        .filter(|target| resolver.requires_materialization(target))
        .cloned()
        .collect();
    store::remember_with(&resolver, &materialized)?;
    store::sync_with(&resolver, config.document.settings.automatic_rewrite).await?;
    for target in &targets {
        runtime::prepare_control_path(&resolver.resolve(target)?)?;
    }
    let output = config.output()?;
    let settings = &config.document.settings;
    let mut command = Command::new(config.program(match program {
        Program::Ssh => &settings.ssh_binary,
        Program::Scp => &settings.scp_binary,
        Program::Sftp => &settings.sftp_binary,
        Program::Rsync => &settings.rsync_binary,
    })?);
    if program == Program::Rsync {
        let ssh_binary = config.ssh_binary()?;
        let rsh = shell_words::join([
            ssh_binary
                .to_str()
                .context("ssh_binary path is not UTF-8")?,
            "-F",
            output.to_str().context("output path is not UTF-8")?,
        ]);
        command.args(["-e", &rsh]);
    } else {
        command.arg("-F").arg(output);
    }
    command.args(args);
    exec(command)
}

#[cfg(unix)]
fn exec(mut command: Command) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    // `exec` only returns on failure.
    Err(command.exec()).context("execute wrapped program")
}

#[cfg(not(unix))]
fn exec(mut command: Command) -> Result<i32> {
    let status = command.status().context("execute wrapped program")?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_remote_command_and_finds_destination() -> Result<()> {
        let args = ["-vvvp", "2222", "alice@db/gateway", "echo", "-J", "remote"].map(str::to_owned);
        assert_eq!(targets(Program::Ssh, &args)?, vec!["db/gateway"]);
        Ok(())
    }

    #[test]
    fn finds_scp_and_rsync_endpoints() -> Result<()> {
        let args = ["-P", "2222", "file", "alice@db:/tmp/file"].map(str::to_owned);
        assert_eq!(targets(Program::Scp, &args)?, vec!["db"]);
        let args = ["--exclude", "ignored:pattern", "source/", "db:/srv/"].map(str::to_owned);
        assert_eq!(targets(Program::Rsync, &args)?, vec!["db"]);
        Ok(())
    }

    #[test]
    fn handles_rsync_short_option_arguments() -> Result<()> {
        for args in [
            vec!["-avf", "exclude:ignored", "source/", "db:/srv/"],
            vec!["-fexclude:ignored", "source/", "db:/srv/"],
            vec!["-T", "temp:dir", "source/", "db:/srv/"],
            vec!["-avF", "source/", "db:/srv/"],
        ] {
            assert_eq!(
                targets(
                    Program::Rsync,
                    &args.into_iter().map(str::to_owned).collect::<Vec<_>>()
                )?,
                vec!["db"]
            );
        }
        for option in ["-e", "-essh", "-avessh", "--rsh=ssh", "--config=other"] {
            let args = [option, "source/", "db:/srv/"].map(str::to_owned);
            assert!(targets(Program::Rsync, &args).is_err(), "{option}");
        }
        assert!(targets(Program::Rsync, &["-avf".into()]).is_err());
        Ok(())
    }

    #[test]
    fn handles_ipv6_uris_and_rejects_conflicting_config() -> Result<()> {
        for (program, input, expected) in [
            (Program::Scp, "alice@[::1]:/tmp", Some("::1")),
            (Program::Ssh, "ssh://alice@[::1]:2222", Some("::1")),
            (Program::Sftp, "sftp://alice@db:2222/path", Some("db")),
            (Program::Scp, "./local:filename", None),
            (Program::Scp, "directory/local:filename", None),
        ] {
            assert_eq!(destination(program, input)?, expected);
        }
        for args in [
            vec!["-vF", "other.conf", "db"],
            vec!["-o", "ProxyCommand=evil", "db"],
        ] {
            assert!(
                targets(
                    Program::Ssh,
                    &args.into_iter().map(str::to_owned).collect::<Vec<_>>()
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_all_jump_spellings_for_ssh_scp_and_sftp() {
        for program in [Program::Ssh, Program::Scp, Program::Sftp] {
            for args in [
                vec!["-J", "jump", "db"],
                vec!["-Jjump", "db"],
                vec!["-vJ", "jump", "db"],
                vec!["-vvJjump", "db"],
                vec!["-o", "ProxyJump=jump", "db"],
                vec!["-oProxyJump=jump", "db"],
                vec!["-vo", "proxyjump jump", "db"],
            ] {
                let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
                let error = targets(program, &args).unwrap_err();
                assert!(error.to_string().contains("wrapper manages SSH routing"));
            }
        }
    }
}

use crate::config::{Event, Host};
use anyhow::{Context, Result, ensure};
use minijinja::{Environment, UndefinedBehavior, value::Serde};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum Hook {
    Write { message: String },
    Exec { command: String },
    Daemon { command: String },
    Notify { message: String },
}

impl Hook {
    pub fn validate(&self) -> Result<()> {
        let source = match self {
            Self::Write { message } | Self::Notify { message } => message,
            Self::Exec { command } | Self::Daemon { command } => {
                ensure!(!command.trim().is_empty(), "hook command cannot be empty");
                command
            }
        };
        environment().add_template("hook", source)?;
        Ok(())
    }
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_filter("shellquote", |value: String| {
        shell_words::quote(&value).into_owned()
    });
    env
}

pub async fn invoke(host: &Host, event: Event, context: &Value) -> Result<()> {
    let Some(hooks) = host.hooks.get(&event) else {
        return Ok(());
    };
    for hook in hooks {
        let source = match hook {
            Hook::Write { message } | Hook::Notify { message } => message,
            Hook::Exec { command } | Hook::Daemon { command } => command,
        };
        let value = environment()
            .render_str(source, Serde(context))
            .with_context(|| format!("{event:?} hook template"))?;
        match hook {
            Hook::Write { .. } => eprintln!("{value}"),
            Hook::Exec { .. } => {
                let status = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&value)
                    .env("SSHWEAVE_CONTEXT", serde_json::to_string(context)?)
                    .stdin(Stdio::null())
                    .stdout(stderr_output()?)
                    .stderr(Stdio::inherit())
                    .status()
                    .await
                    .context("execute hook")?;
                ensure!(status.success(), "{event:?} hook exited with {status}");
            }
            Hook::Daemon { .. } => {
                let mut command = Command::new("/bin/sh");
                command
                    .arg("-c")
                    .arg(&value)
                    .env("SSHWEAVE_CONTEXT", serde_json::to_string(context)?)
                    .stdin(Stdio::null())
                    .stdout(stderr_output()?)
                    .stderr(Stdio::inherit());
                #[cfg(unix)]
                {
                    command.process_group(0);
                }
                let mut child = command.spawn().context("start daemon hook")?;
                tokio::spawn(async move {
                    match child.wait().await {
                        Ok(status) if status.success() => {}
                        Ok(status) => tracing::error!(%status, "daemon hook failed"),
                        Err(error) => tracing::error!(%error, "wait for daemon hook"),
                    }
                });
            }
            Hook::Notify { .. } => {
                #[cfg(target_os = "macos")]
                let mut command = {
                    let mut cmd = Command::new("osascript");
                    // Pass text as argv rather than interpolating AppleScript.
                    cmd.args(["-e", "on run argv\n display notification (item 1 of argv) with title \"sshweave\"\nend run", "--", &value]);
                    cmd
                };
                #[cfg(not(target_os = "macos"))]
                let mut command = {
                    let mut cmd = Command::new("notify-send");
                    cmd.args(["sshweave", &value]);
                    cmd
                };
                let status = command
                    .stdin(Stdio::null())
                    .stdout(stderr_output()?)
                    .stderr(Stdio::inherit())
                    .status()
                    .await
                    .context("send desktop notification")?;
                ensure!(
                    status.success(),
                    "notification command exited with {status}"
                );
            }
        }
    }
    Ok(())
}

// Hook output must never contaminate the SSH protocol on stdout.
fn stderr_output() -> Result<Stdio> {
    use std::os::fd::AsFd;
    Ok(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?))
}

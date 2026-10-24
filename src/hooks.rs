//! Lifecycle hooks: MiniJinja templates rendered with an event context and
//! dispatched to a driver.
//!
//! Hook output is always redirected to stderr, because sshweave's stdout may
//! be carrying the SSH protocol when it runs as a ProxyCommand.

use crate::config::{Event, Host};
use anyhow::{Context, Result, ensure};
use minijinja::{Environment, UndefinedBehavior, value::Serde};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use tokio::process::Command;

/// Name of the environment variable carrying the JSON event context.
const CONTEXT_ENV: &str = "SSHWEAVE_CONTEXT";

/// A hook action; the `driver` field selects the variant.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum Hook {
    /// Print the rendered message to stderr.
    Write {
        /// Message template.
        message: String,
    },
    /// Run a shell command and wait; a non-zero exit fails the operation.
    Exec {
        /// Shell command template.
        command: String,
    },
    /// Start a shell command in its own process group without waiting.
    Daemon {
        /// Shell command template.
        command: String,
    },
    /// Show a desktop notification (`osascript` on macOS, `notify-send` elsewhere).
    Notify {
        /// Message template.
        message: String,
    },
}

impl Hook {
    /// Returns the template source rendered for this hook.
    fn template(&self) -> &str {
        match self {
            Self::Write { message } | Self::Notify { message } => message,
            Self::Exec { command } | Self::Daemon { command } => command,
        }
    }

    /// Checks that the hook is usable and its template compiles.
    pub fn validate(&self) -> Result<()> {
        if let Self::Exec { command } | Self::Daemon { command } = self {
            ensure!(!command.trim().is_empty(), "hook command cannot be empty");
        }
        environment().add_template("hook", self.template())?;
        Ok(())
    }

    /// Renders the template and runs the driver.
    async fn run(&self, event: Event, context: &Value) -> Result<()> {
        let value = environment()
            .render_str(self.template(), Serde(context))
            .with_context(|| format!("{event:?} hook template"))?;
        match self {
            Self::Write { .. } => eprintln!("{value}"),
            Self::Exec { .. } => {
                let status = shell(&value, context)?
                    .status()
                    .await
                    .context("execute hook")?;
                ensure!(status.success(), "{event:?} hook exited with {status}");
            }
            Self::Daemon { .. } => {
                let mut command = shell(&value, context)?;
                // Detach from the terminal's process group so Ctrl+C does not reach it.
                #[cfg(unix)]
                command.process_group(0);
                let mut child = command.spawn().context("start daemon hook")?;
                tokio::spawn(async move {
                    match child.wait().await {
                        Ok(status) if status.success() => {}
                        Ok(status) => tracing::error!(%status, "daemon hook failed"),
                        Err(error) => tracing::error!(%error, "wait for daemon hook"),
                    }
                });
            }
            Self::Notify { .. } => {
                let status = notification(event, &value)
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
        Ok(())
    }
}

/// Template environment; undefined variables are errors, not empty strings.
fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_filter("shellquote", |value: String| {
        shell_words::quote(&value).into_owned()
    });
    env
}

/// Runs every hook registered on `host` for `event`, in order, stopping at the
/// first failure.
pub async fn invoke(host: &Host, event: Event, context: &Value) -> Result<()> {
    for hook in host.hooks.get(&event).into_iter().flatten() {
        hook.run(event, context).await?;
    }
    Ok(())
}

/// Builds a `/bin/sh -c` command with the JSON context in its environment.
fn shell(script: &str, context: &Value) -> Result<Command> {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(script)
        .env(CONTEXT_ENV, serde_json::to_string(context)?)
        .stdin(Stdio::null())
        .stdout(stderr_output()?)
        .stderr(Stdio::inherit());
    Ok(command)
}

/// Title shown by the desktop notification. `on_connect` fires when the byte
/// stream is up, before OpenSSH authentication finishes.
fn notification_title(event: Event) -> &'static str {
    match event {
        Event::OnConnect => "sshweave: transport established",
        _ => "sshweave",
    }
}

#[cfg(target_os = "macos")]
fn notification(event: Event, message: &str) -> Command {
    let mut command = Command::new("osascript");
    // Pass text as argv rather than interpolating it into AppleScript source.
    command.args([
        "-e",
        "on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\nend run",
        "--",
        notification_title(event),
        message,
    ]);
    command
}

#[cfg(not(target_os = "macos"))]
fn notification(event: Event, message: &str) -> Command {
    let mut command = Command::new("notify-send");
    command.args([notification_title(event), message]);
    command
}

/// Duplicates stderr so hook output never contaminates the SSH protocol on stdout.
fn stderr_output() -> Result<Stdio> {
    use std::os::fd::AsFd;
    Ok(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?))
}

use std::path::Path;

use anyhow::{Context, Result, bail};
use thiserror::Error;
use tokio::process::Command;

use super::control::CommandRunner;

pub const EVENT_CHANNEL: &str = "tmux-recover:state-changed";
pub const EVENT_COMMAND: &str = "wait-for -S tmux-recover:state-changed";

const STRUCTURE_HOOKS: &[&str] = &[
    "after-kill-pane",
    "after-new-session",
    "after-new-window",
    "after-rename-session",
    "after-rename-window",
    "after-resize-pane",
    "after-resize-window",
    "after-select-layout",
    "after-select-pane",
    "after-select-window",
    "after-split-window",
    "client-attached",
    "client-detached",
    "client-session-changed",
    "session-closed",
    "session-created",
    "session-renamed",
    "session-window-changed",
    "window-linked",
    "window-unlinked",
];

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("{message}")]
    Legacy { message: String },
    #[error("{message}")]
    Occupied { message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Installs persistent event hooks without ever replacing an occupied slot.
///
/// Hooks are tmux array options, so `set-option -o` is an atomic set-if-absent
/// operation inside the tmux server. An identical hook left by an earlier
/// daemon is reusable because it signals a stable channel rather than naming
/// an ephemeral control client.
pub async fn install(
    client: &mut CommandRunner,
    hook_slot: u16,
) -> std::result::Result<(), InstallError> {
    let mut legacy = Vec::new();
    let mut occupied = Vec::new();
    for hook in STRUCTURE_HOOKS {
        let name = format!("{hook}[{hook_slot}]");
        if let Some(command) = value(client, &name).await? {
            if is_legacy_event_command(&command) {
                legacy.push(name);
            } else if command != EVENT_COMMAND {
                occupied.push(name);
            }
        }
    }
    if !legacy.is_empty() {
        return Err(InstallError::Legacy {
            message: format!(
                "legacy tmux-recover hooks block migration: {}. Stop any old daemon, then explicitly unset only these entries on the target socket or choose another autosave.hook_slot; automatic removal is disabled so a concurrently replaced hook cannot be deleted",
                legacy.join(", ")
            ),
        });
    }
    if !occupied.is_empty() {
        return Err(InstallError::Occupied {
            message: format!(
                "tmux hook slots are already occupied and were not overwritten: {}",
                occupied.join(", ")
            ),
        });
    }

    for hook in STRUCTURE_HOOKS {
        let name = format!("{hook}[{hook_slot}]");
        let command = format!("set-option -go {name} \"{EVENT_COMMAND}\"");
        match client.execute(&command).await {
            Ok(output) if output.is_empty() => {}
            Ok(output) => {
                return Err(anyhow::anyhow!(
                    "tmux returned {} records while installing hook {name}, expected none",
                    output.len()
                )
                .into());
            }
            Err(set_error) => {
                let existing = value(client, &name).await?;
                match existing.as_deref() {
                    Some(command) if is_legacy_event_command(command) => {
                        return Err(InstallError::Legacy {
                            message: format!(
                                "legacy tmux-recover hook {name} appeared during installation and was not removed; stop any old daemon, explicitly unset it on the target socket, or choose another autosave.hook_slot"
                            ),
                        });
                    }
                    Some(EVENT_COMMAND) => {}
                    Some(_) => {
                        return Err(InstallError::Occupied {
                            message: format!(
                                "tmux hook {name} is already occupied and was not overwritten: {set_error:#}"
                            ),
                        });
                    }
                    None => {
                        return Err(anyhow::anyhow!(
                            "failed to install tmux hook {name}, but its slot is still empty: {set_error:#}"
                        )
                        .into());
                    }
                }
            }
        }

        match value(client, &name).await?.as_deref() {
            Some(EVENT_COMMAND) => {}
            Some(command) if is_legacy_event_command(command) => {
                return Err(InstallError::Legacy {
                    message: format!(
                        "legacy tmux-recover hook {name} appeared while installation was being verified and was not removed; stop any old daemon, explicitly unset it on the target socket, or choose another autosave.hook_slot"
                    ),
                });
            }
            Some(_) => {
                return Err(InstallError::Occupied {
                    message: format!(
                        "tmux hook {name} was occupied by another command while installation was being verified"
                    ),
                });
            }
            None => {
                return Err(anyhow::anyhow!(
                    "tmux hook {name} disappeared while installation was being verified"
                )
                .into());
            }
        }
    }
    Ok(())
}

/// Waits for one latched structural event from the persistent hook set.
pub async fn wait_for_event(socket: &Path) -> Result<()> {
    let mut command = Command::new("tmux");
    command
        .env_remove("TMUX")
        .arg("-S")
        .arg(socket)
        .args(["wait-for", EVENT_CHANNEL])
        .kill_on_drop(true);
    let output = command
        .output()
        .await
        .context("failed to execute tmux wait-for")?;
    if !output.status.success() {
        bail!(
            "tmux hook event waiter failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn value(client: &mut CommandRunner, name: &str) -> Result<Option<String>> {
    let output = client.execute(&format!("show-hooks -g {name}")).await?;
    if output.len() != 1 {
        bail!(
            "tmux returned {} records for hook {name}, expected one",
            output.len()
        );
    }
    parse_value(name, &output[0])
}

fn parse_value(name: &str, line: &[u8]) -> Result<Option<String>> {
    let line = String::from_utf8(line.to_vec()).context("tmux returned a non-UTF-8 hook")?;
    let value = line
        .strip_prefix(&format!("{name} "))
        .with_context(|| format!("tmux returned an invalid hook record for {name}"))?;
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn is_legacy_event_command(command: &str) -> bool {
    let Some(client) = command
        .strip_prefix("display-message -c ")
        .and_then(|command| command.strip_suffix(" tmux-recover:state-changed"))
    else {
        return false;
    };
    !client.is_empty() && !client.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_the_requested_hook_record() {
        assert_eq!(
            parse_value(
                "after-new-window[901]",
                b"after-new-window[901] wait-for -S tmux-recover:state-changed"
            )
            .unwrap()
            .as_deref(),
            Some(EVENT_COMMAND)
        );
        assert_eq!(
            parse_value("after-new-window[901]", b"after-new-window[901] ").unwrap(),
            None
        );
        assert!(
            parse_value(
                "after-new-window[901]",
                b"after-new-window[902] display-message external"
            )
            .is_err()
        );
    }

    #[test]
    fn recognizes_only_the_complete_legacy_event_command() {
        assert!(is_legacy_event_command(
            "display-message -c /dev/pts/4 tmux-recover:state-changed"
        ));
        assert!(!is_legacy_event_command(
            "display-message external-tmux-recover:state-changed-hook"
        ));
        assert!(!is_legacy_event_command(
            "display-message -c client tmux-recover:state-changed trailing"
        ));
    }
}

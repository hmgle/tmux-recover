use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use super::control::ControlClient;

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
    "client-session-changed",
    "session-closed",
    "session-created",
    "session-renamed",
    "session-window-changed",
    "window-linked",
    "window-unlinked",
];

/// Installs persistent event hooks without ever replacing an occupied slot.
///
/// Hooks are tmux array options, so `set-option -o` is an atomic set-if-absent
/// operation inside the tmux server. An identical hook left by an earlier
/// daemon is reusable because it signals a stable channel rather than naming
/// an ephemeral control client.
pub async fn install(client: &mut ControlClient, hook_slot: u16) -> Result<()> {
    for hook in STRUCTURE_HOOKS {
        let name = format!("{hook}[{hook_slot}]");
        let command = format!("set-option -go {name} \"{EVENT_COMMAND}\"");
        match client.execute(&command).await {
            Ok(output) if output.is_empty() => {}
            Ok(output) => {
                bail!(
                    "tmux returned {} records while installing hook {name}, expected none",
                    output.len()
                );
            }
            Err(set_error) => {
                let existing = value(client, &name).await?;
                if existing.as_deref() != Some(EVENT_COMMAND) {
                    bail!(
                        "tmux hook {name} is already occupied and was not overwritten: {set_error:#}"
                    );
                }
            }
        }

        if value(client, &name).await?.as_deref() != Some(EVENT_COMMAND) {
            bail!("tmux hook {name} changed while installation was being verified");
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

async fn value(client: &mut ControlClient, name: &str) -> Result<Option<String>> {
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
}

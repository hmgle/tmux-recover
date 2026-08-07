pub mod capture;
pub mod control;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

pub async fn resolve_socket(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = crate::util::socket_from_tmux_env() {
        return Ok(path);
    }
    let output = Command::new("tmux")
        .env_remove("TMUX")
        .args(["display-message", "-p", "#{socket_path}"])
        .output()
        .await
        .context("failed to execute tmux")?;
    if !output.status.success() {
        anyhow::bail!(
            "could not resolve the default tmux socket: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(PathBuf::from(
        String::from_utf8(output.stdout)
            .context("tmux returned a non-UTF-8 socket path")?
            .trim(),
    ))
}

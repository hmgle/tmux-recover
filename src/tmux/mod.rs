pub mod capture;
pub mod control;
pub mod hooks;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

pub async fn resolve_socket(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = if let Some(path) = explicit {
        path.to_path_buf()
    } else if let Some(path) = crate::util::socket_from_tmux_env() {
        path
    } else {
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
        PathBuf::from(
            String::from_utf8(output.stdout)
                .context("tmux returned a non-UTF-8 socket path")?
                .trim(),
        )
    };
    crate::util::canonical_socket_path(&path)
}

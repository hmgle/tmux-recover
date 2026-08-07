use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::model::SocketIdentity;

pub fn hostname() -> Result<String> {
    nix::unistd::gethostname()
        .context("could not read hostname")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("hostname is not valid UTF-8"))
}

pub fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

pub fn socket_from_tmux_env() -> Option<PathBuf> {
    std::env::var_os("TMUX").and_then(|value| {
        let value = value.to_string_lossy();
        value
            .split(',')
            .next()
            .filter(|part| !part.is_empty())
            .map(PathBuf::from)
    })
}

pub fn socket_identity(path: &Path) -> Result<SocketIdentity> {
    SocketIdentity::new(path, &hostname()?, uid())
}

pub fn require_tmux_37(version: &str) -> Result<()> {
    let value = version
        .trim()
        .strip_prefix("tmux ")
        .unwrap_or(version.trim());
    let numeric: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect();
    let mut parts = numeric.split('.');
    let major = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    if (major, minor) < (3, 7) {
        bail!("tmux 3.7 or newer is required, found {version}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_tmux_patch_suffixes() {
        require_tmux_37("tmux 3.7b").unwrap();
        require_tmux_37("3.8").unwrap();
        assert!(require_tmux_37("tmux 3.6a").is_err());
    }
}

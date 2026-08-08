use std::{
    fs,
    path::{Path, PathBuf},
};

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
    let value = std::env::var_os("TMUX")?;
    #[cfg(unix)]
    {
        use std::{
            ffi::OsString,
            os::unix::ffi::{OsStrExt, OsStringExt},
        };

        let bytes = value.as_os_str().as_bytes();
        let socket = bytes.split(|byte| *byte == b',').next()?;
        (!socket.is_empty()).then(|| PathBuf::from(OsString::from_vec(socket.to_vec())))
    }
    #[cfg(not(unix))]
    {
        value
            .to_string_lossy()
            .split(',')
            .next()
            .filter(|part| !part.is_empty())
            .map(PathBuf::from)
    }
}

pub fn canonical_socket_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path)
        .with_context(|| format!("could not resolve tmux socket {}", path.display()))
}

pub fn socket_identity(path: &Path) -> Result<SocketIdentity> {
    SocketIdentity::new(&canonical_socket_path(path)?, &hostname()?, uid())
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
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn accepts_tmux_patch_suffixes() {
        require_tmux_37("tmux 3.7b").unwrap();
        require_tmux_37("3.8").unwrap();
        assert!(require_tmux_37("tmux 3.6a").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_socket_paths_collapse_symlink_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("socket");
        let alias = directory.path().join("alias");
        std::fs::write(&socket, b"").unwrap();
        symlink(&socket, &alias).unwrap();

        assert_eq!(
            canonical_socket_path(&alias).unwrap(),
            canonical_socket_path(&socket).unwrap()
        );
    }
}

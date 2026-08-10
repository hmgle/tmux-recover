use std::process::Command;

use tempfile::TempDir;
use tmux_recover::{
    config::StorageConfig, model::SnapshotSource, storage::SnapshotStore, util::socket_identity,
};

struct Server {
    _directory: TempDir,
    socket: std::path::PathBuf,
}

impl Server {
    fn start() -> Option<Self> {
        let version = Command::new("tmux").arg("-V").output().ok()?;
        if !version.status.success()
            || tmux_recover::util::require_tmux_37(&String::from_utf8_lossy(&version.stdout))
                .is_err()
        {
            return None;
        }
        let directory = tempfile::tempdir().ok()?;
        let socket = directory.path().join("tmux.sock");
        let config = directory.path().join("tmux.conf");
        std::fs::write(&config, "set -g default-shell /bin/sh\n").ok()?;
        let status = Command::new("tmux")
            .args(["-S"])
            .arg(&socket)
            .arg("-f")
            .arg(&config)
            .args(["new-session", "-d", "-s", "plugin-start"])
            .status()
            .ok()?;
        status.success().then_some(Self {
            _directory: directory,
            socket,
        })
    }

    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-S"]).arg(&self.socket);
        command
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").status();
    }
}

#[test]
fn plugin_start_synchronously_initializes_only_an_empty_store() {
    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let environment = tempfile::tempdir().unwrap();
    let data_home = environment.path().join("data");
    let state_home = environment.path().join("state");
    let config_home = environment.path().join("config");
    let tmux_environment = format!("{},0,0", server.socket.display());

    let start = || {
        Command::new("sh")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/start-daemon.sh"
            ))
            .env("TMUX_RECOVER_BIN", env!("CARGO_BIN_EXE_tmux-recover"))
            .env("TMUX", &tmux_environment)
            .env("XDG_DATA_HOME", &data_home)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_CONFIG_HOME", &config_home)
            .output()
            .unwrap()
    };

    let output = start();
    assert!(
        output.status.success(),
        "plugin startup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(
        &data_home.join("tmux-recover"),
        &identity.key,
        &StorageConfig::default(),
    );
    let initial = store.load_current().unwrap();
    assert_eq!(
        initial.source,
        SnapshotSource::Native {
            reason: "initial".to_owned()
        }
    );

    assert!(
        server
            .tmux()
            .args([
                "rename-window",
                "-t",
                "plugin-start:0",
                "must-not-replace-initial",
            ])
            .status()
            .unwrap()
            .success()
    );
    let output = start();
    assert!(output.status.success());
    assert_eq!(store.load_current().unwrap().id, initial.id);
}

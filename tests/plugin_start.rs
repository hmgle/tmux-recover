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

    fn stop(&self) -> bool {
        let _ = self.tmux().arg("kill-server").output();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if self
                .tmux()
                .arg("has-session")
                .output()
                .is_ok_and(|output| !output.status.success())
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[test]
fn plugin_start_synchronously_initializes_only_an_empty_store() {
    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let environment = tempfile::tempdir().unwrap();
    let data_dir = environment.path().join("data");
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
            // `directories` ignores XDG_DATA_HOME on macOS, so use the CLI's
            // explicit cross-platform override for an isolated snapshot store.
            .env("TMUX_RECOVER_DATA_DIR", &data_dir)
            .env("TMUX", &tmux_environment)
            .env("HOME", environment.path())
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("RUST_LOG", "info")
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
    let store = SnapshotStore::for_socket(&data_dir, &identity.key, &StorageConfig::default());
    let initial = store.load_current().unwrap();
    assert_eq!(
        initial.source,
        SnapshotSource::Native {
            reason: "initial".to_owned()
        }
    );
    let daemon_lock = store.root().join("daemon.lock");
    let daemon_log = state_home.join("tmux-recover/tpm.log");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&daemon_log)
            .is_ok_and(|log| log.contains("tmux-recover daemon is watching server"))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached daemon never completed initialization"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let daemon_pid = std::fs::read_to_string(&daemon_lock).unwrap();
    assert!(!daemon_pid.trim().is_empty());

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
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(
        std::fs::read_to_string(&daemon_lock).unwrap(),
        daemon_pid,
        "config reload replaced the live daemon lock owner"
    );

    assert!(server.stop(), "isolated tmux server did not stop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if store.acquire_daemon_lock().is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached daemon did not release its lock after server shutdown"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

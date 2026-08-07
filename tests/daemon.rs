use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::{
    config::{AutosaveConfig, Config, RestoreConfig},
    model::{Snapshot, SnapshotSource},
    storage::SnapshotStore,
    tmux::{capture::capture, control::ControlClient},
    util::socket_identity,
};

struct TestServer {
    directory: TempDir,
    socket: std::path::PathBuf,
}

impl TestServer {
    fn start() -> Option<Self> {
        Self::start_with_command(Some("sleep 60"))
    }

    fn start_shell() -> Option<Self> {
        Self::start_with_command(None)
    }

    fn start_with_command(start_command: Option<&str>) -> Option<Self> {
        let version = Command::new("tmux").arg("-V").output().ok()?;
        if !version.status.success()
            || tmux_recover::util::require_tmux_37(&String::from_utf8_lossy(&version.stdout))
                .is_err()
        {
            return None;
        }
        let directory = tempfile::tempdir().ok()?;
        let socket = directory.path().join("tmux.sock");
        let mut command = Command::new("tmux");
        command.args(["-S"]).arg(&socket).args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "daemon",
        ]);
        if let Some(start_command) = start_command {
            command.arg(start_command);
        }
        let status = command.status().ok()?;
        status.success().then_some(Self { directory, socket })
    }

    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-S"]).arg(&self.socket);
        command
    }

    fn stop(&self) {
        let _ = self.tmux().arg("kill-server").status();
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn polling_saves_cwd_changes_without_structure_hooks() {
    let Some(server) = TestServer::start_shell() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(50),
            poll_interval: Duration::from_millis(80),
            ..AutosaveConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(3), || store.has_current()).await;
    let cwd = server.directory.path().join("poll cwd");
    std::fs::create_dir(&cwd).unwrap();
    let shell_command = format!("cd -- '{}'", cwd.display());
    let output = server
        .tmux()
        .args(["send-keys", "-t", "daemon:0.0", "-l"])
        .arg(shell_command)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        server
            .tmux()
            .args(["send-keys", "-t", "daemon:0.0", "Enter"])
            .status()
            .unwrap()
            .success()
    );
    wait_until(Duration::from_secs(3), || {
        store.load_current().is_ok_and(|snapshot| {
            snapshot.state.windows[0].panes[0]
                .cwd
                .path
                .as_ref()
                .is_some_and(|path| path.to_path_buf().is_ok_and(|path| path == cwd))
        })
    })
    .await;
    assert!(store.list().unwrap().len() >= 2);

    task.abort();
    let _ = task.await;
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tokio::test]
async fn auto_restore_only_replaces_a_young_shell_bootstrap() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(80),
            poll_interval: Duration::from_secs(5),
            ..AutosaveConfig::default()
        },
        restore: RestoreConfig {
            auto: true,
            ..RestoreConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let captured = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    let source = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )
    .unwrap();
    store.commit(&source, true).unwrap();

    server.stop();
    assert!(
        server
            .tmux()
            .args(["-f", "/dev/null", "new-session", "-d", "-s", "bootstrap"])
            .status()
            .unwrap()
            .success()
    );
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(3), || {
        let output = server
            .tmux()
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();
        output.is_ok_and(|output| output.status.success() && output.stdout == b"daemon\n")
    })
    .await;
    assert_eq!(
        store.load_current().unwrap().state.sessions[0].name,
        "daemon"
    );
    assert_eq!(store.load(&source.id).unwrap().id, source.id);

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn hook_event_saves_changed_state() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(80),
            poll_interval: Duration::from_secs(5),
            ..AutosaveConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(3), || store.has_current()).await;
    let initial = store.load_current().unwrap();
    assert_eq!(initial.state.windows[0].panes.len(), 1);

    let output = server
        .tmux()
        .args(["split-window", "-d", "-t", "daemon:0"])
        .arg("sleep 60")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(Duration::from_secs(3), || {
        store
            .load_current()
            .is_ok_and(|snapshot| snapshot.state.windows[0].panes.len() == 2)
    })
    .await;
    assert_eq!(store.list().unwrap().len(), 2);

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn auto_restore_preflight_failure_does_not_kill_the_daemon() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(80),
            poll_interval: Duration::from_secs(5),
            ..AutosaveConfig::default()
        },
        restore: RestoreConfig {
            auto: true,
            ..RestoreConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let mut captured = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    // Point the pane at a cwd that no longer exists so preflight fails.
    let missing = server.directory.path().join("gone-before-restore");
    captured.state.windows[0].panes[0].cwd = tmux_recover::model::PaneCwd::inspect(Some(
        tmux_recover::model::EncodedPath::from(missing.as_os_str()),
    ));
    let source = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )
    .unwrap();
    store.commit(&source, true).unwrap();

    server.stop();
    assert!(
        server
            .tmux()
            .args(["-f", "/dev/null", "new-session", "-d", "-s", "bootstrap"])
            .status()
            .unwrap()
            .success()
    );
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    // The failed auto-restore must not tear the daemon down: the bootstrap
    // session stays put, and the daemon keeps watching and autosaving it.
    wait_until(Duration::from_secs(3), || {
        store.load_current().is_ok_and(|snapshot| {
            snapshot
                .state
                .sessions
                .first()
                .is_some_and(|session| session.name == "bootstrap")
        })
    })
    .await;
    assert!(
        !task.is_finished(),
        "daemon exited after a failed auto-restore"
    );

    task.abort();
    let _ = task.await;
}

async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

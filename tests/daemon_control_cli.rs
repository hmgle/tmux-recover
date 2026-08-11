use std::{
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

use tempfile::TempDir;
use tmux_recover::{
    config::StorageConfig, daemon_control::DaemonStatus, storage::SnapshotStore,
    util::socket_identity,
};

struct TestServer {
    _directory: TempDir,
    socket: std::path::PathBuf,
}

impl TestServer {
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
            .args(["new-session", "-d", "-s", "daemon-control"])
            .status()
            .ok()?;
        status.success().then_some(Self {
            _directory: directory,
            socket,
        })
    }

    fn stop(&self) {
        let _ = Command::new("tmux")
            .args(["-S"])
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

struct IsolatedCli {
    command_config: TempDir,
    data: TempDir,
}

impl IsolatedCli {
    fn new() -> Self {
        let command_config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(command_config.path().join("config")).unwrap();
        std::fs::create_dir(command_config.path().join("runtime")).unwrap();
        std::fs::write(
            command_config.path().join("invalid.toml"),
            "this is not valid TOML = [",
        )
        .unwrap();
        Self {
            data: tempfile::tempdir().unwrap(),
            command_config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tmux-recover"));
        command
            .env("HOME", self.command_config.path())
            .env("XDG_CONFIG_HOME", self.command_config.path().join("config"))
            .env(
                "XDG_RUNTIME_DIR",
                self.command_config.path().join("runtime"),
            )
            .env("XDG_STATE_HOME", self.command_config.path().join("state"))
            .arg("--data-dir")
            .arg(self.data.path());
        command
    }

    fn run(&self, socket: &std::path::Path, action: &str, extra: &[&str]) -> Output {
        let mut command = self.command();
        command
            .arg("--config")
            .arg(self.command_config.path().join("invalid.toml"))
            .arg("daemon")
            .arg("--socket")
            .arg(socket)
            .arg(action)
            .args(extra);
        command.output().unwrap()
    }

    fn spawn_daemon(&self, socket: &std::path::Path) -> Child {
        let mut command = self.command();
        command
            .arg("daemon")
            .arg("--socket")
            .arg(socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().unwrap()
    }
}

#[test]
fn control_endpoint_is_available_while_the_initial_save_is_blocked() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let cli = IsolatedCli::new();
    let identity = socket_identity(&server.socket).unwrap();
    let store =
        SnapshotStore::for_socket(cli.data.path(), &identity.key, &StorageConfig::default());
    let mutation_lock = store.acquire_mutation_lock().unwrap();
    let mut daemon = cli.spawn_daemon(&server.socket);

    let status = wait_for_status(&cli, &server.socket);
    assert_eq!(status.pid, daemon.id());
    assert!(
        !store.has_current(),
        "initial save completed despite the held mutation lock"
    );

    drop(mutation_lock);
    let output = cli.run(&server.socket, "--stop", &[]);
    assert!(output.status.success(), "stop failed: {output:?}");
    let status = daemon.wait().unwrap();
    assert!(status.success(), "daemon did not exit cleanly: {status}");
}

#[test]
fn daemon_status_stop_and_reload_control_the_exact_instance() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let cli = IsolatedCli::new();
    let mut daemon = cli.spawn_daemon(&server.socket);

    let first = wait_for_status(&cli, &server.socket);
    let first_pid = first.pid;
    let output = cli.run(&server.socket, "--status", &["--json"]);
    assert!(output.status.success(), "status failed: {output:?}");
    let status: DaemonStatus = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status, first);

    let output = cli.run(&server.socket, "--reload", &[]);
    assert!(output.status.success(), "reload failed: {output:?}");
    let reloaded = wait_for_status_change(&cli, &server.socket, &first);
    assert_eq!(
        reloaded.pid, first_pid,
        "reload must preserve the supervisor PID"
    );
    assert_eq!(reloaded.version, env!("CARGO_PKG_VERSION"));

    let output = cli.run(&server.socket, "--stop", &[]);
    assert!(output.status.success(), "stop failed: {output:?}");
    let status = daemon.wait().unwrap();
    assert!(status.success(), "daemon did not exit cleanly: {status}");
}

fn wait_for_status(cli: &IsolatedCli, socket: &std::path::Path) -> DaemonStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let output = cli.run(socket, "--status", &["--json"]);
        if output.status.success() {
            return serde_json::from_slice(&output.stdout).unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon did not publish a control endpoint: {output:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_status_change(
    cli: &IsolatedCli,
    socket: &std::path::Path,
    previous: &DaemonStatus,
) -> DaemonStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let output = cli.run(socket, "--status", &["--json"]);
        if output.status.success() {
            let status: DaemonStatus = serde_json::from_slice(&output.stdout).unwrap();
            if status.started_at != previous.started_at {
                return status;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon did not return after reload: {output:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

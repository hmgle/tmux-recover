//! CLI-level cover for `save`'s dedup semantics, which live in main.rs and so
//! are not reachable from a library test.

use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::{config::StorageConfig, storage::SnapshotStore, util::socket_identity};

struct Server {
    /// Held so the socket outlives the server.
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
        // An interactive shell's own startup is real structural change: a
        // framework like oh-my-zsh cd's into its plugin directory while
        // sourcing, which moves the pane's cwd and so its structural hash.
        // These tests assert on exactly that hash, so pin the pane to a bare
        // `sh` with no rc file. `default-shell` has to be set at server start,
        // since naming the shell on the `new-session` command line would record
        // a `pane_start_command` instead.
        let config = directory.path().join("tmux.conf");
        std::fs::write(&config, "set -g default-shell /bin/sh\n").ok()?;
        let status = Command::new("tmux")
            .args(["-S"])
            .arg(&socket)
            .arg("-f")
            .arg(&config)
            .args(["new-session", "-d", "-s", "work"])
            .status()
            .ok()?;
        // A renaming window would change the structure between saves and mask
        // what these assertions are about.
        let _ = Command::new("tmux")
            .args(["-S"])
            .arg(&socket)
            .args([
                "set-window-option",
                "-t",
                "work:0",
                "automatic-rename",
                "off",
            ])
            .status();
        status.success().then_some(Self {
            _directory: directory,
            socket,
        })
    }
}

/// Saves until two consecutive captures agree, so the shell has finished
/// settling before a test asserts on structural dedup. Under load the first
/// save can land mid-startup, and the next one then legitimately reports a
/// change.
fn save_until_settled(data: &std::path::Path, socket: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        save(data, socket, &[]);
        if save(data, socket, &[]).starts_with("unchanged ") {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the tmux structure never settled"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S"])
            .arg(&self.socket)
            .arg("kill-server")
            .status();
    }
}

fn save(data: &std::path::Path, socket: &std::path::Path, extra: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tmux-recover"));
    command
        .arg("--data-dir")
        .arg(data)
        .args(["save", "--socket"])
        .arg(socket)
        .args(extra);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "save failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn snapshots(data: &std::path::Path) -> Vec<tmux_recover::model::Snapshot> {
    let sockets = data.join("sockets");
    let key = std::fs::read_dir(&sockets)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(sockets.join(key).join("snapshots"))
        .unwrap()
        .flatten()
    {
        let bytes = std::fs::read(entry.path()).unwrap();
        found.push(serde_json::from_slice(&bytes).unwrap());
    }
    found
}

fn pins(data: &std::path::Path) -> Vec<String> {
    let sockets = data.join("sockets");
    let key = std::fs::read_dir(&sockets)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let mut found: Vec<String> = std::fs::read_dir(sockets.join(key).join("pins"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

fn checkpoint(data: &std::path::Path) -> Option<tmux_recover::model::ProcessCheckpoint> {
    let sockets = data.join("sockets");
    let key = std::fs::read_dir(&sockets)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let path = sockets.join(key).join("process-current.json");
    let bytes = std::fs::read(path).ok()?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

fn current_id(data: &std::path::Path) -> String {
    let sockets = data.join("sockets");
    let key = std::fs::read_dir(&sockets)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let bytes = std::fs::read(sockets.join(key).join("current.json")).unwrap();
    let pointer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    pointer["snapshot_id"].as_str().unwrap().to_owned()
}

/// An explicit `save` must record the processes running at that moment. The
/// daemon's `process_checkpoint_interval` throttles background polling and must
/// not decide whether a user's own save is written.
#[test]
fn save_refreshes_the_process_checkpoint_even_when_unchanged() {
    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();

    save_until_settled(data.path(), &server.socket);
    let first = current_id(data.path());

    // Start a program, so the live process state now differs from the snapshot
    // without any structural change.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pane_command(&server) != "sleep" {
        assert!(
            std::time::Instant::now() < deadline,
            "shell never ran sleep"
        );
        let _ = Command::new("tmux")
            .args(["-S"])
            .arg(&server.socket)
            .args(["send-keys", "-t", "work:0.0", "sleep 300", "Enter"])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    let output = save(data.path(), &server.socket, &[]);
    assert!(output.starts_with("unchanged "), "{output}");
    let sidecar = checkpoint(data.path()).expect("an explicit save must write a sidecar");
    assert_eq!(
        sidecar.base_snapshot_id, first,
        "an unchanged save must anchor the sidecar to the existing current"
    );
    assert_eq!(current_id(data.path()), first);
    assert!(
        sidecar
            .panes
            .iter()
            .any(|pane| pane.current_command.as_deref() == Some("sleep")),
        "the running program is missing from the sidecar: {:?}",
        sidecar.panes
    );

    // A save that does write a snapshot must anchor the sidecar to the new id.
    let output = save(data.path(), &server.socket, &["--label", "next"]);
    assert!(output.starts_with("saved "), "{output}");
    let second = current_id(data.path());
    assert_ne!(second, first);
    assert_eq!(
        checkpoint(data.path()).unwrap().base_snapshot_id,
        second,
        "a written save must re-anchor the sidecar to the new snapshot"
    );
}

fn pane_command(server: &Server) -> String {
    let output = Command::new("tmux")
        .args(["-S"])
        .arg(&server.socket)
        .args([
            "display-message",
            "-p",
            "-t",
            "work:0.0",
            "#{pane_current_command}",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn save_honours_label_and_pin_when_the_structure_is_unchanged() {
    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();

    save_until_settled(data.path(), &server.socket);
    let baseline = snapshots(data.path()).len();

    // A plain repeat save still dedups.
    let repeat = save(data.path(), &server.socket, &[]);
    assert!(repeat.starts_with("unchanged "), "{repeat}");
    assert_eq!(snapshots(data.path()).len(), baseline);

    // A label is information the stored snapshot does not carry, so dedup must
    // not swallow it.
    let labelled = save(
        data.path(),
        &server.socket,
        &["--label", "before-upgrade", "--pin"],
    );
    assert!(labelled.starts_with("saved "), "{labelled}");
    let all = snapshots(data.path());
    assert_eq!(all.len(), baseline + 1);
    let stored = all
        .iter()
        .find(|snapshot| snapshot.label.as_deref() == Some("before-upgrade"))
        .expect("the labelled snapshot must be on disk");
    assert_eq!(
        pins(data.path()),
        vec![stored.id.clone()],
        "--pin must apply to the snapshot just written"
    );

    // `--pin` alone on an unchanged structure pins the current snapshot rather
    // than duplicating it: the pin is a property of a stored snapshot.
    let pin_only = save(data.path(), &server.socket, &["--pin"]);
    assert!(pin_only.starts_with("unchanged "), "{pin_only}");
    assert!(pin_only.contains("pinned "), "{pin_only}");
    assert_eq!(
        snapshots(data.path()).len(),
        baseline + 1,
        "--pin alone must not add history"
    );
    assert_eq!(pins(data.path()), vec![stored.id.clone()]);
}

#[cfg(unix)]
#[test]
fn socket_aliases_share_one_store_identity() {
    use std::os::unix::fs::symlink;

    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let alias = server._directory.path().join("tmux-alias.sock");
    symlink(&server.socket, &alias).unwrap();

    let output = save(data.path(), &alias, &[]);
    assert!(output.starts_with("saved "), "{output}");
    assert_eq!(
        std::fs::read_dir(data.path().join("sockets"))
            .unwrap()
            .count(),
        1
    );

    for socket in [&alias, &server.socket] {
        let output = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
            .arg("--data-dir")
            .arg(data.path())
            .args(["list", "--socket"])
            .arg(socket)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1s/1w/1p"),
            "list via {} missed the saved snapshot",
            socket.display()
        );
    }
}

#[test]
fn save_captures_after_waiting_for_the_mutation_lock() {
    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &StorageConfig::default());
    let lock = store.acquire_mutation_lock().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .arg("--data-dir")
        .arg(data.path())
        .args(["save", "--socket"])
        .arg(&server.socket)
        .spawn()
        .unwrap();

    // The child has to resolve its socket and reach the held lock before the
    // state changes. It cannot connect to tmux until the lock is released.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        Command::new("tmux")
            .args(["-S"])
            .arg(&server.socket)
            .args(["rename-window", "-t", "work:0", "after-lock"])
            .status()
            .unwrap()
            .success()
    );
    drop(lock);

    assert!(child.wait().unwrap().success());
    assert_eq!(
        store.load_current().unwrap().state.windows[0].name,
        "after-lock"
    );
}

//! CLI-level cover for `save`'s dedup semantics, which live in main.rs and so
//! are not reachable from a library test.

use std::process::Command;

use tempfile::TempDir;

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
        let status = Command::new("tmux")
            .args(["-S"])
            .arg(&socket)
            .args(["-f", "/dev/null", "new-session", "-d", "-s", "work"])
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

#[test]
fn save_honours_label_and_pin_when_the_structure_is_unchanged() {
    let Some(server) = Server::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();

    let first = save(data.path(), &server.socket, &[]);
    assert!(first.starts_with("saved "), "{first}");
    assert_eq!(snapshots(data.path()).len(), 1);

    // A plain repeat save still dedups.
    let repeat = save(data.path(), &server.socket, &[]);
    assert!(repeat.starts_with("unchanged "), "{repeat}");
    assert_eq!(snapshots(data.path()).len(), 1);

    // A label is information the stored snapshot does not carry, so dedup must
    // not swallow it.
    let labelled = save(
        data.path(),
        &server.socket,
        &["--label", "before-upgrade", "--pin"],
    );
    assert!(labelled.starts_with("saved "), "{labelled}");
    let all = snapshots(data.path());
    assert_eq!(all.len(), 2);
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
        2,
        "--pin alone must not add history"
    );
    assert_eq!(pins(data.path()), vec![stored.id.clone()]);
}

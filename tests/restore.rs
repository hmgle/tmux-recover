use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::{
    config::{RestoreConfig, StorageConfig},
    model::{RestoreStatus, Snapshot, SnapshotSource},
    restore::{apply, preflight, restore_config_options},
    storage::SnapshotStore,
    tmux::{capture::capture, control::ControlClient},
    util::socket_identity,
};

struct TestServer {
    directory: TempDir,
    socket: std::path::PathBuf,
}

impl TestServer {
    fn available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                tmux_recover::util::require_tmux_37(&String::from_utf8_lossy(&output.stdout))
                    .is_ok()
            })
    }

    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("tmux.sock");
        Self { directory, socket }
    }

    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(["-S"])
            .arg(&self.socket)
            .args(["-f", "/dev/null"]);
        command
    }

    fn stop(&self) {
        let _ = self.tmux().arg("kill-server").status();
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn real_restore_with_an_empty_allowlist_removes_the_process_checkpoint() {
    if !TestServer::available() {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    }
    let server = TestServer::new();
    success(
        server
            .tmux()
            .args(["new-session", "-d", "-s", "current", "sleep 60"]),
    );
    let data = tempfile::tempdir().unwrap();
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &StorageConfig::default());

    let save = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .arg("--data-dir")
        .arg(data.path())
        .args(["save", "--socket"])
        .arg(&server.socket)
        .output()
        .unwrap();
    assert!(
        save.status.success(),
        "{}",
        String::from_utf8_lossy(&save.stderr)
    );
    assert!(store.read_process_checkpoint().unwrap().is_some());

    let config = data.path().join("disabled.toml");
    std::fs::write(&config, "[restore]\nprocess_allowlist = []\n").unwrap();
    let dry_run = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .arg("--data-dir")
        .arg(data.path())
        .arg("--config")
        .arg(&config)
        .args(["restore", "current", "--socket"])
        .arg(&server.socket)
        .args(["--replace", "--yes", "--dry-run", "--restore-processes"])
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&dry_run.stdout)
            .contains("process restore is disabled because restore.process_allowlist is empty")
    );
    assert!(
        store.read_process_checkpoint().unwrap().is_some(),
        "a dry-run must not mutate the snapshot store"
    );

    let restore = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .arg("--data-dir")
        .arg(data.path())
        .arg("--config")
        .arg(&config)
        .args(["restore", "current", "--socket"])
        .arg(&server.socket)
        .args(["--replace", "--yes"])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&restore.stdout),
        String::from_utf8_lossy(&restore.stderr)
    );
    assert!(
        store.read_process_checkpoint().unwrap().is_none(),
        "a real restore must remove stale process metadata while capture is disabled"
    );
}

#[tokio::test]
async fn restores_special_fields_active_pane_and_zoom() {
    if !TestServer::available() {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    }
    let server = TestServer::new();
    let cwd_one = server.directory.path().join("cwd one");
    let cwd_two = server.directory.path().join("cwd\ttab\nline:雪");
    std::fs::create_dir(&cwd_one).unwrap();
    std::fs::create_dir(&cwd_two).unwrap();
    // macOS tempfile paths use /var while tmux reports the canonical
    // /private/var spelling. Exercise the same directories through their
    // canonical identities so restore assertions compare like with like.
    let cwd_one = cwd_one.canonicalize().unwrap();
    let cwd_two = cwd_two.canonicalize().unwrap();

    let first = output(
        server
            .tmux()
            .args([
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_id}|#{window_id}|#{pane_id}",
                "-s",
                "work:雪",
                "-n",
                "main window",
                "-c",
            ])
            .arg(&cwd_one)
            .arg("sleep 60"),
    );
    let mut first_fields = first.trim().split('|');
    let session_id = first_fields.next().unwrap();
    let window_id = first_fields.next().unwrap();
    let first_pane = first_fields.next().unwrap();
    assert_eq!(first_fields.next(), None);
    let second_pane = output(
        server
            .tmux()
            .args(["split-window", "-d", "-P", "-F", "#{pane_id}", "-t"])
            .arg(first_pane)
            .arg("-c")
            .arg(&cwd_two)
            .arg("sleep 60"),
    );
    let second_pane = second_pane.trim();
    let third_pane = output(
        server
            .tmux()
            .args(["split-window", "-d", "-P", "-F", "#{pane_id}", "-t"])
            .arg(first_pane)
            .arg("-c")
            .arg(&cwd_one)
            .arg("sleep 60"),
    );
    let third_pane = third_pane.trim();
    let fourth_pane = output(
        server
            .tmux()
            .args(["split-window", "-d", "-P", "-F", "#{pane_id}", "-t"])
            .arg(first_pane)
            .arg("-c")
            .arg(&cwd_two)
            .arg("sleep 60"),
    );
    let fourth_pane = fourth_pane.trim();
    let auxiliary = output(
        server
            .tmux()
            .args([
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{window_id}|#{pane_id}",
                "-t",
            ])
            .arg(format!("{session_id}:1"))
            .args(["-n", "aux", "-c"])
            .arg(&cwd_one)
            .arg("sleep 60"),
    );
    let (auxiliary_window, auxiliary_pane) = auxiliary.trim().split_once('|').unwrap();
    std::thread::sleep(Duration::from_millis(100));
    success(server.tmux().args([
        "set-window-option",
        "-t",
        window_id,
        "automatic-rename",
        "off",
    ]));
    success(server.tmux().args([
        "set-window-option",
        "-t",
        auxiliary_window,
        "automatic-rename",
        "off",
    ]));
    success(server.tmux().args(["select-pane", "-t", second_pane]));
    success(server.tmux().args(["resize-pane", "-Z", "-t", second_pane]));

    let source = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        success(
            server
                .tmux()
                .args(["select-pane", "-t", first_pane, "-T", ""]),
        );
        success(
            server
                .tmux()
                .args(["select-pane", "-t", second_pane, "-T", "title:雪"]),
        );
        success(
            server
                .tmux()
                .args(["select-pane", "-t", third_pane, "-T", "third"]),
        );
        success(
            server
                .tmux()
                .args(["select-pane", "-t", fourth_pane, "-T", "fourth"]),
        );
        success(
            server
                .tmux()
                .args(["select-pane", "-t", auxiliary_pane, "-T", "auxiliary"]),
        );
        capture(&mut client, &server.socket).await.unwrap()
    };
    let snapshot = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        source.origin,
        source.state,
        source.diagnostics,
    )
    .unwrap();
    let main = snapshot
        .state
        .windows
        .iter()
        .find(|window| window.name == "main window")
        .unwrap();
    assert_pane_properties(main, &cwd_one, &cwd_two);

    server.stop();
    success(server.tmux().args(["new-session", "-d", "-s", "bootstrap"]));
    let mut client = ControlClient::connect(&server.socket).await.unwrap();
    let target = capture(&mut client, &server.socket).await.unwrap();
    let config = RestoreConfig {
        process_allowlist: vec!["sleep".to_owned()],
        ..RestoreConfig::default()
    };
    let options = restore_config_options(&config, false, false, None, true, None);
    let plan = preflight(&snapshot, &target, &options).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(plan.process_restarts, 5);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        plan.process_restarts, 0,
        "process restart metadata is collected from Linux procfs"
    );
    let report = apply(&mut client, &snapshot, &target, &plan).await;
    assert_eq!(report.status, RestoreStatus::Succeeded, "{report:#?}");
    drop(client);

    let restored = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    assert_eq!(restored.state.sessions.len(), 1);
    assert_eq!(restored.state.sessions[0].name, "work:雪");
    assert_eq!(restored.state.windows.len(), 2);
    let global_window_size = output(server.tmux().args(["show-options", "-gv", "window-size"]));
    for restored_window in &restored.state.windows {
        let local_window_size = output(server.tmux().args([
            "show-options",
            "-wqv",
            "-t",
            &restored_window.id,
            "window-size",
        ]));
        assert!(
            local_window_size.trim().is_empty(),
            "restore left a window-size override on {}: {local_window_size:?}",
            restored_window.id
        );
        let effective_window_size = output(server.tmux().args([
            "display-message",
            "-p",
            "-t",
            &restored_window.id,
            "#{window-size}",
        ]));
        assert_eq!(
            effective_window_size.trim(),
            global_window_size.trim(),
            "restored window {} did not inherit the global size policy",
            restored_window.id
        );
    }
    let window = restored
        .state
        .windows
        .iter()
        .find(|window| window.name == "main window")
        .unwrap();
    assert_eq!(window.name, "main window");
    assert!(window.zoomed);
    assert_eq!(window.panes.len(), 4);
    assert_pane_properties(window, &cwd_one, &cwd_two);
    let auxiliary = restored
        .state
        .windows
        .iter()
        .find(|window| window.name == "aux")
        .unwrap();
    assert_eq!(auxiliary.panes.len(), 1);
    assert_eq!(auxiliary.panes[0].title.as_deref(), Some("auxiliary"));
    assert_eq!(cwd(&auxiliary.panes[0]), cwd_one);
}

#[tokio::test]
async fn restores_default_shell_panes_without_a_hold_command() {
    if !TestServer::available() {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    }
    let server = TestServer::new();
    let cwd = server.directory.path();

    let first_pane = output(
        server
            .tmux()
            .args([
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-s",
                "shells",
                "-n",
                "main",
                "-c",
            ])
            .arg(cwd),
    );
    for _ in 0..2 {
        output(
            server
                .tmux()
                .args(["split-window", "-d", "-P", "-F", "#{pane_id}", "-t"])
                .arg(first_pane.trim())
                .arg("-c")
                .arg(cwd),
        );
    }
    success(server.tmux().args([
        "set-window-option",
        "-t",
        first_pane.trim(),
        "automatic-rename",
        "off",
    ]));

    let source = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    let snapshot = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        source.origin,
        source.state,
        source.diagnostics,
    )
    .unwrap();

    server.stop();
    success(server.tmux().args(["new-session", "-d", "-s", "bootstrap"]));
    let mut client = ControlClient::connect(&server.socket).await.unwrap();
    let target = capture(&mut client, &server.socket).await.unwrap();
    for hook in [
        "after-new-session[901]",
        "session-created[901]",
        "after-new-window[901]",
    ] {
        success(server.tmux().args([
            "set-option",
            "-g",
            hook,
            "wait-for -S tmux-recover:state-changed",
        ]));
    }
    let default_shell = target.default_shell.as_deref().unwrap();
    let expected_command = std::path::Path::new(default_shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap()
        .to_owned();
    let config = RestoreConfig::default();
    let options = restore_config_options(&config, false, false, None, false, None);
    let plan = preflight(&snapshot, &target, &options).unwrap();
    assert_eq!(plan.process_restarts, 0);
    let report = apply(&mut client, &snapshot, &target, &plan).await;
    assert_eq!(report.status, RestoreStatus::Succeeded, "{report:#?}");
    drop(client);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let restored = loop {
        let restored = {
            let mut client = ControlClient::connect(&server.socket).await.unwrap();
            capture(&mut client, &server.socket).await.unwrap()
        };
        let panes: Vec<_> = restored
            .state
            .windows
            .iter()
            .flat_map(|window| &window.panes)
            .collect();
        if panes.len() == 3
            && panes
                .iter()
                .all(|pane| pane.current_command.as_deref() == Some(&expected_command))
        {
            break restored;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "restored panes did not enter {expected_command}: {:#?}",
            panes
                .iter()
                .map(|pane| (&pane.current_command, &pane.start_command))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let panes: Vec<_> = restored
        .state
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .collect();
    assert_eq!(panes.len(), 3);
    assert!(
        panes.iter().all(|pane| pane.start_command.is_none()),
        "restore polluted pane_start_command: {:#?}",
        panes
            .iter()
            .map(|pane| &pane.start_command)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn backup_cleanup_failure_keeps_the_committed_restore_live() {
    if !TestServer::available() {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    }
    let server = TestServer::new();
    let cwd = server.directory.path();
    success(
        server
            .tmux()
            .args(["new-session", "-d", "-s", "restored", "-c"])
            .arg(cwd),
    );
    let active_pane = output(server.tmux().args([
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "restored",
    ]));
    success(
        server
            .tmux()
            .args(["select-pane", "-t", active_pane.trim()]),
    );
    for index in 1..=8 {
        success(
            server
                .tmux()
                .args(["new-window", "-d", "-t"])
                .arg(format!("restored:{index}"))
                .arg("-c")
                .arg(cwd),
        );
    }
    let source = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    let snapshot = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        source.origin,
        source.state,
        source.diagnostics,
    )
    .unwrap();

    server.stop();
    success(server.tmux().args(["new-session", "-d", "-s", "old-one"]));
    success(server.tmux().args(["new-session", "-d", "-s", "old-two"]));
    let backup_name = format!("__tmux_recover_backup_{}_1", &snapshot.semantic_hash[..8]);
    let killer_socket = server.socket.clone();
    let killer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let output = Command::new("tmux")
                .args(["-S"])
                .arg(&killer_socket)
                .args(["list-sessions", "-F", "#{session_name}"])
                .output()
                .unwrap();
            let sessions = String::from_utf8_lossy(&output.stdout);
            if sessions.lines().any(|name| name == "restored")
                && sessions.lines().any(|name| name == backup_name)
            {
                let status = Command::new("tmux")
                    .args(["-S"])
                    .arg(&killer_socket)
                    .args(["kill-session", "-t"])
                    .arg(&backup_name)
                    .status()
                    .unwrap();
                assert!(status.success());
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "restore never exposed the new session and second backup together"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    let mut client = ControlClient::connect_to(&server.socket, Some("old-one"))
        .await
        .unwrap();
    let target = capture(&mut client, &server.socket).await.unwrap();
    let config = RestoreConfig::default();
    let options = restore_config_options(&config, true, false, None, false, None);
    let plan = preflight(&snapshot, &target, &options).unwrap();
    let report = apply(&mut client, &snapshot, &target, &plan).await;
    killer.join().unwrap();
    assert_eq!(report.status, RestoreStatus::Succeeded, "{report:#?}");
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    assert!(report.warnings[0].contains("old-two"));
    drop(client);

    let restored = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    assert_eq!(restored.state.sessions.len(), 1);
    assert_eq!(restored.state.sessions[0].name, "restored");
}

fn output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn success(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cwd(pane: &tmux_recover::model::Pane) -> std::path::PathBuf {
    pane.cwd.path.as_ref().unwrap().to_path_buf().unwrap()
}

fn assert_pane_properties(
    window: &tmux_recover::model::Window,
    cwd_one: &std::path::Path,
    cwd_two: &std::path::Path,
) {
    let mut titles: Vec<&str> = window
        .panes
        .iter()
        .filter_map(|pane| pane.title.as_deref())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, ["", "fourth", "third", "title:雪"]);
    assert_eq!(
        window
            .panes
            .iter()
            .filter(|pane| cwd(pane) == cwd_one)
            .count(),
        2
    );
    assert_eq!(
        window
            .panes
            .iter()
            .filter(|pane| cwd(pane) == cwd_two)
            .count(),
        2
    );
    let active = window
        .panes
        .iter()
        .find(|pane| Some(&pane.id) == window.active_pane_id.as_ref())
        .unwrap();
    assert_eq!(active.title.as_deref(), Some("title:雪"));
}

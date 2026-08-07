use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::{
    config::RestoreConfig,
    model::{RestoreStatus, Snapshot, SnapshotSource},
    restore::{apply, preflight, restore_config_options},
    tmux::{capture::capture, control::ControlClient},
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
    let options = restore_config_options(&config, false, false, None, true);
    let plan = preflight(&snapshot, &target, &options).unwrap();
    assert_eq!(plan.process_restarts, 5);
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

use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::tmux::{
    capture::{capture, capture_structure},
    control::ControlClient,
};

struct TestServer {
    directory: TempDir,
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
        let status = Command::new("tmux")
            .args(["-S"])
            .arg(&socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "capture:雪",
                "sleep 60",
            ])
            .status()
            .ok()?;
        status.success().then_some(Self { directory, socket })
    }

    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-S"]).arg(&self.socket);
        command
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").status();
    }
}

#[tokio::test]
async fn captures_empty_title_and_line_unsafe_cwd() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let cwd = server.directory.path().join("space\ttab\nline:雪");
    std::fs::create_dir(&cwd).unwrap();

    let window_id =
        command_output(
            server
                .tmux()
                .args(["list-windows", "-a", "-F", "#{window_id}"]),
        );
    let pane_id = command_output(server.tmux().args(["list-panes", "-a", "-F", "#{pane_id}"]));
    assert!(
        server
            .tmux()
            .args(["respawn-pane", "-k", "-c"])
            .arg(&cwd)
            .arg("-t")
            .arg(pane_id.trim())
            .arg("sleep 60")
            .status()
            .unwrap()
            .success()
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        server
            .tmux()
            .args(["set-window-option", "-t"])
            .arg(window_id.trim())
            .args(["automatic-rename", "off"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        server
            .tmux()
            .args(["select-pane", "-t"])
            .arg(pane_id.trim())
            .args(["-T", ""])
            .status()
            .unwrap()
            .success()
    );

    let mut client = ControlClient::connect(&server.socket).await.unwrap();
    let structural = capture_structure(&mut client, &server.socket)
        .await
        .unwrap();
    assert!(
        structural.state.windows[0].panes[0].restart.is_none(),
        "structural capture must not collect process restart metadata"
    );
    let captured = capture(&mut client, &server.socket).await.unwrap();
    // macOS reports a cwd below /private/var even when tempfile returned its
    // /var alias. Compare filesystem identities while still exercising the
    // tabs, newlines, colons, and Unicode this test exists to preserve.
    let expected_cwd = cwd.canonicalize().unwrap();
    assert_eq!(captured.state.sessions[0].name, "capture:雪");
    let window = &captured.state.windows[0];
    assert_eq!(window.automatic_rename, Some(false));
    assert_eq!(window.panes[0].title.as_deref(), Some(""));
    #[cfg(target_os = "linux")]
    assert!(
        window.panes[0].restart.is_some(),
        "full capture must retain Linux process metadata"
    );
    assert_eq!(
        window.panes[0]
            .cwd
            .path
            .as_ref()
            .unwrap()
            .to_path_buf()
            .unwrap(),
        expected_cwd
    );
}

fn command_output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

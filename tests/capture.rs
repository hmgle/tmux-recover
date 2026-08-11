use std::{
    fs::File,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::Duration,
};

use nix::pty::{Winsize, openpty};
use tempfile::TempDir;
use tmux_recover::tmux::{
    capture::{capture, capture_structure},
    control::ControlClient,
};

mod support;

use support::{PtyDrain, ioctl_request};

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

struct AttachedClient {
    child: Child,
    drain: PtyDrain,
    name: String,
}

impl AttachedClient {
    fn start(server: &TestServer, target: &str) -> Self {
        let pty = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .unwrap();
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        let mut command = server.tmux();
        command
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TERM", "xterm-256color")
            .args(["attach-session", "-t", target])
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // tmux requires a controlling terminal, not merely a PTY-backed file
        // descriptor. The child becomes a session leader and claims its stdin
        // slave before exec; both operations are async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if nix::libc::ioctl(0, ioctl_request(nix::libc::TIOCSCTTY), 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let mut drain = PtyDrain::start(master);
        let mut name = None;
        for _ in 0..100 {
            if let Some(status) = child.try_wait().unwrap() {
                drain.join();
                let output = drain.output();
                panic!("ordinary tmux client exited before attaching: {status}: {output:?}");
            }
            let output = command_output(server.tmux().args([
                "list-clients",
                "-F",
                "#{client_control_mode}|#{client_name}",
            ]));
            name = output.lines().find_map(|line| {
                line.strip_prefix("0|")
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            });
            if name.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let name = name.unwrap_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            drain.join();
            panic!(
                "ordinary tmux client did not attach to the isolated server: {:?}",
                drain.output()
            );
        });
        Self { child, drain, name }
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.drain.join();
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

#[tokio::test]
async fn captures_only_ordinary_client_session_selection() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    assert!(
        server
            .tmux()
            .args(["new-session", "-d", "-s", "other", "sleep 60"])
            .status()
            .unwrap()
            .success()
    );
    let original_session_id = command_output(server.tmux().args([
        "list-sessions",
        "-F",
        "#{session_id}|#{session_name}",
    ]))
    .lines()
    .find_map(|line| line.strip_suffix("|capture:雪"))
    .unwrap()
    .to_owned();
    let ordinary = AttachedClient::start(&server, &original_session_id);
    assert!(
        server
            .tmux()
            .args(["switch-client", "-c", &ordinary.name, "-t", "other"])
            .status()
            .unwrap()
            .success()
    );

    let mut control = ControlClient::connect(&server.socket).await.unwrap();
    let captured = capture_structure(&mut control, &server.socket)
        .await
        .unwrap();
    let current = captured
        .state
        .sessions
        .iter()
        .find(|session| session.name == "other")
        .unwrap();
    let last = captured
        .state
        .sessions
        .iter()
        .find(|session| session.name == "capture:雪")
        .unwrap();
    let client_state = captured.state.client_state.unwrap();
    assert_eq!(client_state.attachments.len(), 1);
    assert_eq!(client_state.attachments[0].session_id, current.id);
    assert_eq!(
        client_state.attachments[0].last_session_id.as_deref(),
        Some(last.id.as_str())
    );
}

fn command_output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

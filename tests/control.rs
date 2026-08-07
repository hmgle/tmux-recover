//! Protocol-level tests for control-mode command sequencing.
//!
//! These pin down what tmux actually does when one command in a
//! semicolon-separated sequence fails, because the answer decides whether
//! `execute_blocks` may keep reading after an error.

use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::tmux::control::ControlClient;

struct TestServer {
    #[allow(dead_code)]
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
                "control",
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

/// tmux abandons the remainder of a sequence at the first failure, so a
/// mid-sequence `%error` means the later blocks never arrive. `execute_blocks`
/// must return that error instead of waiting for them; if it ever goes back to
/// draining the missing blocks this test hangs and trips the timeout.
#[tokio::test]
async fn mid_sequence_error_returns_instead_of_waiting_for_missing_blocks() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let mut client = ControlClient::connect(&server.socket).await.unwrap();

    let sequence = "display-message -p one ; list-panes -t no-such-window -F two ; \
                    display-message -p three ; display-message -p four";
    let result = tokio::time::timeout(Duration::from_secs(10), client.execute_blocks(sequence, 4))
        .await
        .expect("execute_blocks hung waiting for blocks tmux never emits");

    let error = format!("{:#}", result.expect_err("the sequence must fail"));
    assert!(
        error.contains("can't find window"),
        "error should carry tmux's own text, got: {error}"
    );
    assert!(
        error.contains("step 2 of 4"),
        "error should say which command failed, got: {error}"
    );
    assert!(
        client.is_poisoned(),
        "a mid-sequence failure must mark the client for reconnection"
    );
}

/// A failure in the last requested block leaves nothing unread, so the client
/// stays usable and must not be marked poisoned.
#[tokio::test]
async fn trailing_error_leaves_the_client_usable() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let mut client = ControlClient::connect(&server.socket).await.unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.execute_blocks("list-panes -t no-such-window", 1),
    )
    .await
    .expect("execute_blocks hung on a single failing command");
    assert!(result.is_err());
    assert!(
        !client.is_poisoned(),
        "an error in the final block leaves the stream clean"
    );

    // Proof the stream really is still aligned: the next command's output must
    // come back intact rather than picking up a stale block.
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        client.execute("display-message -p after-error"),
    )
    .await
    .expect("the connection desynced after a recoverable error")
    .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(String::from_utf8_lossy(&output[0]), "after-error");
}

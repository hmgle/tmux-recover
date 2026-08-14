//! Protocol-level tests for control-mode command sequencing.
//!
//! These pin down what tmux actually does when one command in a
//! semicolon-separated sequence fails, because the answer decides whether
//! `execute_blocks` may keep reading after an error.

use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::tmux::control::{CommandRunner, ControlClient, command_runner_is_unavailable};

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

#[tokio::test]
async fn one_shot_runner_preserves_command_error_blocks() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let mut runner = CommandRunner::new(&server.socket);
    let result = runner
        .execute_blocks(
            [
                "display-message",
                "-p",
                "one",
                ";",
                "list-panes",
                "-t",
                "no-such-window",
                ";",
                "display-message",
                "-p",
                "three",
            ],
            3,
        )
        .await;

    let error = result.expect_err("the one-shot sequence must fail");
    assert!(!command_runner_is_unavailable(&error));
    assert_eq!(
        error.to_string(),
        "tmux command sequence failed after 1 complete control blocks (expected 3 commands): \
         can't find window: no-such-window"
    );
}

#[tokio::test]
async fn one_shot_runner_tolerates_blocks_from_command_hooks() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    assert!(
        server
            .tmux()
            .args([
                "set-option",
                "-go",
                "after-list-panes[900]",
                "wait-for -S one-shot-hook-probe",
            ])
            .status()
            .unwrap()
            .success()
    );
    let mut runner = CommandRunner::new(&server.socket);
    let blocks = runner
        .execute_blocks(
            [
                "list-panes",
                "-a",
                "-F",
                "#{pane_id}",
                ";",
                "display-message",
                "-p",
                "after-hook",
            ],
            2,
        )
        .await
        .unwrap();

    assert_eq!(blocks.len(), 3);
    assert!(String::from_utf8_lossy(&blocks[0][0]).starts_with('%'));
    assert!(blocks[1].is_empty());
    assert_eq!(blocks[2], vec![b"after-hook".to_vec()]);
}

#[tokio::test]
async fn complete_one_shot_response_with_too_few_blocks_is_not_unavailable() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let mut runner = CommandRunner::new(&server.socket);
    let error = runner
        .execute_blocks(["display-message", "-p", "one"], 2)
        .await
        .expect_err("an incomplete command response was accepted");

    assert!(!command_runner_is_unavailable(&error));
    assert!(error.to_string().contains("expected at least 2"));
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

/// A persistent hook runs as an unflagged control-mode command and emits its
/// own empty output block after the command that triggered it. That block must
/// not become the result of the next command sent by the client.
#[tokio::test]
async fn hook_output_blocks_do_not_desynchronize_command_results() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let status = server
        .tmux()
        .args([
            "set-option",
            "-g",
            "after-new-window[901]",
            "wait-for -S tmux-recover:state-changed",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let mut client = ControlClient::connect(&server.socket).await.unwrap();
    let created = tokio::time::timeout(
        Duration::from_secs(10),
        client.execute(r##"new-window -d -P -F "#{window_id}|#{pane_id}" -t control -n created"##),
    )
    .await
    .expect("new-window hung")
    .unwrap();
    assert_eq!(created.len(), 1, "new-window output was misclassified");
    assert!(String::from_utf8_lossy(&created[0]).starts_with('@'));

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        client.execute("display-message -p after-hook"),
    )
    .await
    .expect("the command after the hook hung")
    .unwrap();
    assert_eq!(output, vec![b"after-hook".to_vec()]);
}

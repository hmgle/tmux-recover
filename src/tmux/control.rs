use std::{collections::VecDeque, path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    Message(String),
    Event(String),
    Exit(String),
}

pub struct ControlClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    notifications: VecDeque<Notification>,
    poisoned: bool,
}

impl ControlClient {
    pub async fn connect(socket: &Path) -> Result<Self> {
        Self::connect_to(socket, None).await
    }

    pub async fn connect_to(socket: &Path, target_session: Option<&str>) -> Result<Self> {
        tracing::debug!(
            target: "tmux_recover::control",
            socket = %socket.display(),
            target_session,
            "connect tmux control client"
        );
        let mut command = Command::new("tmux");
        command.env_remove("TMUX").arg("-S").arg(socket).args([
            "-u",
            "-C",
            "attach-session",
            "-f",
            "ignore-size,no-output,no-detach-on-destroy",
        ]);
        if let Some(target_session) = target_session {
            command.arg("-t").arg(target_session);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to connect to tmux socket {}", socket.display()))?;
        let stdin = child
            .stdin
            .take()
            .context("tmux control stdin is unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("tmux control stdout is unavailable")?;
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            notifications: VecDeque::new(),
            poisoned: false,
        };
        client.drain_startup().await?;
        Ok(client)
    }

    async fn drain_startup(&mut self) -> Result<()> {
        loop {
            let line = self.read_line().await?;
            if line.starts_with(b"%session-changed ") {
                self.notifications
                    .push_back(Notification::Event(text_lossy(&line)));
                return Ok(());
            }
            if line.starts_with(b"%exit") {
                bail!(
                    "tmux control client exited during attach: {}",
                    text_lossy(&line)
                );
            }
            if line.starts_with(b"%error ") {
                bail!("tmux rejected control-mode attach: {}", text_lossy(&line));
            }
        }
    }

    pub async fn execute(&mut self, command: &str) -> Result<Vec<Vec<u8>>> {
        let mut blocks = self.execute_blocks(command, 1).await?;
        Ok(blocks.pop().unwrap_or_default())
    }

    pub async fn execute_blocks(
        &mut self,
        command: &str,
        block_count: usize,
    ) -> Result<Vec<Vec<Vec<u8>>>> {
        tracing::debug!(target: "tmux_recover::control", blocks = block_count, command, "execute tmux command");
        self.stdin.write_all(command.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;

        let mut blocks: Vec<Vec<Vec<u8>>> = Vec::with_capacity(block_count);
        for index in 0..block_count {
            loop {
                let line = self.read_line().await?;
                if line.starts_with(b"%begin ") {
                    break;
                }
                self.record_notification(line)?;
            }

            let mut output: Vec<Vec<u8>> = Vec::new();
            loop {
                let line = self.read_line().await?;
                if line.starts_with(b"%end ") {
                    tracing::debug!(
                        target: "tmux_recover::control",
                        lines = output.len(),
                        output = ?output
                            .iter()
                            .map(|line| String::from_utf8_lossy(line).into_owned())
                            .collect::<Vec<_>>(),
                        "tmux command completed"
                    );
                    blocks.push(output);
                    break;
                }
                if line.starts_with(b"%error ") {
                    // tmux abandons the rest of a semicolon-separated sequence
                    // at the first failure, so the blocks we have not read yet
                    // will never arrive: waiting for them would hang forever.
                    // Return now, but mark the client unusable when the error
                    // landed mid-sequence, because we cannot prove where the
                    // stream stands. Callers must reconnect rather than reuse it.
                    if index + 1 < block_count {
                        self.poisoned = true;
                    }
                    let detail = output
                        .iter()
                        .map(|line| String::from_utf8_lossy(line))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let detail = if detail.is_empty() {
                        text_lossy(&line)
                    } else {
                        detail
                    };
                    bail!(
                        "tmux command failed at step {} of {block_count}: {detail}",
                        index + 1
                    );
                }
                if line.starts_with(b"%exit") {
                    bail!(
                        "tmux control client exited while a command was pending: {}",
                        text_lossy(&line)
                    );
                }
                output.push(line);
            }
        }
        Ok(blocks)
    }

    /// True once a command failed partway through a multi-command sequence.
    /// The remaining blocks were never emitted, so the connection cannot be
    /// trusted for further commands and the caller should reconnect.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub async fn next_notification(&mut self) -> Result<Notification> {
        if let Some(notification) = self.notifications.pop_front() {
            return Ok(notification);
        }
        loop {
            let line = self.read_line().await?;
            if line.starts_with(b"%begin ") {
                // No command is outstanding, so consume unsolicited command output safely.
                loop {
                    let nested = self.read_line().await?;
                    if nested.starts_with(b"%end ") || nested.starts_with(b"%error ") {
                        break;
                    }
                }
                continue;
            }
            if let Some(notification) = parse_notification(&line) {
                return Ok(notification);
            }
        }
    }

    pub async fn client_name(&mut self) -> Result<String> {
        let output = self
            .execute("display-message -p -F \"#{client_name}\"")
            .await?;
        if output.len() != 1 {
            bail!(
                "tmux returned {} client-name lines, expected one",
                output.len()
            );
        }
        String::from_utf8(output[0].clone()).context("tmux returned an invalid client name")
    }

    pub fn take_notifications(&mut self) -> Vec<Notification> {
        self.notifications.drain(..).collect()
    }

    fn record_notification(&mut self, line: Vec<u8>) -> Result<()> {
        if let Some(notification) = parse_notification(&line) {
            self.notifications.push_back(notification);
            Ok(())
        } else {
            bail!("unexpected control-mode line: {}", text_lossy(&line))
        }
    }

    async fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        let count = self.stdout.read_until(b'\n', &mut line).await?;
        if count == 0 {
            let status = self.child.wait().await?;
            let stderr = match self.child.stderr.take() {
                Some(stderr) => {
                    let mut reader = BufReader::new(stderr);
                    let mut bytes = Vec::new();
                    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes).await?;
                    String::from_utf8_lossy(&bytes).trim().to_owned()
                }
                None => String::new(),
            };
            bail!("tmux control connection closed ({status}): {stderr}");
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        Ok(line)
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn parse_notification(line: &[u8]) -> Option<Notification> {
    if let Some(message) = line.strip_prefix(b"%message ") {
        Some(Notification::Message(text_lossy(message)))
    } else if line.starts_with(b"%exit") {
        Some(Notification::Exit(text_lossy(line)))
    } else if line.starts_with(b"%") {
        Some(Notification::Event(text_lossy(line)))
    } else {
        None
    }
}

fn text_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

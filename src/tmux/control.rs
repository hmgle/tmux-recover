use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use thiserror::Error;
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

/// Executes tmux commands without attaching a client to any session.
///
/// A long-lived control-mode client participates in session terminal state even
/// with `ignore-size` and `no-output`. In particular, it can interfere with
/// terminal capability and colour queries from programs running in that
/// session. The daemon uses this runner for capture and hook management so it
/// remains invisible to user sessions between commands.
pub struct CommandRunner {
    socket: PathBuf,
}

#[derive(Debug, Error)]
#[error("tmux one-shot command client became unavailable: {detail}")]
struct CommandRunnerUnavailable {
    detail: String,
}

/// True when a one-shot client stopped before it completed every requested
/// command. This normally means the target tmux server has exited or its socket
/// can no longer be reached, so a daemon should release its lock and stop.
pub fn command_runner_is_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<CommandRunnerUnavailable>())
}

impl CommandRunner {
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }

    pub async fn execute<I, S>(&mut self, arguments: I) -> Result<Vec<Vec<u8>>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let blocks = self.execute_blocks(arguments, 1).await?;
        Ok(blocks.into_iter().next().unwrap_or_default())
    }

    pub async fn execute_blocks<I, S>(
        &mut self,
        arguments: I,
        block_count: usize,
    ) -> Result<Vec<Vec<Vec<u8>>>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arguments: Vec<OsString> = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect();
        tracing::debug!(
            target: "tmux_recover::control",
            socket = %self.socket.display(),
            arguments = ?arguments,
            blocks = block_count,
            "execute unattached tmux command"
        );
        let output = Command::new("tmux")
            .env_remove("TMUX")
            .arg("-S")
            .arg(&self.socket)
            .args(["-u", "-C"])
            .args(&arguments)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| CommandRunnerUnavailable {
                detail: format!(
                    "failed to execute tmux command on socket {}: {error}",
                    self.socket.display(),
                ),
            })?;

        let response = match parse_one_shot_output(&output.stdout, block_count) {
            Ok(response) => response,
            Err(OneShotParseError::Command(error)) => return Err(error),
            Err(OneShotParseError::Protocol(error)) => return Err(error),
            Err(OneShotParseError::Incomplete(detail)) => {
                return Err(CommandRunnerUnavailable { detail }.into());
            }
        };
        if !response.saw_exit {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if stderr.is_empty() {
                format!(
                    "{}; received {} of at least {block_count} command blocks before the control stream closed",
                    output.status,
                    response.blocks.len(),
                )
            } else {
                stderr
            };
            return Err(CommandRunnerUnavailable { detail }.into());
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if stderr.is_empty() {
                bail!("tmux one-shot command client exited with {}", output.status);
            }
            bail!(
                "tmux one-shot command client exited with {}: {stderr}",
                output.status
            );
        }
        if response.blocks.len() < block_count {
            bail!(
                "tmux one-shot command returned {} command blocks, expected at least {block_count}",
                response.blocks.len()
            );
        }
        Ok(response.blocks)
    }
}

struct OneShotResponse {
    blocks: Vec<Vec<Vec<u8>>>,
    saw_exit: bool,
}

enum OneShotParseError {
    Command(anyhow::Error),
    Protocol(anyhow::Error),
    Incomplete(String),
}

fn parse_one_shot_output(
    stdout: &[u8],
    block_count: usize,
) -> std::result::Result<OneShotResponse, OneShotParseError> {
    let lines = split_lines(stdout);
    let mut lines = lines.into_iter();
    let mut blocks: Vec<Vec<Vec<u8>>> = Vec::with_capacity(block_count);
    let mut saw_exit = false;

    while let Some(line) = lines.next() {
        let header = parse_block_header(&line).map_err(OneShotParseError::Protocol)?;
        if let Some(_header) = header {
            // Initial argv commands and any hooks they trigger are both usually
            // unflagged. Keep every complete block; callers can require a
            // minimum count, but cannot infer ownership from the flags.
            let mut output: Vec<Vec<u8>> = Vec::new();
            loop {
                let Some(line) = lines.next() else {
                    return Err(OneShotParseError::Incomplete(
                        "control output ended while a command block was pending".to_owned(),
                    ));
                };
                if line.starts_with(b"%end ") {
                    blocks.push(output);
                    break;
                }
                if line.starts_with(b"%error ") {
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
                    return Err(OneShotParseError::Command(anyhow::anyhow!(
                        "tmux command sequence failed after {} complete control blocks \
                         (expected {block_count} commands): {detail}",
                        blocks.len()
                    )));
                }
                if line.starts_with(b"%exit") {
                    return Err(OneShotParseError::Incomplete(
                        "one-shot client exited while a command block was pending".to_owned(),
                    ));
                }
                output.push(line);
            }
            continue;
        }
        if line.starts_with(b"%exit") {
            saw_exit = true;
        } else if parse_notification(&line).is_none() {
            return Err(OneShotParseError::Protocol(anyhow::anyhow!(
                "unexpected control-mode line: {}",
                text_lossy(&line)
            )));
        }
    }

    Ok(OneShotResponse { blocks, saw_exit })
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
            'response: loop {
                let header = loop {
                    let line = self.read_line().await?;
                    if let Some(header) = parse_block_header(&line)? {
                        break header;
                    }
                    self.record_notification(line)?;
                };

                let mut output: Vec<Vec<u8>> = Vec::new();
                let mut error = None;
                loop {
                    let line = self.read_line().await?;
                    if line.starts_with(b"%end ") {
                        break;
                    }
                    if line.starts_with(b"%error ") {
                        error = Some(line);
                        break;
                    }
                    if line.starts_with(b"%exit") {
                        bail!(
                            "tmux control client exited while a command was pending: {}",
                            text_lossy(&line)
                        );
                    }
                    output.push(line);
                }

                // Hook commands run in the same tmux command queue but are not
                // marked as responses to input received from this client. They
                // still emit complete control-mode blocks, so ignoring only
                // notifications is not enough to keep subsequent responses
                // aligned.
                if header.flags == 0 {
                    continue 'response;
                }

                if let Some(line) = error {
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
                break 'response;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockHeader {
    flags: i32,
}

fn parse_block_header(line: &[u8]) -> Result<Option<BlockHeader>> {
    if !line.starts_with(b"%begin ") {
        return Ok(None);
    }
    let fields: Vec<_> = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect();
    if fields.len() != 4 || fields[0] != b"%begin" {
        bail!("invalid tmux control block header: {}", text_lossy(line));
    }
    std::str::from_utf8(fields[1])
        .context("invalid UTF-8 in tmux control block timestamp")?
        .parse::<i64>()
        .context("invalid tmux control block timestamp")?;
    std::str::from_utf8(fields[2])
        .context("invalid UTF-8 in tmux control block command number")?
        .parse::<u32>()
        .context("invalid tmux control block command number")?;
    let flags = std::str::from_utf8(fields[3])
        .context("invalid UTF-8 in tmux control block flags")?
        .parse::<i32>()
        .context("invalid tmux control block flags")?;
    Ok(Some(BlockHeader { flags }))
}

fn text_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.split(|byte| *byte == b'\n').map(Vec::from).collect()
}

#[cfg(test)]
mod tests {
    use super::{OneShotParseError, parse_one_shot_output, split_lines};

    #[test]
    fn splits_command_output_without_a_synthetic_trailing_record() {
        assert!(split_lines(&[]).is_empty());
        assert_eq!(
            split_lines(b"one\ntwo\n"),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        assert_eq!(split_lines(b"\n"), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn parses_unflagged_one_shot_command_blocks() {
        let response = parse_one_shot_output(
            b"%begin 1 2 0\none\n%end 1 2 0\n%begin 1 3 0\ntwo\n%end 1 3 0\n%exit\n",
            2,
        )
        .unwrap_or_else(|_| panic!("valid one-shot control output was rejected"));
        assert!(response.saw_exit);
        assert_eq!(
            response.blocks,
            vec![vec![b"one".to_vec()], vec![b"two".to_vec()]]
        );
    }

    #[test]
    fn accepts_one_shot_command_block_flags() {
        let response = parse_one_shot_output(b"%begin 1 2 7\none\n%end 1 2 7\n%exit\n", 1)
            .unwrap_or_else(|_| panic!("flagged one-shot control output was rejected"));
        assert!(response.saw_exit);
        assert_eq!(response.blocks, vec![vec![b"one".to_vec()]]);
    }

    #[test]
    fn reports_one_shot_command_errors_after_completed_blocks() {
        let error = parse_one_shot_output(
            b"%begin 1 2 0\none\n%end 1 2 0\n%begin 1 3 0\nno target\n%error 1 3 0\n%exit\n",
            3,
        )
        .err()
        .expect("failing control output was accepted");
        let OneShotParseError::Command(error) = error else {
            panic!("command failure was misclassified as a protocol failure");
        };
        assert_eq!(
            error.to_string(),
            "tmux command sequence failed after 1 complete control blocks (expected 3 commands): \
             no target"
        );
    }
}

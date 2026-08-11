use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{model::EncodedPath, util::uid};

const PROTOCOL_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: u64 = 4_096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    Status,
    Stop,
    Reload,
}

impl ControlRequest {
    fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Stop => "stop",
            Self::Reload => "reload",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlAction {
    Continue,
    Stop,
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub protocol_version: u32,
    pub pid: u32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub socket: EncodedPath,
}

impl DaemonStatus {
    pub(crate) fn running(socket: &Path) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            version: crate::VERSION.to_owned(),
            started_at: Utc::now(),
            socket: EncodedPath::from_path(socket),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WireRequest {
    protocol_version: u32,
    action: ControlRequest,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireResponse {
    protocol_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<ControlRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<DaemonStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl WireResponse {
    fn status(status: DaemonStatus) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            action: Some(ControlRequest::Status),
            status: Some(status),
            error: None,
        }
    }

    fn acknowledged(action: ControlRequest) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            action: Some(action),
            status: None,
            error: None,
        }
    }

    fn error(error: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            action: None,
            status: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt},
        sync::Arc,
    };

    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        sync::{
            Mutex,
            mpsc::{self, UnboundedReceiver, UnboundedSender},
        },
        task::{JoinHandle, JoinSet},
        time::timeout,
    };

    use super::*;

    pub(crate) struct ControlServer {
        actions: UnboundedReceiver<Result<ControlAction>>,
        task: JoinHandle<()>,
        _socket: SocketGuard,
    }

    impl ControlServer {
        pub(crate) fn bind(
            data_dir: &Path,
            socket_key: &str,
            status: DaemonStatus,
        ) -> Result<Self> {
            let path = control_path(data_dir, socket_key)?;
            ensure_runtime_directory(
                path.parent()
                    .context("daemon control socket has no parent directory")?,
            )?;
            remove_stale_socket(&path)?;

            let listener = UnixListener::bind(&path).with_context(|| {
                format!("failed to bind daemon control socket {}", path.display())
            })?;
            let metadata = fs::symlink_metadata(&path).with_context(|| {
                format!("failed to inspect daemon control socket {}", path.display())
            })?;
            let guard = SocketGuard {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            let (action_sender, actions) = mpsc::unbounded_channel();
            let task = tokio::spawn(serve(listener, status, action_sender));
            Ok(Self {
                actions,
                task,
                _socket: guard,
            })
        }

        pub(crate) async fn next_request(&mut self) -> Result<ControlAction> {
            self.actions
                .recv()
                .await
                .context("daemon control server stopped unexpectedly")?
        }
    }

    impl Drop for ControlServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve(
        listener: UnixListener,
        status: DaemonStatus,
        actions: UnboundedSender<Result<ControlAction>>,
    ) {
        let mut requests = JoinSet::new();
        let lifecycle = Arc::new(Mutex::new(None));
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            requests.spawn(handle_request(
                                stream,
                                status.clone(),
                                Arc::clone(&lifecycle),
                            ));
                        }
                        Err(error) => {
                            let _ = actions.send(
                                Err(error).context("failed to accept a daemon control request")
                            );
                            return;
                        }
                    }
                }
                request = requests.join_next(), if !requests.is_empty() => {
                    match request {
                        Some(Ok(Ok(ControlAction::Continue))) => {}
                        Some(Ok(Ok(action))) => {
                            if actions.send(Ok(action)).is_err() {
                                return;
                            }
                        }
                        Some(Ok(Err(error))) => {
                            tracing::warn!(
                                error = %format!("{error:#}"),
                                "daemon control request failed"
                            );
                        }
                        Some(Err(error)) => {
                            tracing::warn!(
                                error = %error,
                                "daemon control request task failed"
                            );
                        }
                        None => {}
                    }
                }
            }
        }
    }

    struct SocketGuard {
        path: PathBuf,
        device: u64,
        inode: u64,
    }

    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let Ok(metadata) = fs::symlink_metadata(&self.path) else {
                return;
            };
            if metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    pub async fn request(
        data_dir: &Path,
        socket_key: &str,
        action: ControlRequest,
    ) -> Result<Option<DaemonStatus>> {
        let path = control_path(data_dir, socket_key)?;
        let response = timeout(REQUEST_TIMEOUT, exchange(&path, action))
            .await
            .with_context(|| {
                format!(
                    "timed out waiting for the tmux-recover daemon at {}",
                    path.display()
                )
            })??;
        if response.protocol_version != PROTOCOL_VERSION {
            bail!(
                "daemon control protocol {} is incompatible with client protocol {}",
                response.protocol_version,
                PROTOCOL_VERSION
            );
        }
        if !response.ok {
            bail!(
                "daemon rejected {} request: {}",
                action.as_str(),
                response.error.as_deref().unwrap_or("unknown error")
            );
        }
        if response.action != Some(action) {
            bail!(
                "daemon returned a mismatched response to {}",
                action.as_str()
            );
        }
        if action == ControlRequest::Status && response.status.is_none() {
            bail!("daemon status response did not include status");
        }
        Ok(response.status)
    }

    async fn exchange(path: &Path, action: ControlRequest) -> Result<WireResponse> {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|source| ConnectError {
                path: path.to_path_buf(),
                source,
            })?;
        let request = WireRequest {
            protocol_version: PROTOCOL_VERSION,
            action,
        };
        write_message(&mut stream, &request).await?;
        read_message(&mut stream)
            .await
            .context("failed to read daemon control response")
    }

    async fn handle_request(
        mut stream: UnixStream,
        status: DaemonStatus,
        lifecycle: Arc<Mutex<Option<ControlRequest>>>,
    ) -> Result<ControlAction> {
        let request: WireRequest = match timeout(REQUEST_TIMEOUT, read_message(&mut stream)).await {
            Ok(Ok(request)) => request,
            Ok(Err(error)) => {
                write_message(
                    &mut stream,
                    &WireResponse::error(format!("invalid request: {error:#}")),
                )
                .await?;
                return Ok(ControlAction::Continue);
            }
            Err(_) => {
                write_message(&mut stream, &WireResponse::error("request timed out")).await?;
                return Ok(ControlAction::Continue);
            }
        };
        if request.protocol_version != PROTOCOL_VERSION {
            write_message(
                &mut stream,
                &WireResponse::error(format!(
                    "unsupported control protocol {}, expected {}",
                    request.protocol_version, PROTOCOL_VERSION
                )),
            )
            .await?;
            return Ok(ControlAction::Continue);
        }

        match request.action {
            ControlRequest::Status => {
                write_message(&mut stream, &WireResponse::status(status)).await?;
                Ok(ControlAction::Continue)
            }
            action @ (ControlRequest::Stop | ControlRequest::Reload) => {
                let mut pending = lifecycle.lock().await;
                if let Some(pending) = *pending {
                    write_message(
                        &mut stream,
                        &WireResponse::error(format!("{} already requested", pending.as_str())),
                    )
                    .await?;
                    return Ok(ControlAction::Continue);
                }
                write_message(&mut stream, &WireResponse::acknowledged(action)).await?;
                *pending = Some(action);
                Ok(match action {
                    ControlRequest::Stop => ControlAction::Stop,
                    ControlRequest::Reload => ControlAction::Reload,
                    ControlRequest::Status => unreachable!(),
                })
            }
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("no controllable tmux-recover daemon was found at {path}")]
    struct ConnectError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    }

    pub fn is_daemon_unavailable(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause.downcast_ref::<ConnectError>().is_some_and(|error| {
                matches!(
                    error.source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                )
            })
        })
    }

    async fn read_message<T>(stream: &mut UnixStream) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut bytes = Vec::new();
        {
            let mut reader = BufReader::new(stream).take(MAX_MESSAGE_BYTES + 1);
            reader.read_until(b'\n', &mut bytes).await?;
        }
        if bytes.len() as u64 > MAX_MESSAGE_BYTES {
            bail!("daemon control message exceeds {MAX_MESSAGE_BYTES} bytes");
        }
        if bytes.is_empty() {
            bail!("daemon control peer closed without sending a message");
        }
        while bytes.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
            bytes.pop();
        }
        serde_json::from_slice(&bytes).context("daemon control message is not valid JSON")
    }

    async fn write_message<T>(stream: &mut UnixStream, message: &T) -> Result<()>
    where
        T: Serialize,
    {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    pub(super) fn ensure_runtime_directory(path: &Path) -> Result<()> {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create daemon runtime directory {}",
                        path.display()
                    )
                });
            }
        }
        let metadata = fs::symlink_metadata(path).with_context(|| {
            format!(
                "failed to inspect daemon runtime directory {}",
                path.display()
            )
        })?;
        if !metadata.file_type().is_dir() {
            bail!("daemon runtime path {} is not a directory", path.display());
        }
        if metadata.uid() != uid() {
            bail!(
                "daemon runtime directory {} is owned by uid {}, expected {}",
                path.display(),
                metadata.uid(),
                uid()
            );
        }
        if metadata.mode() & 0o077 != 0 {
            bail!(
                "daemon runtime directory {} must not be accessible by group or other users",
                path.display()
            );
        }
        Ok(())
    }

    fn remove_stale_socket(path: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect daemon control path {}", path.display())
                });
            }
        };
        if !metadata.file_type().is_socket() {
            bail!(
                "refusing to replace non-socket daemon control path {}",
                path.display()
            );
        }
        fs::remove_file(path)
            .with_context(|| format!("failed to remove stale daemon socket {}", path.display()))
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;

    pub(crate) struct ControlServer;

    impl ControlServer {
        pub(crate) fn bind(
            _data_dir: &Path,
            _socket_key: &str,
            _status: DaemonStatus,
        ) -> Result<Self> {
            bail!("daemon control is only supported on Unix")
        }

        pub(crate) async fn next_request(&mut self) -> Result<ControlAction> {
            bail!("daemon control is only supported on Unix")
        }
    }

    pub async fn request(
        _data_dir: &Path,
        _socket_key: &str,
        _action: ControlRequest,
    ) -> Result<Option<DaemonStatus>> {
        bail!("daemon control is only supported on Unix")
    }

    pub fn is_daemon_unavailable(_error: &anyhow::Error) -> bool {
        false
    }
}

pub(crate) use platform::ControlServer;
pub use platform::{is_daemon_unavailable, request};

fn control_path(data_dir: &Path, socket_key: &str) -> Result<PathBuf> {
    let data_dir = canonical_or_absolute(data_dir)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(uid().to_string().as_bytes());
    hasher.update(&[0]);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(data_dir.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(data_dir.to_string_lossy().as_bytes());
    hasher.update(&[0]);
    hasher.update(socket_key.as_bytes());
    let key = &hasher.finalize().to_hex()[..24];
    Ok(
        runtime_directory(std::env::var_os("XDG_RUNTIME_DIR").as_deref())?
            .join(format!("{key}.sock")),
    )
}

fn runtime_directory(xdg_runtime_dir: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(value) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            bail!(
                "XDG_RUNTIME_DIR must be an absolute path, got {}",
                path.display()
            );
        }
        return Ok(path.join("tmux-recover"));
    }
    Ok(PathBuf::from(format!("/tmp/tmux-recover-{}", uid())))
}

fn canonical_or_absolute(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn status(socket: &Path) -> DaemonStatus {
        DaemonStatus::running(socket)
    }

    #[tokio::test]
    async fn status_round_trips_over_the_private_control_socket() {
        let data = tempfile::tempdir().unwrap();
        let socket = data.path().join("tmux.sock");
        let expected = status(&socket);
        let _server = ControlServer::bind(data.path(), "socket-key", expected.clone()).unwrap();

        let actual = request(data.path(), "socket-key", ControlRequest::Status)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn stop_is_acknowledged_before_the_server_returns_the_action() {
        let data = tempfile::tempdir().unwrap();
        let socket = data.path().join("tmux.sock");
        let mut server = ControlServer::bind(data.path(), "socket-key", status(&socket)).unwrap();

        assert_eq!(
            request(data.path(), "socket-key", ControlRequest::Stop)
                .await
                .unwrap(),
            None
        );
        let error = request(data.path(), "socket-key", ControlRequest::Reload)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("stop already requested"));
        assert_eq!(server.next_request().await.unwrap(), ControlAction::Stop);
    }

    #[tokio::test]
    async fn an_idle_connection_does_not_block_status_requests() {
        let data = tempfile::tempdir().unwrap();
        let socket = data.path().join("tmux.sock");
        let expected = status(&socket);
        let _server = ControlServer::bind(data.path(), "socket-key", expected.clone()).unwrap();
        let path = control_path(data.path(), "socket-key").unwrap();
        let _idle = tokio::net::UnixStream::connect(path).await.unwrap();

        let actual = tokio::time::timeout(
            Duration::from_secs(1),
            request(data.path(), "socket-key", ControlRequest::Status),
        )
        .await
        .expect("status request was blocked by an idle connection")
        .unwrap()
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn only_connect_failures_report_an_unavailable_daemon() {
        let data = tempfile::tempdir().unwrap();
        let error = request(data.path(), "missing", ControlRequest::Status)
            .await
            .unwrap_err();
        assert!(is_daemon_unavailable(&error));
        assert!(!is_daemon_unavailable(&anyhow::anyhow!(
            "protocol mismatch"
        )));
    }

    #[test]
    fn data_directories_isolate_control_endpoints_for_the_same_socket() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert_ne!(
            control_path(first.path(), "socket-key").unwrap(),
            control_path(second.path(), "socket-key").unwrap()
        );
    }

    #[test]
    fn startup_refuses_to_replace_a_regular_control_path() {
        let data = tempfile::tempdir().unwrap();
        let path = control_path(data.path(), "socket-key").unwrap();
        platform::ensure_runtime_directory(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"do not replace").unwrap();

        let error = match ControlServer::bind(data.path(), "socket-key", status(Path::new("tmux")))
        {
            Ok(_) => panic!("regular control path was replaced"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("refusing to replace non-socket"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"do not replace");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn startup_replaces_a_stale_control_socket() {
        let data = tempfile::tempdir().unwrap();
        let path = control_path(data.path(), "socket-key").unwrap();
        platform::ensure_runtime_directory(path.parent().unwrap()).unwrap();
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);

        let server =
            ControlServer::bind(data.path(), "socket-key", status(Path::new("tmux"))).unwrap();
        assert!(path.exists());
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn runtime_directory_prefers_an_absolute_xdg_path() {
        assert_eq!(
            runtime_directory(Some(OsStr::new("/run/user/1000"))).unwrap(),
            Path::new("/run/user/1000/tmux-recover")
        );
        assert_eq!(
            runtime_directory(None).unwrap(),
            PathBuf::from(format!("/tmp/tmux-recover-{}", uid()))
        );
    }

    #[test]
    fn runtime_directory_rejects_a_relative_xdg_path() {
        let error = runtime_directory(Some(OsStr::new("relative/runtime"))).unwrap_err();
        assert!(format!("{error:#}").contains("must be an absolute path"));
    }
}

use std::{collections::BTreeMap, ffi::OsStr, path::Path};

use anyhow::{Context, Result, bail};

use crate::{
    VERSION,
    model::{
        Diagnostic, EncodedPath, Origin, Pane, PaneCwd, Session, SocketIdentity, TmuxState, Window,
        WindowLink,
    },
    process::collect_restart_specs,
    util::{canonical_socket_path, hostname, require_tmux_37, uid},
};

use super::control::ControlClient;

const SEPARATOR: u8 = b'|';

pub struct CaptureResult {
    pub origin: Origin,
    pub state: TmuxState,
    pub diagnostics: Vec<Diagnostic>,
    pub default_shell: Option<String>,
}

pub async fn capture(client: &mut ControlClient, requested_socket: &Path) -> Result<CaptureResult> {
    let output = client
        .execute_blocks(&capture_command(), 4)
        .await?
        .into_iter()
        .flatten()
        .collect();
    parse_capture(output, requested_socket)
}

fn capture_command() -> String {
    let s = |name| escaped_format(name);
    let session = format!(
        "S|{}|{}|{}|{}|{}",
        s("session_id"),
        s("session_name"),
        s("session_group"),
        s("session_created"),
        s("session_grouped")
    );
    let window = format!(
        "W|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        s("session_id"),
        s("window_id"),
        s("window_index"),
        s("window_name"),
        s("window_active"),
        s("window_last_flag"),
        s("window_layout"),
        s("window_visible_layout"),
        s("window_zoomed_flag"),
        s("automatic-rename"),
        s("window_width"),
        s("window_height"),
        s("pane_id"),
        s("session_name")
    );
    let pane = format!(
        "P|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        s("window_id"),
        s("pane_id"),
        s("pane_index"),
        s("pane_title"),
        s("pane_current_path"),
        s("pane_active"),
        s("pane_current_command"),
        s("pane_start_command"),
        s("pane_start_path"),
        s("pane_pid"),
        s("pane_tty"),
        s("pane_dead"),
        s("pane_dead_status"),
        s("session_id")
    );
    let metadata = format!(
        "M|{}|{}|{}|{}|{}",
        s("version"),
        s("pid"),
        s("start_time"),
        s("socket_path"),
        s("default-shell")
    );
    format!(
        "list-sessions -F \"{session}\" ; list-windows -a -F \"{window}\" ; list-panes -a -F \"{pane}\" ; display-message -p -F \"{metadata}\""
    )
}

fn escaped_format(name: &str) -> String {
    format!("#{{s|%|%25|;s/[|]/%7C/;s|\\011|%09|;s|\\012|%0A|;s|\\015|%0D|:{name}}}")
}

fn parse_capture(lines: Vec<Vec<u8>>, requested_socket: &Path) -> Result<CaptureResult> {
    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut windows: BTreeMap<String, Window> = BTreeMap::new();
    let mut pane_rows = Vec::new();
    let mut metadata = None;

    for line in lines {
        let fields = parse_record(&line)?;
        match fields.first().map(Vec::as_slice) {
            Some(b"S") => parse_session(&fields, &mut sessions)?,
            Some(b"W") => parse_window(&fields, &mut sessions, &mut windows)?,
            Some(b"P") => pane_rows.push(fields),
            Some(b"M") => metadata = Some(fields),
            _ => bail!("unknown tmux capture record"),
        }
    }

    let restart_specs = collect_restart_specs(
        pane_rows
            .iter()
            .filter_map(|fields| field_u32(fields, 10).ok().flatten()),
    );
    for fields in pane_rows {
        let window_id = field_string(&fields, 1)?;
        let pane = parse_pane(&fields, &restart_specs)?;
        let window = windows
            .get_mut(&window_id)
            .with_context(|| format!("pane {} references unknown window {window_id}", pane.id))?;
        if !window.panes.iter().any(|existing| existing.id == pane.id) {
            if field_bool(&fields, 6)? {
                window.active_pane_id = Some(pane.id.clone());
            }
            window.panes.push(pane);
        }
    }
    for window in windows.values_mut() {
        window.panes.sort_by_key(|pane| pane.index);
    }

    let metadata = metadata.context("tmux capture did not return server metadata")?;
    let tmux_version = field_string(&metadata, 1)?;
    require_tmux_37(&tmux_version)?;
    let socket_bytes = field(&metadata, 4)?;
    let socket_path = encoded_path(socket_bytes);
    let hostname = hostname()?;
    let uid = uid();
    // tmux reports the path it was started with, which may be a filesystem
    // alias such as macOS /var for /private/var. The requested path identifies
    // the socket we actually connected to and is canonicalized by every other
    // store lookup, so derive the key from that same identity while preserving
    // tmux's original spelling in the snapshot metadata below.
    let socket = SocketIdentity::new(&canonical_socket_path(requested_socket)?, &hostname, uid)?;
    let state = TmuxState {
        sessions: sessions.into_values().collect(),
        windows: windows.into_values().collect(),
    };
    state.validate()?;

    Ok(CaptureResult {
        origin: Origin {
            hostname,
            uid,
            os: std::env::consts::OS.to_owned(),
            tool_version: VERSION.to_owned(),
            tmux_version: Some(tmux_version),
            socket: Some(SocketIdentity {
                path: socket_path,
                ..socket
            }),
            server_pid: field_u32(&metadata, 2)?,
            server_started_at: field_i64(&metadata, 3)?,
        },
        state,
        diagnostics: Vec::new(),
        default_shell: field_optional_string(&metadata, 5)?,
    })
}

fn parse_session(fields: &[Vec<u8>], sessions: &mut BTreeMap<String, Session>) -> Result<()> {
    expect_fields(fields, 6)?;
    let id = field_string(fields, 1)?;
    let grouped = field_bool(fields, 5)?;
    sessions.insert(
        id.clone(),
        Session {
            id,
            name: field_string(fields, 2)?,
            group: grouped.then(|| field_string(fields, 3)).transpose()?,
            created_at: field_i64(fields, 4)?,
            active_window_id: None,
            last_window_id: None,
            windows: Vec::new(),
        },
    );
    Ok(())
}

fn parse_window(
    fields: &[Vec<u8>],
    sessions: &mut BTreeMap<String, Session>,
    windows: &mut BTreeMap<String, Window>,
) -> Result<()> {
    expect_fields(fields, 15)?;
    let session_id = field_string(fields, 1)?;
    let window_id = field_string(fields, 2)?;
    let index = field_i32_required(fields, 3)?;
    let session = sessions
        .get_mut(&session_id)
        .with_context(|| format!("window references unknown session {session_id}"))?;
    if !session
        .windows
        .iter()
        .any(|link| link.window_id == window_id)
    {
        session.windows.push(WindowLink {
            window_id: window_id.clone(),
            index,
        });
    }
    if field_bool(fields, 5)? {
        session.active_window_id = Some(window_id.clone());
    }
    if field_bool(fields, 6)? {
        session.last_window_id = Some(window_id.clone());
    }
    windows.entry(window_id.clone()).or_insert(Window {
        id: window_id,
        name: field_string(fields, 4)?,
        layout: field_string(fields, 7)?,
        visible_layout: field_optional_string(fields, 8)?,
        width: field_u32(fields, 11)?.unwrap_or_default(),
        height: field_u32(fields, 12)?.unwrap_or_default(),
        zoomed: field_bool(fields, 9)?,
        automatic_rename: field_optional_bool(fields, 10)?,
        active_pane_id: field_optional_string(fields, 13)?,
        panes: Vec::new(),
    });
    Ok(())
}

fn parse_pane(fields: &[Vec<u8>], restart_specs: &BTreeMapCompat) -> Result<Pane> {
    expect_fields(fields, 15)?;
    let id = field_string(fields, 2)?;
    let pid = field_u32(fields, 10)?;
    let path = field(fields, 5)?;
    let cwd_path = (!path.is_empty()).then(|| encoded_path(path));
    Ok(Pane {
        id,
        index: field_i32_required(fields, 3)?,
        title: Some(field_string(fields, 4)?),
        cwd: PaneCwd::inspect(cwd_path),
        current_command: field_optional_string(fields, 7)?,
        start_command: field_optional_string(fields, 8)?,
        start_path: field_optional_path(fields, 9)?,
        pid,
        tty: field_optional_string(fields, 11)?,
        dead: field_bool(fields, 12)?,
        dead_status: field_i32(fields, 13)?,
        restart: pid.and_then(|pid| restart_specs.get(&pid).cloned()),
        import_status: None,
    })
}

type BTreeMapCompat = std::collections::HashMap<u32, crate::model::RestartSpec>;

fn parse_record(line: &[u8]) -> Result<Vec<Vec<u8>>> {
    line.split(|byte| *byte == SEPARATOR)
        .map(percent_decode)
        .collect()
}

fn percent_decode(input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'%' {
            output.push(input[index]);
            index += 1;
            continue;
        }
        let hex = input
            .get(index + 1..index + 3)
            .context("truncated percent escape in tmux output")?;
        let text = std::str::from_utf8(hex).context("invalid percent escape in tmux output")?;
        output.push(u8::from_str_radix(text, 16).context("invalid percent escape in tmux output")?);
        index += 3;
    }
    Ok(output)
}

fn encoded_path(bytes: &[u8]) -> EncodedPath {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        EncodedPath::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        EncodedPath::Utf8 {
            value: String::from_utf8_lossy(bytes).into_owned(),
        }
    }
}

fn expect_fields(fields: &[Vec<u8>], expected: usize) -> Result<()> {
    if fields.len() != expected {
        bail!(
            "tmux record has {} fields, expected {expected}",
            fields.len()
        );
    }
    Ok(())
}

fn field(fields: &[Vec<u8>], index: usize) -> Result<&[u8]> {
    fields
        .get(index)
        .map(Vec::as_slice)
        .context("tmux record is missing a field")
}

fn field_string(fields: &[Vec<u8>], index: usize) -> Result<String> {
    String::from_utf8(field(fields, index)?.to_vec()).context("tmux returned invalid UTF-8 text")
}

fn field_optional_string(fields: &[Vec<u8>], index: usize) -> Result<Option<String>> {
    let bytes = field(fields, index)?;
    if bytes.is_empty() {
        Ok(None)
    } else {
        String::from_utf8(bytes.to_vec())
            .map(Some)
            .context("tmux returned invalid UTF-8 text")
    }
}

fn field_optional_path(fields: &[Vec<u8>], index: usize) -> Result<Option<EncodedPath>> {
    let bytes = field(fields, index)?;
    Ok((!bytes.is_empty()).then(|| encoded_path(bytes)))
}

fn field_bool(fields: &[Vec<u8>], index: usize) -> Result<bool> {
    match field(fields, index)? {
        b"1" => Ok(true),
        b"0" | b"" => Ok(false),
        value => bail!("invalid tmux boolean {}", String::from_utf8_lossy(value)),
    }
}

fn field_optional_bool(fields: &[Vec<u8>], index: usize) -> Result<Option<bool>> {
    match field(fields, index)? {
        b"1" | b"on" => Ok(Some(true)),
        b"0" | b"off" => Ok(Some(false)),
        b"" => Ok(None),
        value => bail!("invalid tmux boolean {}", String::from_utf8_lossy(value)),
    }
}

fn field_i64(fields: &[Vec<u8>], index: usize) -> Result<Option<i64>> {
    let value = field(fields, index)?;
    if value.is_empty() {
        Ok(None)
    } else {
        String::from_utf8_lossy(value)
            .parse()
            .map(Some)
            .context("invalid tmux integer")
    }
}

fn field_i32(fields: &[Vec<u8>], index: usize) -> Result<Option<i32>> {
    field_i64(fields, index)?
        .map(i32::try_from)
        .transpose()
        .context("tmux integer is out of range")
}

fn field_i32_required(fields: &[Vec<u8>], index: usize) -> Result<i32> {
    field_i32(fields, index)?.context("required tmux integer is empty")
}

fn field_u32(fields: &[Vec<u8>], index: usize) -> Result<Option<u32>> {
    field_i64(fields, index)?
        .map(u32::try_from)
        .transpose()
        .context("tmux integer is out of range")
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn percent_decoder_preserves_empty_and_special_fields() {
        let fields = parse_record(b"P||a%7Cb|tab%09x|line%0Aend|100%25").unwrap();
        assert_eq!(fields[1], b"");
        assert_eq!(fields[2], b"a|b");
        assert_eq!(fields[3], b"tab\tx");
        assert_eq!(fields[4], b"line\nend");
        assert_eq!(fields[5], b"100%");
    }

    #[test]
    fn format_encoder_is_line_safe() {
        let format = escaped_format("pane_current_path");
        assert!(!format.contains('\n'));
        assert!(format.contains("\\012"));
    }

    #[cfg(unix)]
    #[test]
    fn capture_keys_temporary_socket_aliases_by_canonical_path() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("tmux.sock");
        let alias = directory.path().join("tmux-alias.sock");
        std::fs::write(&socket, b"").unwrap();
        symlink(&socket, &alias).unwrap();

        let metadata = format!("M|tmux 3.7b|123|456|{}|/bin/sh", alias.to_string_lossy());
        let captured = parse_capture(vec![metadata.into_bytes()], &socket).unwrap();
        let identity = captured.origin.socket.unwrap();

        assert_eq!(identity.path.to_path_buf().unwrap(), alias);
        assert_eq!(
            identity.key,
            crate::util::socket_identity(&socket).unwrap().key
        );
    }
}

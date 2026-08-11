use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};

use crate::{
    VERSION,
    model::{
        ClientSessionState, ClientState, Diagnostic, EncodedPath, ImportStatus, Origin, Pane,
        PaneCwd, Session, Severity, Snapshot, SnapshotSource, TmuxState, Window, WindowLink,
    },
    util::{hostname, uid},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResurrectVersion {
    V3,
    V4,
}

impl ResurrectVersion {
    fn label(self) -> &'static str {
        match self {
            Self::V3 => "v3",
            Self::V4 => "v4",
        }
    }
}

pub struct ImportResult {
    pub snapshot: Snapshot,
    pub exact_panes: usize,
    pub repaired_panes: usize,
    pub ambiguous_panes: usize,
}

pub fn import_resurrect(path: &Path) -> Result<ImportResult> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read resurrect snapshot {}", path.display()))?;
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let input = std::str::from_utf8(&bytes).context("resurrect snapshot is not valid UTF-8")?;
    let rows: Vec<Row<'_>> = input
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            (!line.is_empty()).then(|| Row {
                number: index + 1,
                fields: line.split('\t').collect(),
            })
        })
        .collect();
    let version = detect_version(&rows)?;
    let mut builder = Builder::new(version);
    builder.parse(&rows)?;
    let (state, diagnostics, exact_panes, repaired_panes, ambiguous_panes) = builder.finish()?;
    let source_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let label = path
        .file_name()
        .map(|name| format!("resurrect import: {}", name.to_string_lossy()));
    let snapshot = Snapshot::new(
        label,
        SnapshotSource::ResurrectImport {
            source_path: EncodedPath::from_path(&source_path),
            source_digest: digest,
            detected_version: version.label().to_owned(),
        },
        Origin {
            hostname: hostname()?,
            uid: uid(),
            os: std::env::consts::OS.to_owned(),
            tool_version: VERSION.to_owned(),
            tmux_version: None,
            socket: None,
            server_pid: None,
            server_started_at: None,
        },
        state,
        diagnostics,
    )?;
    Ok(ImportResult {
        snapshot,
        exact_panes,
        repaired_panes,
        ambiguous_panes,
    })
}

struct Row<'a> {
    number: usize,
    fields: Vec<&'a str>,
}

fn detect_version(rows: &[Row<'_>]) -> Result<ResurrectVersion> {
    let mut detected = None;
    for row in rows {
        let Some(kind) = row.fields.first() else {
            continue;
        };
        let candidate = match (*kind, row.fields.len()) {
            ("window", 6) => Some(ResurrectVersion::V3),
            ("window", 8) => Some(ResurrectVersion::V4),
            _ => None,
        };
        if let Some(candidate) = candidate {
            if detected.is_some_and(|current| current != candidate) {
                bail!("resurrect snapshot mixes v3 and v4 window records");
            }
            detected = Some(candidate);
        }
    }
    if let Some(version) = detected {
        return Ok(version);
    }
    for row in rows
        .iter()
        .filter(|row| row.fields.first() == Some(&"pane"))
    {
        if row.fields.get(3).is_some_and(|field| is_bool(field)) {
            return Ok(ResurrectVersion::V4);
        }
        if row
            .fields
            .get(3)
            .is_some_and(|field| field.starts_with(':'))
        {
            return Ok(ResurrectVersion::V3);
        }
    }
    bail!("could not detect resurrect v3 or v4 format")
}

struct Builder {
    version: ResurrectVersion,
    sessions: BTreeMap<String, Session>,
    windows: BTreeMap<(String, i32), Window>,
    grouped: Vec<GroupRecord>,
    client_state: Option<StateRecord>,
    diagnostics: Vec<Diagnostic>,
    exact_panes: usize,
    repaired_panes: usize,
    ambiguous_panes: usize,
}

impl Builder {
    fn new(version: ResurrectVersion) -> Self {
        Self {
            version,
            sessions: BTreeMap::new(),
            windows: BTreeMap::new(),
            grouped: Vec::new(),
            client_state: None,
            diagnostics: Vec::new(),
            exact_panes: 0,
            repaired_panes: 0,
            ambiguous_panes: 0,
        }
    }

    fn parse(&mut self, rows: &[Row<'_>]) -> Result<()> {
        for row in rows
            .iter()
            .filter(|row| row.fields.first() == Some(&"window"))
        {
            if let Err(error) = self.parse_window(row) {
                self.line_error(row.number, "invalid_window_record", error);
            }
        }
        for row in rows {
            match row.fields.first().copied() {
                Some("pane") => {
                    if let Err(error) = self.parse_pane(row) {
                        self.line_error(row.number, "invalid_pane_record", error);
                    }
                }
                Some("grouped_session") => {
                    if let Err(error) = self.parse_group(row) {
                        self.line_error(row.number, "invalid_group_record", error);
                    }
                }
                Some("state") => {
                    if let Err(error) = self.parse_state(row) {
                        self.line_error(row.number, "invalid_state_record", error);
                    }
                }
                Some("window") => {}
                Some(kind) => self.diagnostics.push(Diagnostic {
                    severity: Severity::Warning,
                    code: "unknown_resurrect_record".to_owned(),
                    object_id: None,
                    message: format!("line {} has unknown record type {kind:?}", row.number),
                }),
                None => {}
            }
        }
        Ok(())
    }

    fn parse_window(&mut self, row: &Row<'_>) -> Result<()> {
        let (session_name, index, name, active, flags, layout, automatic_rename) =
            match self.version {
                ResurrectVersion::V3 => {
                    expect_fields(row, 6)?;
                    (
                        row.fields[1],
                        parse_i32(row.fields[2], "window index")?,
                        String::new(),
                        parse_bool(row.fields[3], "window active")?,
                        strip_marker(row.fields[4]).to_owned(),
                        row.fields[5].to_owned(),
                        None,
                    )
                }
                ResurrectVersion::V4 => {
                    expect_fields(row, 8)?;
                    (
                        row.fields[1],
                        parse_i32(row.fields[2], "window index")?,
                        strip_marker(row.fields[3]).to_owned(),
                        parse_bool(row.fields[4], "window active")?,
                        strip_marker(row.fields[5]).to_owned(),
                        row.fields[6].to_owned(),
                        parse_automatic_rename(row.fields[7])?,
                    )
                }
            };
        let window_id = window_id(session_name, index);
        let (width, height) = layout_size(&layout);
        self.ensure_session(session_name);
        let session = self.sessions.get_mut(session_name).expect("session exists");
        if !session.windows.iter().any(|link| link.index == index) {
            session.windows.push(WindowLink {
                window_id: window_id.clone(),
                index,
            });
        }
        if active || flags.contains('*') {
            session.active_window_id = Some(window_id.clone());
        }
        if flags.contains('-') {
            session.last_window_id = Some(window_id.clone());
        }
        self.windows.insert(
            (session_name.to_owned(), index),
            Window {
                id: window_id,
                name,
                layout,
                visible_layout: None,
                width,
                height,
                zoomed: flags.contains('Z'),
                automatic_rename,
                active_pane_id: None,
                panes: Vec::new(),
            },
        );
        Ok(())
    }

    fn parse_pane(&mut self, row: &Row<'_>) -> Result<()> {
        let parsed = match self.version {
            ResurrectVersion::V3 => parse_v3_pane(row)?,
            ResurrectVersion::V4 => parse_v4_pane(row)?,
        };
        match parsed.status {
            ImportStatus::Exact => self.exact_panes += 1,
            ImportStatus::RepairedEmptyTitleShift => {
                self.repaired_panes += 1;
                self.diagnostics.push(Diagnostic {
                    severity: Severity::Warning,
                    code: "repaired_empty_title_shift".to_owned(),
                    object_id: Some(pane_id(&parsed.session, parsed.window_index, parsed.index)),
                    message: format!(
                        "line {} matched the tmux-resurrect v4 empty-title field shift; cwd and active state were repaired, but the full process command was discarded",
                        row.number
                    ),
                });
            }
            ImportStatus::Ambiguous | ImportStatus::Lossy => {
                self.ambiguous_panes += 1;
                self.diagnostics.push(Diagnostic {
                    severity: Severity::Warning,
                    code: "ambiguous_pane_fields".to_owned(),
                    object_id: Some(pane_id(&parsed.session, parsed.window_index, parsed.index)),
                    message: format!(
                        "line {} contained extra Tab-separated fields and was recovered heuristically",
                        row.number
                    ),
                });
            }
        }
        self.ensure_session(&parsed.session);
        let key = (parsed.session.clone(), parsed.window_index);
        if !self.windows.contains_key(&key) {
            let id = window_id(&parsed.session, parsed.window_index);
            let session = self
                .sessions
                .get_mut(&parsed.session)
                .expect("session exists");
            session.windows.push(WindowLink {
                window_id: id.clone(),
                index: parsed.window_index,
            });
            self.windows.insert(
                key.clone(),
                Window {
                    id,
                    name: parsed.window_name.clone().unwrap_or_default(),
                    layout: "tiled".to_owned(),
                    visible_layout: None,
                    width: 0,
                    height: 0,
                    zoomed: parsed.flags.contains('Z'),
                    automatic_rename: None,
                    active_pane_id: None,
                    panes: Vec::new(),
                },
            );
            self.diagnostics.push(Diagnostic {
                severity: Severity::Warning,
                code: "missing_window_record".to_owned(),
                object_id: Some(window_id(&parsed.session, parsed.window_index)),
                message: format!(
                    "line {} referenced a window without a window record; using the tiled layout",
                    row.number
                ),
            });
        }
        let window = self.windows.get_mut(&key).expect("window exists");
        if window.name.is_empty() {
            if let Some(name) = &parsed.window_name {
                window.name.clone_from(name);
            }
        }
        let id = pane_id(&parsed.session, parsed.window_index, parsed.index);
        if parsed.active {
            window.active_pane_id = Some(id.clone());
        }
        let encoded_cwd = parsed
            .cwd
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| EncodedPath::from_path(Path::new(&unescape_legacy_path(&cwd))));
        window.panes.push(Pane {
            id,
            index: parsed.index,
            title: parsed.title,
            cwd: PaneCwd::inspect(encoded_cwd),
            current_command: nonempty(parsed.command),
            start_command: parsed.full_command.and_then(nonempty),
            start_path: None,
            pid: None,
            tty: None,
            dead: false,
            dead_status: None,
            restart: None,
            import_status: Some(parsed.status),
        });
        Ok(())
    }

    fn parse_group(&mut self, row: &Row<'_>) -> Result<()> {
        expect_fields(row, 5)?;
        self.grouped.push(GroupRecord {
            name: row.fields[1].to_owned(),
            original: row.fields[2].to_owned(),
            alternate_index: parse_optional_marker_i32(row.fields[3])?,
            active_index: parse_optional_marker_i32(row.fields[4])?,
        });
        Ok(())
    }

    fn parse_state(&mut self, row: &Row<'_>) -> Result<()> {
        expect_fields(row, 3)?;
        if self.client_state.is_some() {
            bail!("resurrect snapshot contains more than one state record");
        }
        self.client_state = Some(StateRecord {
            current_session: row.fields[1].to_owned(),
            last_session: (!row.fields[2].is_empty()).then(|| row.fields[2].to_owned()),
        });
        Ok(())
    }

    fn ensure_session(&mut self, name: &str) {
        self.sessions.entry(name.to_owned()).or_insert(Session {
            id: session_id(name),
            name: name.to_owned(),
            group: None,
            created_at: None,
            active_window_id: None,
            last_window_id: None,
            windows: Vec::new(),
        });
    }

    fn line_error(&mut self, line: usize, code: &str, error: anyhow::Error) {
        self.diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: code.to_owned(),
            object_id: None,
            message: format!("line {line}: {error:#}"),
        });
    }

    fn finish(mut self) -> Result<(TmuxState, Vec<Diagnostic>, usize, usize, usize)> {
        for group in &self.grouped {
            let Some(original) = self.sessions.get(&group.original).cloned() else {
                self.diagnostics.push(Diagnostic {
                    severity: Severity::Error,
                    code: "missing_group_origin".to_owned(),
                    object_id: Some(group.name.clone()),
                    message: format!(
                        "grouped session {} references missing session {}",
                        group.name, group.original
                    ),
                });
                continue;
            };
            let group_id = format!("resurrect-group:{}", stable_id(&[&group.original]));
            if let Some(original_mut) = self.sessions.get_mut(&group.original) {
                original_mut.group = Some(group_id.clone());
            }
            let mut linked = Session {
                id: session_id(&group.name),
                name: group.name.clone(),
                group: Some(group_id),
                created_at: None,
                active_window_id: window_at_index(&original, group.active_index),
                last_window_id: window_at_index(&original, group.alternate_index),
                windows: original.windows,
            };
            linked.windows.sort_by_key(|link| link.index);
            self.sessions.insert(group.name.clone(), linked);
        }
        for session in self.sessions.values_mut() {
            session.windows.sort_by_key(|link| link.index);
            if session.active_window_id.is_none() {
                session.active_window_id =
                    session.windows.first().map(|link| link.window_id.clone());
            }
        }
        for window in self.windows.values_mut() {
            window.panes.sort_by_key(|pane| pane.index);
            if window.active_pane_id.is_none() {
                window.active_pane_id = window.panes.first().map(|pane| pane.id.clone());
            }
        }
        if self.sessions.is_empty() || self.windows.is_empty() {
            bail!("resurrect snapshot contains no usable sessions and windows");
        }
        let client_state = match self.client_state {
            Some(record) => match self.sessions.get(&record.current_session) {
                Some(current) => {
                    let last_session_id = match record.last_session {
                        Some(name) => match self.sessions.get(&name) {
                            Some(session) => Some(session.id.clone()),
                            None => {
                                self.diagnostics.push(Diagnostic {
                                    severity: Severity::Warning,
                                    code: "missing_state_last_session".to_owned(),
                                    object_id: Some(name.clone()),
                                    message: format!(
                                        "resurrect state references missing last session {name}; only the current session will be restored"
                                    ),
                                });
                                None
                            }
                        },
                        None => None,
                    };
                    Some(ClientState {
                        attachments: vec![ClientSessionState {
                            session_id: current.id.clone(),
                            last_session_id,
                        }],
                    })
                }
                None => {
                    self.diagnostics.push(Diagnostic {
                        severity: Severity::Error,
                        code: "missing_state_current_session".to_owned(),
                        object_id: Some(record.current_session.clone()),
                        message: format!(
                            "resurrect state references missing current session {}",
                            record.current_session
                        ),
                    });
                    None
                }
            },
            None => None,
        };
        let state = TmuxState {
            sessions: self.sessions.into_values().collect(),
            windows: self.windows.into_values().collect(),
            client_state,
        };
        state.validate()?;
        Ok((
            state,
            self.diagnostics,
            self.exact_panes,
            self.repaired_panes,
            self.ambiguous_panes,
        ))
    }
}

struct ParsedPane {
    session: String,
    window_index: i32,
    window_name: Option<String>,
    flags: String,
    index: i32,
    title: Option<String>,
    cwd: Option<String>,
    active: bool,
    command: String,
    full_command: Option<String>,
    status: ImportStatus,
}

struct StateRecord {
    current_session: String,
    last_session: Option<String>,
}

fn parse_v3_pane(row: &Row<'_>) -> Result<ParsedPane> {
    expect_fields(row, 11)?;
    Ok(ParsedPane {
        session: row.fields[1].to_owned(),
        window_index: parse_i32(row.fields[2], "window index")?,
        window_name: Some(strip_marker(row.fields[3]).to_owned()),
        flags: strip_marker(row.fields[5]).to_owned(),
        index: parse_i32(row.fields[6], "pane index")?,
        title: None,
        cwd: Some(strip_marker(row.fields[7]).to_owned()),
        active: parse_bool(row.fields[8], "pane active")?,
        command: row.fields[9].to_owned(),
        full_command: Some(strip_marker(row.fields[10]).to_owned()),
        status: ImportStatus::Exact,
    })
}

fn parse_v4_pane(row: &Row<'_>) -> Result<ParsedPane> {
    if row.fields.len() < 11 {
        bail!(
            "v4 pane record has {} fields, expected at least 11",
            row.fields.len()
        );
    }
    let session = row.fields[1].to_owned();
    let window_index = parse_i32(row.fields[2], "window index")?;
    let flags = strip_marker(row.fields[4]).to_owned();
    let index = parse_i32(row.fields[5], "pane index")?;

    if row.fields.len() == 11
        && row.fields[6].starts_with(':')
        && is_bool(row.fields[7])
        && !is_bool(row.fields[8])
        && (row.fields[9].is_empty() || row.fields[9].parse::<u32>().is_ok())
    {
        return Ok(ParsedPane {
            session,
            window_index,
            window_name: None,
            flags,
            index,
            title: Some(String::new()),
            cwd: Some(strip_marker(row.fields[6]).to_owned()),
            active: parse_bool(row.fields[7], "repaired pane active")?,
            command: row.fields[8].to_owned(),
            full_command: None,
            status: ImportStatus::RepairedEmptyTitleShift,
        });
    }
    if row.fields.len() == 11 {
        return Ok(ParsedPane {
            session,
            window_index,
            window_name: None,
            flags,
            index,
            title: Some(row.fields[6].to_owned()),
            cwd: Some(strip_marker(row.fields[7]).to_owned()),
            active: parse_bool(row.fields[8], "pane active")?,
            command: row.fields[9].to_owned(),
            full_command: Some(strip_marker(row.fields[10]).to_owned()),
            status: ImportStatus::Exact,
        });
    }

    let active_index = row.fields.len() - 3;
    let cwd_index = row.fields[6..active_index]
        .iter()
        .position(|field| field.starts_with(':'))
        .map(|index| index + 6)
        .context("could not locate cwd in pane record with extra fields")?;
    let title = row.fields[6..cwd_index].join("\t");
    let cwd = row.fields[cwd_index..active_index].join("\t");
    Ok(ParsedPane {
        session,
        window_index,
        window_name: None,
        flags,
        index,
        title: Some(title),
        cwd: Some(strip_marker(&cwd).to_owned()),
        active: parse_bool(row.fields[active_index], "pane active")?,
        command: row.fields[active_index + 1].to_owned(),
        full_command: Some(strip_marker(row.fields[active_index + 2]).to_owned()),
        status: ImportStatus::Ambiguous,
    })
}

struct GroupRecord {
    name: String,
    original: String,
    alternate_index: Option<i32>,
    active_index: Option<i32>,
}

fn window_at_index(session: &Session, index: Option<i32>) -> Option<String> {
    index.and_then(|index| {
        session
            .windows
            .iter()
            .find(|link| link.index == index)
            .map(|link| link.window_id.clone())
    })
}

fn expect_fields(row: &Row<'_>, expected: usize) -> Result<()> {
    if row.fields.len() != expected {
        bail!(
            "record has {} fields, expected {expected}",
            row.fields.len()
        );
    }
    Ok(())
}

fn parse_bool(value: &str, name: &str) -> Result<bool> {
    match value {
        "1" => Ok(true),
        "0" | "" => Ok(false),
        _ => bail!("{name} has invalid boolean {value:?}"),
    }
}

fn is_bool(value: &str) -> bool {
    matches!(value, "0" | "1")
}

fn parse_i32(value: &str, name: &str) -> Result<i32> {
    value
        .parse()
        .with_context(|| format!("invalid {name} {value:?}"))
}

fn parse_optional_marker_i32(value: &str) -> Result<Option<i32>> {
    let value = strip_marker(value);
    if value.is_empty() {
        Ok(None)
    } else {
        value
            .parse()
            .map(Some)
            .context("invalid grouped window index")
    }
}

fn parse_automatic_rename(value: &str) -> Result<Option<bool>> {
    match value {
        ":" | "" => Ok(None),
        "1" | "on" => Ok(Some(true)),
        "0" | "off" => Ok(Some(false)),
        _ => bail!("invalid automatic-rename value {value:?}"),
    }
}

fn strip_marker(value: &str) -> &str {
    value.strip_prefix(':').unwrap_or(value)
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn unescape_legacy_path(value: &str) -> String {
    value.replace("\\ ", " ")
}

fn layout_size(layout: &str) -> (u32, u32) {
    let Some(size) = layout.split(',').nth(1) else {
        return (0, 0);
    };
    let Some((width, height)) = size.split_once('x') else {
        return (0, 0);
    };
    (
        width.parse().unwrap_or_default(),
        height.parse().unwrap_or_default(),
    )
}

fn session_id(name: &str) -> String {
    format!("legacy-session:{}", stable_id(&[name]))
}

fn window_id(session: &str, index: i32) -> String {
    format!(
        "legacy-window:{}",
        stable_id(&[session, &index.to_string()])
    )
}

fn pane_id(session: &str, window: i32, pane: i32) -> String {
    format!(
        "legacy-pane:{}",
        stable_id(&[session, &window.to_string(), &pane.to_string()])
    )
}

fn stable_id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex()[..20].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn import(input: &str) -> ImportResult {
        let directory = tempdir().unwrap();
        let path = directory.path().join("snapshot.txt");
        std::fs::write(&path, input).unwrap();
        import_resurrect(&path).unwrap()
    }

    #[test]
    fn imports_v4_empty_title_without_losing_the_cwd_field() {
        let input = concat!(
            "pane\twork\t0\t1\t:*Z\t0\t\t:/tmp\t1\tzsh\t:\n",
            "window\twork\t0\t:editor\t1\t:*Z\t1234,80x24,0,0,1\toff\n",
            "state\twork\t\n",
        );
        let result = import(input);
        let client_state = result.snapshot.state.client_state.as_ref().unwrap();
        assert_eq!(client_state.attachments.len(), 1);
        assert_eq!(client_state.attachments[0].session_id, session_id("work"));
        assert_eq!(client_state.attachments[0].last_session_id, None);
        let window = &result.snapshot.state.windows[0];
        assert_eq!(window.name, "editor");
        assert!(window.zoomed);
        assert_eq!(window.automatic_rename, Some(false));
        assert_eq!(window.panes[0].title.as_deref(), Some(""));
        assert_eq!(window.panes[0].import_status, Some(ImportStatus::Exact));
        assert_eq!(
            window.panes[0]
                .cwd
                .path
                .as_ref()
                .unwrap()
                .to_path_buf()
                .unwrap(),
            Path::new("/tmp")
        );
    }

    #[test]
    fn imports_current_and_last_client_sessions() {
        let input = concat!(
            "pane\tcurrent\t0\t1\t:*\t0\t:shell\t:/tmp\t1\tzsh\t:\n",
            "pane\tlast\t0\t1\t:*\t0\t:shell\t:/tmp\t1\tzsh\t:\n",
            "window\tcurrent\t0\t:current\t1\t:*\t1234,80x24,0,0,1\t:\n",
            "window\tlast\t0\t:last\t1\t:*\t1234,80x24,0,0,1\t:\n",
            "state\tcurrent\tlast\n",
        );
        let result = import(input);
        let attachment = &result
            .snapshot
            .state
            .client_state
            .as_ref()
            .unwrap()
            .attachments[0];
        assert_eq!(attachment.session_id, session_id("current"));
        assert_eq!(
            attachment.last_session_id.as_deref(),
            Some(session_id("last").as_str())
        );
    }

    #[test]
    fn repairs_v4_empty_title_field_shift_and_discards_process_text() {
        let input = concat!(
            "pane\twork\t0\t1\t:*Z\t0\t:/tmp\t1\tzsh\t12345\t:\n",
            "window\twork\t0\t:editor\t1\t:*Z\t1234,80x24,0,0,1\t:\n",
            "state\twork\t\n",
        );
        let result = import(input);
        let pane = &result.snapshot.state.windows[0].panes[0];
        assert_eq!(result.repaired_panes, 1);
        assert_eq!(pane.title.as_deref(), Some(""));
        assert_eq!(pane.current_command.as_deref(), Some("zsh"));
        assert_eq!(pane.start_command, None);
        assert_eq!(
            pane.import_status,
            Some(ImportStatus::RepairedEmptyTitleShift)
        );
        assert_eq!(
            result.snapshot.diagnostics[0].code,
            "repaired_empty_title_shift"
        );
    }

    #[test]
    fn imports_v3_window_name_and_untrusted_command_as_metadata() {
        let input = concat!(
            "pane\twork\t0\t:editor\t1\t:*\t0\t:/tmp\t1\tvim\t:vim file.txt\n",
            "window\twork\t0\t1\t:*\t1234,80x24,0,0,1\n",
            "state\twork\t\n",
        );
        let result = import(input);
        let window = &result.snapshot.state.windows[0];
        assert_eq!(window.name, "editor");
        assert_eq!(window.panes[0].title, None);
        assert_eq!(
            window.panes[0].start_command.as_deref(),
            Some("vim file.txt")
        );
        assert_eq!(window.panes[0].restart, None);
        assert_eq!(window.panes[0].import_status, Some(ImportStatus::Exact));
    }
}

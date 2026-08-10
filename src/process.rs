use std::collections::HashMap;

#[cfg(target_os = "linux")]
use std::{fs, path::PathBuf};

#[cfg(target_os = "linux")]
use crate::model::EncodedPath;
use crate::model::{RestartSpec, TmuxState};

pub fn populate_restart_specs(state: &mut TmuxState) {
    let restart_specs = collect_restart_specs(
        state
            .windows
            .iter()
            .flat_map(|window| &window.panes)
            .filter_map(|pane| pane.pid),
    );
    for pane in state
        .windows
        .iter_mut()
        .flat_map(|window| &mut window.panes)
    {
        pane.restart = pane.pid.and_then(|pid| restart_specs.get(&pid).cloned());
    }
}

#[cfg(target_os = "linux")]
pub fn collect_restart_specs(pane_pids: impl Iterator<Item = u32>) -> HashMap<u32, RestartSpec> {
    let pane_pids: Vec<u32> = pane_pids.collect();
    let mut result = HashMap::new();
    for pane_pid in pane_pids {
        let Some(stat) = read_stat(pane_pid) else {
            continue;
        };
        let foreground_group = stat.tpgid;
        if foreground_group <= 0 {
            continue;
        }
        let foreground_pid = foreground_group as u32;
        let Some(argv) = read_cmdline(foreground_pid) else {
            continue;
        };
        let executable = fs::read_link(format!("/proc/{foreground_pid}/exe"))
            .unwrap_or_else(|_| PathBuf::from(&argv[0]));
        result.insert(
            pane_pid,
            RestartSpec {
                executable: EncodedPath::from_path(&executable),
                argv,
                trusted: true,
            },
        );
    }
    result
}

#[cfg(not(target_os = "linux"))]
pub fn collect_restart_specs(_pane_pids: impl Iterator<Item = u32>) -> HashMap<u32, RestartSpec> {
    // Process metadata is optional; structural capture must still work on every target.
    HashMap::new()
}

#[cfg(target_os = "linux")]
struct ProcStat {
    tpgid: i32,
}

#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<ProcStat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat)
}

#[cfg(target_os = "linux")]
fn parse_stat(stat: &str) -> Option<ProcStat> {
    let close = stat.rfind(')')?;
    Some(ProcStat {
        tpgid: stat
            .get(close + 2..)?
            .split_whitespace()
            .nth(5)?
            .parse()
            .ok()?,
    })
}

#[cfg(target_os = "linux")]
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn parses_tpgid_without_being_confused_by_parentheses_in_comm() {
        let stat = "123 (name with ) parenthesis) S 1 2 3 4 456 7 8";
        assert_eq!(parse_stat(stat).unwrap().tpgid, 456);
    }
}

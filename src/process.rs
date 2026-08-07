use std::{collections::HashMap, fs, path::PathBuf};

use crate::model::{EncodedPath, RestartSpec};

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
    // Native process metadata is optional. The macOS backend is filled by libproc when available;
    // a missing backend must never make the structural snapshot fail.
    HashMap::new()
}

#[cfg(target_os = "linux")]
struct ProcStat {
    tpgid: i32,
}

#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<ProcStat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(close + 2..)?.split_whitespace().collect();
    Some(ProcStat {
        tpgid: fields.get(5)?.parse().ok()?,
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

use std::process::Command;

#[test]
fn save_help_explains_snapshot_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .args(["save", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let help = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "--socket <SOCKET>",
        "Use this tmux socket instead of $TMUX or the default socket",
        "--label <LABEL>",
        "Attach a label; labeled saves are recorded even when unchanged",
        "--pin",
        "Keep the saved snapshot from retention pruning",
        "--if-empty",
        "Save only when this socket has no snapshot history",
    ] {
        assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
    }
}

#[test]
fn daemon_help_explains_lifecycle_controls() {
    let output = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .args(["daemon", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let help = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "--status",
        "Report the running daemon version and process identity",
        "--stop",
        "Ask the running daemon to exit cleanly",
        "--reload",
        "Re-exec the running daemon from the installed binary",
        "--json",
        "Print daemon status as JSON",
    ] {
        assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
    }
}

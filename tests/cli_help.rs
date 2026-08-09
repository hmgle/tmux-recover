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
    ] {
        assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
    }
}

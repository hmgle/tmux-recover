//! CLI-level cover for `import-resurrect`, whose commit semantics live in
//! main.rs and so are not reachable from a library test.

use std::process::Command;

const RESURRECT_V3: &str = "window\twork\t0\t1\t*\tb25d,80x24,0,0,0\n\
                            pane\twork\t0\twin\t:\t*\t0\t/tmp\t1\tzsh\t:zsh\n";

fn import(data: &std::path::Path, file: &std::path::Path, extra: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .arg("--data-dir")
        .arg(data)
        .arg("import-resurrect")
        .arg(file)
        .args(extra)
        .output()
        .unwrap();
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn imported_ids(data: &std::path::Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(data.join("imports").join("snapshots"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".json")
                .to_owned()
        })
        .collect();
    found.sort();
    found
}

/// Two resurrect files can describe the same layout and still be different
/// history: different source paths, digests, and labels, none of which the
/// structural hash covers. Deduping the second one reported an id that was
/// never written.
#[test]
fn importing_two_files_with_the_same_structure_records_both() {
    let directory = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let first_file = directory.path().join("first.txt");
    let second_file = directory.path().join("second.txt");
    std::fs::write(&first_file, RESURRECT_V3).unwrap();
    std::fs::write(&second_file, RESURRECT_V3).unwrap();

    let (ok, first_output) = import(data.path(), &first_file, &[]);
    assert!(ok, "{first_output}");
    let first_id = first_output
        .lines()
        .next()
        .unwrap()
        .strip_prefix("imported ")
        .unwrap()
        .to_owned();

    // `--pin` used to fail here with "not found", because the id printed above
    // was never written.
    let (ok, second_output) = import(data.path(), &second_file, &["--pin"]);
    assert!(ok, "{second_output}");
    assert!(!second_output.contains("was not found"), "{second_output}");
    let second_id = second_output
        .lines()
        .next()
        .unwrap()
        .strip_prefix("imported ")
        .unwrap()
        .to_owned();
    assert_ne!(first_id, second_id);

    // Both reported ids must actually exist, and the labels must record which
    // file each came from.
    assert_eq!(imported_ids(data.path()), {
        let mut expected = vec![first_id.clone(), second_id.clone()];
        expected.sort();
        expected
    });
    let pins: Vec<String> = std::fs::read_dir(data.path().join("imports").join("pins"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(pins, vec![second_id.clone()]);

    for (id, expected_label) in [(&first_id, "first.txt"), (&second_id, "second.txt")] {
        let bytes = std::fs::read(
            data.path()
                .join("imports")
                .join("snapshots")
                .join(format!("{id}.json")),
        )
        .unwrap();
        let snapshot: tmux_recover::model::Snapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            snapshot.label.as_deref(),
            Some(format!("resurrect import: {expected_label}").as_str())
        );
    }
}

/// The id `--json` publishes has to be resolvable, since callers key on it.
#[test]
fn the_json_snapshot_id_is_resolvable() {
    let directory = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let file = directory.path().join("only.txt");
    std::fs::write(&file, RESURRECT_V3).unwrap();

    import(data.path(), &file, &[]);
    let (ok, output) = import(data.path(), &file, &["--json"]);
    assert!(ok, "{output}");
    let parsed: serde_json::Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
    let id = parsed["snapshot_id"].as_str().unwrap();

    let validate = Command::new(env!("CARGO_BIN_EXE_tmux-recover"))
        .arg("--data-dir")
        .arg(data.path())
        .args(["validate", id, "--imports"])
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "the id reported by --json does not resolve: {}",
        String::from_utf8_lossy(&validate.stderr)
    );
}

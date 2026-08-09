use std::{process::Command, time::Duration};

use tempfile::TempDir;
use tmux_recover::{
    config::{AutosaveConfig, Config, RestoreConfig},
    model::{Snapshot, SnapshotSource},
    restore::ProcessMetadataSource,
    storage::SnapshotStore,
    tmux::{capture::capture, control::ControlClient},
    util::socket_identity,
};

struct TestServer {
    directory: TempDir,
    socket: std::path::PathBuf,
}

impl TestServer {
    fn start() -> Option<Self> {
        Self::start_with_command(Some("sleep 60"))
    }

    /// A pane running a bare `sh`. Both callers baseline a snapshot count and
    /// then assert nothing was added, so the user's real interactive shell is
    /// unusable here: a framework like oh-my-zsh cd's into its plugin directory
    /// while sourcing, which is real structural change the daemon correctly
    /// commits. Under load that churn can pause long enough to look settled and
    /// then resume, so waiting it out is not enough -- it has to not happen.
    fn start_shell() -> Option<Self> {
        Self::start_with_command(None)
    }

    fn start_with_command(start_command: Option<&str>) -> Option<Self> {
        let version = Command::new("tmux").arg("-V").output().ok()?;
        if !version.status.success()
            || tmux_recover::util::require_tmux_37(&String::from_utf8_lossy(&version.stdout))
                .is_err()
        {
            return None;
        }
        let directory = tempfile::tempdir().ok()?;
        let socket = directory.path().join("tmux.sock");
        // `default-shell` has to be set at server start: naming the shell on
        // the `new-session` command line would record a `pane_start_command`,
        // which `target_is_bootstrap` requires to be absent.
        let config = directory.path().join("tmux.conf");
        std::fs::write(&config, "set -g default-shell /bin/sh\n").ok()?;
        let mut command = Command::new("tmux");
        command
            .args(["-S"])
            .arg(&socket)
            .arg("-f")
            .arg(&config)
            .args(["new-session", "-d", "-s", "daemon"]);
        if let Some(start_command) = start_command {
            command.arg(start_command);
        }
        let status = command.status().ok()?;
        status.success().then_some(Self { directory, socket })
    }

    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(["-S"]).arg(&self.socket);
        command
    }

    fn stop(&self) {
        let _ = self.tmux().arg("kill-server").status();
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn daemon_polls_when_an_existing_hook_slot_is_occupied() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(20),
            min_interval: Duration::from_millis(30),
            poll_interval: Duration::from_millis(50),
            ..AutosaveConfig::default()
        },
        ..Config::default()
    };
    let hook = format!("after-new-window[{}]", config.autosave.hook_slot);
    assert!(
        server
            .tmux()
            .args([
                "set-hook",
                "-g",
                &hook,
                "display-message external-tmux-recover:state-changed-hook",
            ])
            .status()
            .unwrap()
            .success()
    );

    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(5), || store.has_current()).await;
    let original = store.load_current().unwrap();
    let original_path = store
        .list()
        .unwrap()
        .into_iter()
        .find(|snapshot| snapshot.current)
        .unwrap()
        .path;

    // Nothing in tmux changed and no hook can report this filesystem damage.
    // Only a later poll can notice that dedup can no longer validate current
    // and repair it by publishing the fresh capture.
    std::fs::write(original_path, b"{broken").unwrap();
    wait_until(Duration::from_secs(5), || {
        store
            .load_current()
            .is_ok_and(|snapshot| snapshot.id != original.id)
    })
    .await;
    assert!(!task.is_finished(), "daemon exited instead of polling");

    let output = server
        .tmux()
        .args(["show-hooks", "-g", &hook])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("external-tmux-recover:state-changed-hook")
    );
    let unused_hook = format!("after-new-session[{}]", config.autosave.hook_slot);
    let output = server
        .tmux()
        .args(["show-hooks", "-g", &unused_hook])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("wait-for -S tmux-recover:state-changed"),
        "hook preflight installed a partial event hook set"
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn daemon_reports_legacy_hooks_without_removing_them() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config::default();
    let hook = format!("after-new-window[{}]", config.autosave.hook_slot);
    let legacy = "display-message -c /dev/pts/legacy tmux-recover:state-changed";
    assert!(
        server
            .tmux()
            .args(["set-hook", "-g", &hook, legacy])
            .status()
            .unwrap()
            .success()
    );

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tmux_recover::daemon::run(&server.socket, data.path(), &config),
    )
    .await
    .expect("daemon hung while reporting the legacy hook")
    .unwrap_err();
    let error = format!("{result:#}");
    assert!(error.contains("legacy tmux-recover hooks"), "{error}");
    assert!(error.contains(&hook), "{error}");

    let output = server
        .tmux()
        .args(["show-hooks", "-g", &hook])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(legacy));
}

#[tokio::test]
async fn polling_saves_cwd_changes_without_structure_hooks() {
    let Some(server) = TestServer::start_shell() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(50),
            poll_interval: Duration::from_millis(80),
            ..AutosaveConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(15), || store.has_current()).await;
    let cwd = server.directory.path().join("poll cwd");
    std::fs::create_dir(&cwd).unwrap();
    let shell_command = format!("cd -- '{}'", cwd.display());
    // Keys sent before the shell reaches its first prompt are dropped, and
    // under load that prompt can be slow. Re-send until tmux reports the new
    // cwd, then let the poll notice it.
    let started = tokio::time::Instant::now();
    while !pane_cwd(&server, "daemon:0.0").is_some_and(|path| path == cwd) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "shell never changed directory"
        );
        assert!(
            server
                .tmux()
                .args(["send-keys", "-t", "daemon:0.0", "-l"])
                .arg(&shell_command)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            server
                .tmux()
                .args(["send-keys", "-t", "daemon:0.0", "Enter"])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    wait_until(Duration::from_secs(5), || {
        store.load_current().is_ok_and(|snapshot| {
            snapshot.state.windows[0].panes[0]
                .cwd
                .path
                .as_ref()
                .is_some_and(|path| path.to_path_buf().is_ok_and(|path| path == cwd))
        })
    })
    .await;
    assert!(store.list().unwrap().len() >= 2);

    task.abort();
    let _ = task.await;
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tokio::test]
async fn auto_restore_only_replaces_a_young_shell_bootstrap() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(80),
            poll_interval: Duration::from_secs(5),
            ..AutosaveConfig::default()
        },
        restore: RestoreConfig {
            auto: true,
            // Two distinct races made this flake under load, and both needed
            // fixing: the pane not having exec'd its shell by the time the
            // daemon captured (see the wait below), and this window elapsing
            // during the test's own setup, after which the daemon correctly
            // declines. The gate itself is covered by `server_is_young`'s unit
            // test, so widening it here loses no coverage.
            auto_bootstrap_max_age_seconds: 600,
            ..RestoreConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let captured = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    let source = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )
    .unwrap();
    store.commit(&source, true).unwrap();

    server.stop();
    // Auto-restore is a deliberate one-shot at daemon startup, and its gate
    // requires the pane's foreground command to equal the default shell. The
    // developer's interactive shell breaks that in two ways: `new-session -d`
    // returns before the exec completes, and a prompt framework like oh-my-zsh
    // runs `git` on every render, so the pane flickers off the shell name --
    // and the daemon then correctly declines. Pin the pane to a bare `sh` with
    // no rc file so its foreground command is stable, then wait for it.
    // `default-shell` has to be set at server start, not after: naming the
    // shell on the `new-session` command line would record a
    // `pane_start_command`, and `target_is_bootstrap` requires none.
    let conf = server.directory.path().join("bootstrap.conf");
    std::fs::write(&conf, "set -g default-shell /bin/sh\n").unwrap();
    assert!(
        server
            .tmux()
            .arg("-f")
            .arg(&conf)
            .args(["new-session", "-d", "-s", "bootstrap"])
            .status()
            .unwrap()
            .success()
    );
    wait_for_default_shell_pane(&server, "bootstrap:0.0").await;
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    // An auto-restore has to start a daemon, capture, preflight, and rebuild
    // the session before this holds, so it gets a larger ceiling than the
    // single-step waits elsewhere.
    wait_until(Duration::from_secs(45), || {
        let output = server
            .tmux()
            .args(["list-sessions", "-F", "#{session_name}"])
            .output();
        output.is_ok_and(|output| output.status.success() && output.stdout == b"daemon\n")
    })
    .await;
    assert_eq!(
        store.load_current().unwrap().state.sessions[0].name,
        "daemon"
    );
    assert_eq!(store.load(&source.id).unwrap().id, source.id);

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn hook_event_saves_changed_state() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(80),
            poll_interval: Duration::from_secs(5),
            ..AutosaveConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(15), || store.has_current()).await;
    let initial = store.load_current().unwrap();
    assert_eq!(initial.state.windows[0].panes.len(), 1);

    let output = server
        .tmux()
        .args(["split-window", "-d", "-t", "daemon:0"])
        .arg("sleep 60")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(Duration::from_secs(15), || {
        store
            .load_current()
            .is_ok_and(|snapshot| snapshot.state.windows[0].panes.len() == 2)
    })
    .await;
    assert_eq!(store.list().unwrap().len(), 2);

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn auto_restore_preflight_failure_does_not_kill_the_daemon() {
    let Some(server) = TestServer::start() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(80),
            poll_interval: Duration::from_secs(5),
            ..AutosaveConfig::default()
        },
        restore: RestoreConfig {
            auto: true,
            ..RestoreConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);
    let mut captured = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    // Point the pane at a cwd that no longer exists so preflight fails.
    let missing = server.directory.path().join("gone-before-restore");
    captured.state.windows[0].panes[0].cwd = tmux_recover::model::PaneCwd::inspect(Some(
        tmux_recover::model::EncodedPath::from(missing.as_os_str()),
    ));
    let source = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: "test".to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )
    .unwrap();
    store.commit(&source, true).unwrap();

    server.stop();
    // This test needs auto-restore to be *attempted* so preflight can fail. A
    // bootstrap running the developer's interactive shell can be rejected by
    // the gate before that (see `auto_restore_only_replaces_a_young_shell_bootstrap`),
    // which would make this pass without exercising anything.
    let conf = server.directory.path().join("bootstrap.conf");
    std::fs::write(&conf, "set -g default-shell /bin/sh\n").unwrap();
    assert!(
        server
            .tmux()
            .arg("-f")
            .arg(&conf)
            .args(["new-session", "-d", "-s", "bootstrap"])
            .status()
            .unwrap()
            .success()
    );
    wait_for_default_shell_pane(&server, "bootstrap:0.0").await;

    // Pin both preconditions, since a gate rejection and a preflight failure
    // leave the same observable end state and would be indistinguishable
    // below: the daemon must get past the gate, and preflight must then fail.
    {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        let target = capture(&mut client, &server.socket).await.unwrap();
        assert!(
            tmux_recover::restore::target_is_auto_bootstrap(&target),
            "auto-restore would be declined before preflight, making this test vacuous"
        );
        let options = tmux_recover::restore::restore_config_options(
            &config.restore,
            false,
            false,
            None,
            false,
            None,
        );
        assert!(
            tmux_recover::restore::preflight(&source, &target, &options).is_err(),
            "the snapshot under test must fail preflight"
        );
    }

    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    // The failed auto-restore must not tear the daemon down: the bootstrap
    // session stays put, and the daemon keeps watching and autosaving it.
    wait_until(Duration::from_secs(15), || {
        store.load_current().is_ok_and(|snapshot| {
            snapshot
                .state
                .sessions
                .first()
                .is_some_and(|session| session.name == "bootstrap")
        })
    })
    .await;
    assert!(
        !task.is_finished(),
        "daemon exited after a failed auto-restore"
    );

    task.abort();
    let _ = task.await;
}

/// End-to-end cover for the process checkpoint sidecar against a real tmux
/// server: a process-only change must reach the sidecar without adding a
/// snapshot, a `current` restore must use it, an explicit snapshot id must
/// not, the program must actually come back, and the daemon must keep writing
/// eligible checkpoints once it is watching the restored server.
///
/// Does not cover a sidecar pane with `restart: null`, which needs `tpgid <= 0`
/// or a failing `/proc` read to arise for real; that suppression rule is
/// covered by `restore`'s unit tests instead.
///
/// Nor does step 7 reproduce a structural-hash collision across the generation
/// boundary, which is what makes `commit` compare `Origin`. A first restore
/// allocates fresh ids because the bootstrap consumes the low ones, so the
/// structure differs and `commit` writes regardless; the collision needs a
/// second cycle whose restart specs also line up, which conflicts with this
/// test restoring a live process. Verified by hand against a live server, and
/// guarded by `same_structural_state_from_a_new_server_generation_is_written`.
///
/// Linux-only because restart metadata is collected from `/proc`; on other
/// targets `collect_restart_specs` returns nothing and there is no sidecar
/// content to assert on.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn sidecar_tracks_a_live_process_change_and_restores_it() {
    let Some(server) = TestServer::start_shell() else {
        eprintln!("tmux 3.7+ is unavailable; skipping integration test");
        return;
    };
    let data = tempfile::tempdir().unwrap();
    let config = Config {
        autosave: AutosaveConfig {
            debounce: Duration::from_millis(30),
            min_interval: Duration::from_millis(50),
            poll_interval: Duration::from_millis(100),
            process_checkpoint_interval: Duration::from_millis(1),
            hook_slot: AutosaveConfig::default().hook_slot,
        },
        restore: RestoreConfig {
            process_allowlist: vec!["sleep".to_owned()],
            ..RestoreConfig::default()
        },
        ..Config::default()
    };
    let identity = socket_identity(&server.socket).unwrap();
    let store = SnapshotStore::for_socket(data.path(), &identity.key, &config.storage);

    // Without this, running a command renames the window, which changes the
    // structural hash and produces a snapshot -- the very churn the sidecar
    // exists to avoid, and it would mask what this test is checking.
    assert!(
        server
            .tmux()
            .args([
                "set-window-option",
                "-t",
                "daemon:0",
                "automatic-rename",
                "off"
            ])
            .status()
            .unwrap()
            .success()
    );

    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    // 1. The daemon publishes a current snapshot and a sidecar beside it.
    wait_until(Duration::from_secs(5), || {
        store.has_current() && store.read_process_checkpoint().unwrap().is_some()
    })
    .await;
    // An interactive shell's own startup is a source of real structural
    // change: a framework like oh-my-zsh cd's into its plugin directory while
    // sourcing, and the daemon correctly snapshots that. Wait for the count to
    // stop moving before baselining, or step 3 blames those commits on the
    // process change.
    let snapshots_before = wait_for_stable_snapshot_count(&store).await;
    let first = store.read_process_checkpoint().unwrap().unwrap();
    assert_eq!(
        first.base_snapshot_id,
        store.current_snapshot_id().unwrap().unwrap(),
        "the sidecar must be pinned to the current snapshot"
    );

    // 2. A process-only change: no new pane, no layout change, same cwd.
    //    Keystrokes sent before the shell reaches its first prompt are
    //    dropped, and there is no event that reliably marks "prompt ready",
    //    so re-send until tmux itself reports the new foreground process.
    let started = tokio::time::Instant::now();
    while pane_command(&server, "daemon:0.0") != "sleep" {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "shell never ran the command"
        );
        assert!(
            server
                .tmux()
                .args(["send-keys", "-t", "daemon:0.0", "sleep 300", "Enter"])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // 3. It reaches the sidecar, and no snapshot is added for it. Waiting on
    //    the recorded argv rather than on "the hash moved" keeps unrelated
    //    startup churn from satisfying this early.
    wait_until(Duration::from_secs(5), || {
        store
            .read_process_checkpoint()
            .unwrap()
            .is_some_and(|checkpoint| {
                checkpoint.panes.iter().any(|pane| {
                    pane.restart
                        .as_ref()
                        .is_some_and(|restart| restart.argv == ["sleep", "300"])
                })
            })
    })
    .await;
    let updated = store.read_process_checkpoint().unwrap().unwrap();
    assert_eq!(
        store.list().unwrap().len(),
        snapshots_before,
        "a process-only change must not add a snapshot"
    );
    assert_eq!(updated.base_snapshot_id, first.base_snapshot_id);
    assert_ne!(updated.process_hash, first.process_hash);

    task.abort();
    let _ = task.await;

    // Restore into a fresh bootstrap server on the same socket.
    let snapshot = store.load_current().unwrap();
    server.stop();
    // tmux reports `start_time` in whole seconds, and a test restarts the
    // server far faster than a real crash-and-recover would. Step 7 below is
    // about behaviour across a generation boundary, so make the generations
    // actually distinguishable.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(
        server
            .tmux()
            .args(["-f", "/dev/null", "new-session", "-d", "-s", "bootstrap"])
            .status()
            .unwrap()
            .success()
    );
    let mut client = ControlClient::connect(&server.socket).await.unwrap();
    let target = capture(&mut client, &server.socket).await.unwrap();

    // 4. Restoring `current` uses the sidecar and finds the sleep.
    assert!(tmux_recover::restore::process_checkpoint_is_offered(
        "current", false, true
    ));
    let checkpoint = store.read_process_checkpoint().unwrap();
    let options = tmux_recover::restore::restore_config_options(
        &config.restore,
        false,
        false,
        None,
        true,
        checkpoint.as_ref(),
    );
    let plan = tmux_recover::restore::preflight(&snapshot, &target, &options).unwrap();
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    assert_eq!(
        plan.process_metadata_source,
        ProcessMetadataSource::Checkpoint
    );
    assert_eq!(plan.process_restarts, 1);
    assert_eq!(
        plan.process_checkpoint_captured_at,
        Some(updated.captured_at)
    );

    // 5. Naming the same snapshot by id must not consult the sidecar, so it
    //    falls back to metadata that only recorded the idle shell.
    assert!(!tmux_recover::restore::process_checkpoint_is_offered(
        &snapshot.id,
        false,
        true
    ));
    let snapshot_only = tmux_recover::restore::restore_config_options(
        &config.restore,
        false,
        false,
        None,
        true,
        None,
    );
    let snapshot_plan =
        tmux_recover::restore::preflight(&snapshot, &target, &snapshot_only).unwrap();
    assert_eq!(
        snapshot_plan.process_metadata_source,
        ProcessMetadataSource::Snapshot
    );
    assert_eq!(snapshot_plan.process_restarts, 0);

    // 6. The program actually comes back in the restored pane.
    let report = tmux_recover::restore::apply(&mut client, &snapshot, &target, &plan).await;
    assert_eq!(
        report.status,
        tmux_recover::model::RestoreStatus::Succeeded,
        "{report:#?}"
    );
    drop(client);
    let pane_pid: u32 = String::from_utf8(
        server
            .tmux()
            .args(["display-message", "-p", "-t", "daemon:0.0", "#{pane_pid}"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse()
    .unwrap();
    wait_until(Duration::from_secs(10), || {
        child_cmdlines(pane_pid).iter().any(|cmdline| {
            cmdline.first().is_some_and(|arg| arg.ends_with("sleep"))
                && cmdline.get(1).is_some_and(|arg| arg == "300")
        })
    })
    .await;
    let restored_children = child_cmdlines(pane_pid);
    assert!(
        restored_children.iter().any(|cmdline| cmdline
            .first()
            .is_some_and(|arg| arg.ends_with("sleep"))
            && cmdline.get(1).is_some_and(|arg| arg == "300")),
        "restored pane {pane_pid} is not running sleep 300: {restored_children:?}"
    );

    // 7. Keep going on the restored server, which is the case that broke: a
    //    restore reproduces tmux ids deterministically, so this new generation
    //    can present the exact structure of the snapshot it came from. If the
    //    daemon deduped that away, `current` would stay pinned to the old
    //    generation and every checkpoint written from here on would be
    //    rejected as coming from a different generation.
    let restored_generation = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket)
            .await
            .unwrap()
            .origin
            .server_started_at
    };
    assert_ne!(
        restored_generation, first.origin.server_started_at,
        "this step is only meaningful across a generation boundary"
    );

    let daemon_socket = server.socket.clone();
    let daemon_data = data.path().to_path_buf();
    let daemon_config = config.clone();
    let task = tokio::spawn(async move {
        tmux_recover::daemon::run(&daemon_socket, &daemon_data, &daemon_config).await
    });

    wait_until(Duration::from_secs(15), || {
        store
            .load_current()
            .is_ok_and(|snapshot| snapshot.origin.server_started_at == restored_generation)
    })
    .await;
    let rebased = store.load_current().unwrap();
    let snapshots_after_rebase = wait_for_stable_snapshot_count(&store).await;

    // A further process-only change must produce a checkpoint that is still
    // eligible against the rebased snapshot.
    // Interrupt the restored program the way a user would. This is the case
    // the old `<cmd>; exec <shell>` wrapper got wrong: C-c killed the wrapper
    // with the program and the pane died, taking this single-pane session and
    // the whole server with it.
    assert!(
        server
            .tmux()
            .args(["send-keys", "-t", "daemon:0.0", "C-c"])
            .status()
            .unwrap()
            .success()
    );
    wait_until(Duration::from_secs(10), || {
        !child_cmdlines(pane_pid)
            .iter()
            .any(|cmdline| cmdline.first().is_some_and(|arg| arg.ends_with("sleep")))
    })
    .await;
    assert!(
        server
            .tmux()
            .args(["has-session", "-t", "daemon"])
            .status()
            .unwrap()
            .success(),
        "C-c on a restored program killed the pane and the session with it"
    );
    assert!(
        !child_cmdlines(pane_pid)
            .iter()
            .any(|cmdline| cmdline.first().is_some_and(|arg| arg.ends_with("sleep"))),
        "C-c did not reach the restored program: {:?}",
        child_cmdlines(pane_pid)
    );
    wait_for_default_shell_pane(&server, "daemon:0.0").await;

    // The wrapper ignored SIGINT while waiting for the restored process. That
    // disposition survives exec, so verify it was reset before entering tmux's
    // configured default-shell: a second command must still respond to C-c.
    // This also proves the fallback is /bin/sh from the target server rather
    // than this test runner's SHELL (normally zsh).
    let started = tokio::time::Instant::now();
    while pane_command(&server, "daemon:0.0") != "sleep" {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "fallback shell never ran the second sleep"
        );
        assert!(
            server
                .tmux()
                .args(["send-keys", "-t", "daemon:0.0", "sleep 300", "Enter"])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        server
            .tmux()
            .args(["send-keys", "-t", "daemon:0.0", "C-c"])
            .status()
            .unwrap()
            .success()
    );
    wait_for_default_shell_pane(&server, "daemon:0.0").await;
    assert!(
        !child_cmdlines(pane_pid)
            .iter()
            .any(|cmdline| cmdline.first().is_some_and(|arg| arg.ends_with("sleep"))),
        "the fallback shell passed ignored SIGINT to its child: {:?}",
        child_cmdlines(pane_pid)
    );

    let started = tokio::time::Instant::now();
    while pane_command(&server, "daemon:0.0") != "cat" {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "shell never ran the second command"
        );
        assert!(
            server
                .tmux()
                .args(["send-keys", "-t", "daemon:0.0", "cat", "Enter"])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    wait_until(Duration::from_secs(10), || {
        store
            .read_process_checkpoint()
            .unwrap()
            .is_some_and(|checkpoint| {
                checkpoint.base_snapshot_id == rebased.id
                    && checkpoint
                        .panes
                        .iter()
                        .any(|pane| pane.current_command.as_deref() == Some("cat"))
            })
    })
    .await;
    let after = store.read_process_checkpoint().unwrap().unwrap();
    assert_eq!(
        store.list().unwrap().len(),
        snapshots_after_rebase,
        "a process-only change must not add a snapshot after a restore either"
    );
    assert_eq!(after.origin.server_started_at, restored_generation);

    let target = {
        let mut client = ControlClient::connect(&server.socket).await.unwrap();
        capture(&mut client, &server.socket).await.unwrap()
    };
    // `replace` because the target is the restored session now, not a
    // bootstrap; this preflight is only here to read back the plan's verdict on
    // the checkpoint.
    let options = tmux_recover::restore::restore_config_options(
        &config.restore,
        true,
        false,
        None,
        true,
        Some(&after),
    );
    let plan = tmux_recover::restore::preflight(&rebased, &target, &options).unwrap();
    assert!(
        plan.warnings.is_empty(),
        "checkpoint rejected after a restore: {:?}",
        plan.warnings
    );
    assert_eq!(
        plan.process_metadata_source,
        ProcessMetadataSource::Checkpoint
    );

    task.abort();
    let _ = task.await;
}

/// Waits until `pane` reports the server's `default-shell` as its foreground
/// command, which is the precondition `target_is_auto_bootstrap` checks.
async fn wait_for_default_shell_pane(server: &TestServer, pane: &str) {
    let output = server
        .tmux()
        .args(["show-options", "-gv", "default-shell"])
        .output()
        .unwrap();
    let default_shell = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let expected = std::path::Path::new(&default_shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    assert!(!expected.is_empty(), "tmux reported no default-shell");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let current = server
            .tmux()
            .args([
                "display-message",
                "-p",
                "-t",
                pane,
                "#{pane_current_command}",
            ])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        if current.as_deref() == Some(expected.as_str()) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pane {pane} never reached the default shell {expected:?} (saw {current:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn pane_cwd(server: &TestServer, pane: &str) -> Option<std::path::PathBuf> {
    let output = server
        .tmux()
        .args(["display-message", "-p", "-t", pane, "#{pane_current_path}"])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

/// Blocks until the snapshot count holds steady, then returns it. Used to
/// separate the shell's own startup churn from the change under test.
#[cfg(target_os = "linux")]
async fn wait_for_stable_snapshot_count(store: &SnapshotStore) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = store.list().unwrap().len();
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let now = store.list().unwrap().len();
        if now == last {
            return now;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "snapshot count never settled (last {last}, now {now})"
        );
        last = now;
    }
}

#[cfg(target_os = "linux")]
fn pane_command(server: &TestServer, pane: &str) -> String {
    let output = server
        .tmux()
        .args([
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_current_command}",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// argv of every direct child of `parent`, read from `/proc`. The restore wraps
/// a restarted program in a shell that survives C-c and then exec's the user's
/// shell, so the program is a child of the pane process rather than the pane
/// process itself. The wrapper's subshell exec's the program, so it stays a
/// direct child and does not need a recursive walk.
#[cfg(target_os = "linux")]
fn child_cmdlines(parent: u32) -> Vec<Vec<String>> {
    child_cmdlines_with_pids(parent)
        .into_iter()
        .map(|(_, cmdline)| cmdline)
        .collect()
}

#[cfg(target_os = "linux")]
fn child_cmdlines_with_pids(parent: u32) -> Vec<(u32, Vec<String>)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // The comm field can contain spaces and parentheses, so parse after
        // the final ')' rather than splitting the whole line.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        if fields.get(1).and_then(|ppid| ppid.parse::<u32>().ok()) != Some(parent) {
            continue;
        }
        let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        found.push((
            pid,
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect(),
        ));
    }
    found
}

/// Polls `condition` until it holds. The timeout is a generous ceiling for a
/// real hang, not a performance assertion: several tmux servers and daemons
/// share this binary's runner, so a tight bound only produces flakes. Callers
/// waiting on an auto-restore use a larger one still, since that path starts a
/// daemon, captures, preflights, and rebuilds a session before it can hold.
async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

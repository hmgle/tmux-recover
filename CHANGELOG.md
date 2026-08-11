# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Capture ordinary terminal clients' current/last sessions, import resurrect
  `state` rows, and record explicit client transitions and per-session
  visibility in restore reports.
- Install zsh completion for commands, options, paths, and snapshot selectors
  with release archives and `scripts/install.sh`.
- Add per-socket daemon status, clean stop, and in-place reload controls over a
  private local Unix socket. Reload preserves the PID and supervisor ownership
  while executing the installed binary and re-reading configuration.
- Allow `scripts/install.sh --reload-daemon --socket SOCKET` to reload one
  explicit watcher after an atomic binary replacement.

### Changed

- Reject foreground restores that would destroy their calling pane before the
  durable report is written.
- Defer Linux `/proc` process metadata collection until a structural save or a
  due process checkpoint needs it. An empty `restore.process_allowlist` now
  disables process capture, removes the live sidecar from every mutating entry
  path, and leaves structural snapshots unaffected.
- Keep daemon control commands independent of configuration parsing so a bad
  configuration cannot prevent status or a clean stop.
- Retry a daemon control accept that fails on descriptor or memory exhaustion
  instead of ending the watcher, retry an interrupted one immediately, and
  rebuild a failed control endpoint without cancelling startup or autosave.
  Preserve an accepted stop or reload across that endpoint rebuild, including
  when its request task is cancelled while writing the acknowledgement.
- Report a clean stop when the daemon closes an in-flight control request while
  exiting, whether it sent nothing or only part of a response, and keep stop and
  reload waiting while the daemon is still finishing the startup transaction
  that both are applied after. Lifecycle polls use their own deadline instead of
  ending early at the ordinary one-shot request timeout.

### Fixed

- Keep a completed restore live when an ordinary terminal detaches during
  client switching, and report the vanished client as a warning instead of a
  failed or incomplete rollback.

## [0.2.0] - 2026-08-10

### Added

- Download and verify the latest release by default in `scripts/install.sh`,
  with `--local` for atomic upgrades built from a maintainer checkout.
- Explain CLI options directly in command help and document migration from
  tmux-resurrect in English and Chinese.

### Fixed

- Ignore unflagged tmux hook output blocks so persistent hooks cannot
  desynchronize later control-mode commands.
- Show restore-key failures in the tmux status line instead of only reporting
  a generic `run-shell` error.
- Synchronously establish the first TPM snapshot so stopping tmux immediately
  after initial plugin activation cannot outrun the background watcher.
- Retry the automatic-restore shell check briefly when prompt helpers obscure
  an otherwise empty bootstrap pane during server startup.
- Preserve the previous-generation `current` snapshot when automatic restore
  cannot resolve a still-empty bootstrap server.

## [0.1.0] - 2026-08-09

### Added

- Continuous, per-socket tmux snapshots with event-driven saves and polling.
- Transactional restore with dry-run preflight, rollback, safety snapshots,
  retention, pins, and durable reports.
- Session, linked/grouped window, pane, cwd, title, layout, selection, and zoom
  capture on Linux and macOS.
- Opt-in trusted process restart metadata on Linux.
- tmux-resurrect v3/v4 import without executing imported command text.
- TPM and systemd integration, JSON output, zstd storage, and isolated tmux
  integration tests.

[Unreleased]: https://github.com/hmgle/tmux-recover/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/hmgle/tmux-recover/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/hmgle/tmux-recover/releases/tag/v0.1.0

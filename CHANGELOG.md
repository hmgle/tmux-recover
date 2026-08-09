# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/hmgle/tmux-recover/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/hmgle/tmux-recover/releases/tag/v0.1.0

## Summary

Describe the user-visible change and why it is needed.

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] `cargo test --locked --all-targets`
- [ ] tmux-backed testing used a dedicated `tmux -S` socket

## Compatibility and safety

Note any snapshot-schema, restore, process-command, platform, or tmux-version impact.

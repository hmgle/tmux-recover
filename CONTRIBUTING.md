# Contributing

Bug reports and focused pull requests are welcome. For behavior changes, open
an issue first when the expected recovery or compatibility semantics are not
obvious.

## Development setup

Use Rust 1.85 or newer and tmux 3.7 or newer. Then run:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

The repository also supports an explicit minimum-version check:

```sh
rustup toolchain install 1.85.0
cargo +1.85.0 check --locked --all-targets
```

## Tmux test safety

Never run mutating tests against the default socket or the server named by
`$TMUX`. Create a disposable socket in a temporary directory and pass it to
every command with `tmux -S`. Clean up only that exact disposable server. The
integration-test `TestServer` helpers demonstrate the expected pattern.

Add unit tests beside private implementation details and put CLI or
cross-module behavior in `tests/`. Restore changes should cover preflight,
rollback, cleanup warnings, and malformed input as appropriate.

## Pull requests

Keep commits small, imperative, and under 72 characters in the subject. Explain
the user-visible behavior, platform and tmux assumptions, compatibility impact,
and verification commands in the pull request description.

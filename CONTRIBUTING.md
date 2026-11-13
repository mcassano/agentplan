# Contributing to agentplan

Thanks for helping out. Bug reports, docs fixes, and code are all welcome.

## Before you start

For anything bigger than a small fix, please open an issue first so we can
agree on the approach. agentplan sits between people and the agents changing
their code, so we care more about correctness and predictable behavior than
about features.

## Development setup

You need a stable Rust toolchain (the minimum supported version is listed as
`rust-version` in `Cargo.toml`).

```sh
git clone https://github.com/mcassano/agentplan
cd agentplan
cargo build
cargo test
```

Platform notes:

- **macOS:** the tests use `sandbox-exec` and APFS clones. Both are available
  on stock macOS.
- **Linux:** the overlay backend needs unprivileged user namespaces. On
  Ubuntu 24.04 and later, AppArmor restricts them by default. For local
  testing you can run
  `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
  Tests that need namespaces skip themselves when namespaces are unavailable.

## Checks

CI runs the following. Please run them before opening a PR:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Guidelines

- Keep commits focused, and write messages that explain *why*.
- Add tests for behavior changes, especially anything touching `apply`,
  snapshots, or the network policy.
- Update `CHANGELOG.md` under "Unreleased" for user-visible changes.
- If a change affects what agentplan contains or does not contain, update
  `docs/security-model.md` too.

## License

By contributing, you agree that your contributions are licensed under the MIT
license that covers this project.

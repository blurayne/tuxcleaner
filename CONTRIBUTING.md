# Contributing

Contributions are welcome, especially new distribution adapters and cleanup rules backed by official distribution documentation.

## Requirements

- Rust 1.85 or newer
- Linux with `/proc` and `/etc/os-release`
- No destructive tests against the host filesystem

## Workflow

1. Add or update a distribution fixture when changing adapter detection.
2. Represent cleanup as a typed action. Do not construct shell strings.
3. Add an exact executor allowlist entry for every new external command.
4. Add tests that prove both the intended action and relevant refusal cases.
5. Update the safety documentation when a trust boundary changes.

Run the full local validation before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Public documentation and change descriptions should be written in English. Keep commits focused and do not add generated co-author trailers.


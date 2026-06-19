# Rayline

Rayline is an open-source local-router runtime for Claude Code workflows. It
provides the `rayline` CLI and `rld` daemon for local passthrough, selective
subagent routing, local model support, and update checks.

Rayline defaults to local operation. Local-router-only use does not require a
hosted Rayline account.

Built-in hosted Rayline auth and hosted cloud-router launch are intentionally
deferred in this release. They will be added after the public-client auth
boundary is redesigned and reviewed.

Rayline is an independent project from Atlas Futures, Inc. It is not affiliated
with, endorsed by, or sponsored by Anthropic PBC. "Claude", "Claude Code", and
"Anthropic" are trademarks of Anthropic PBC, used here nominatively to describe
interoperability.

## Build

```bash
cargo build --workspace --locked
cargo build --release -p rayline-cli -p rayline-daemon --locked
```

## Validate

```bash
cargo +1.85.0 fmt --all -- --check
cargo +1.85.0 test --workspace --locked -- --test-threads=1
cargo +1.85.0 clippy --workspace --all-targets --locked -- -D warnings
```

## Local Router

See [docs/rayline-local-router.md](docs/rayline-local-router.md).

Rayline can install a local certificate authority and proxy Claude Code traffic
through a local process when using proxy modes. Only enable TLS interception on
machines you control and only after reviewing the generated config and logs.
You are responsible for complying with the terms of the services and tools you
use with Rayline.

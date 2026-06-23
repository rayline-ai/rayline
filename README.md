# Rayline Local

Rayline Local is the open-source local router from Atlas Futures, Inc. It runs
on your machine and sits between your coding agent and the AI models it talks
to, deciding where each request should go. It provides the `rayline` CLI and
`rld` daemon for local passthrough, selective subagent routing, local model
support, and update checks.

This repository contains the Rayline Local router. Local-router-only use through
`rayline claude --local` does not require a hosted account and does not
connect to any remote hosted service.

## Demo

Claude Code running with hybrid cloud + on-device AI. The main agent runs Opus
in the cloud and orchestrates an `Explore` subagent that Rayline Local routes to
a model running fully on your machine (Qwen3.6-35B-A3B, Q4) — seamlessly, in a
single session.

<a href="https://get.rayline.ai/media/rayline-local-routing-demo.mp4">
  <img src="https://get.rayline.ai/media/rayline-local-routing-demo-4x.gif"
       alt="Rayline Local routing a Claude Code Explore subagent to an on-device model"
       width="100%">
</a>

## How It Works

Run `rayline claude` to start a Claude Code session with hosted Rayline routing
layered on top, or `rayline claude --local` for the auth-free local static
router. Your conversation works as it normally would, but Rayline Local can
route cheaper, high-volume work such as background subagent tasks to a fast
model running locally on your machine.

Routing is set by three orthogonal flags: `--local` picks the on-device router
(versus the hosted cloud router), `--via proxy|env` picks how Claude Code
connects, and `--route all|subagents` picks what flows through the router. Most
users only ever need `--local`. By default, your main conversation stays on
Claude, and only configured subagent traffic is routed to local or alternative
endpoints. See [docs/rayline-local-router.md](docs/rayline-local-router.md#routing-arguments)
for the full matrix. Run `rayline --help` to see the available commands and
configuration options.

Rayline Local operates on your machine and with your provider credentials. It is
not affiliated with any model provider.

## Supported Clients

- Claude Code, Anthropic's CLI coding agent.

More clients may be supported over time.

## Use Rayline From Code

You can also send your own Anthropic API traffic through Rayline from a script,
using the official Anthropic SDKs. Examples are grouped by routing path, with a
Python and TypeScript version of each:

- Cloud router (point the SDK at `https://api.rayline.ai` with a router key):
  [examples/cloud/python](examples/cloud/python) ·
  [examples/cloud/typescript](examples/cloud/typescript)
- Local routing (start the router with `rayline router start`, then route the SDK
  through the proxy on `127.0.0.1:20810` and request model `rayline-local` so the
  call lands on your on-device model):
  [examples/local/python](examples/local/python) ·
  [examples/local/typescript](examples/local/typescript)

## Install

Rayline Local release assets are published on
[GitHub Releases](https://github.com/rayline-ai/rayline/releases).

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/rayline-ai/rayline/main/scripts/install-rayline.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/rayline-ai/rayline/main/scripts/install-rayline.ps1 | iex
```

The installers place `rayline` and `rld` in `~/.rayline/bin` by default and
verify downloaded binaries against the release `SHA256SUMS` file.

## Build

```bash
cargo build --workspace --locked
cargo build --release -p rayline-cli -p rayline-daemon --locked
```

## Validate

```bash
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 test --workspace --locked -- --test-threads=1
cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
```

## Local Router

See [docs/rayline-local-router.md](docs/rayline-local-router.md).

## Disclaimers

### Non-Affiliation

Rayline Local is an independent, open-source project from Atlas Futures, Inc. It
is not affiliated with, endorsed by, or sponsored by Anthropic PBC. "Claude",
"Claude Code", and "Anthropic" are trademarks of Anthropic PBC, used here
nominatively to describe interoperability.

### User Responsibility and Local TLS Interception

To route traffic in proxy modes, Rayline Local can install a local certificate
authority on your machine and intercept TLS traffic to provider APIs locally,
using your own credentials. You are responsible for ensuring your use of
Rayline Local complies with the terms of service of any provider whose API you
route to. Install and use Rayline Local only on machines and accounts you
control.

## License and Trademarks

Rayline Local is licensed under the [Apache License 2.0](LICENSE). The Apache
license does not grant rights to the Rayline Local name or logos. See
[TRADEMARK.md](TRADEMARK.md).

Copyright 2026 Atlas Futures, Inc.

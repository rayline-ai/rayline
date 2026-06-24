# Rayline Local

Rayline Local is an open-source router from Atlas Futures, Inc. that runs on your
machine and sits between your coding agent and the AI models it talks to,
deciding where each request should go. The point is hybrid sessions: keep your
main agent on a frontier cloud model while quietly sending cheaper, high-volume
work — like background subagent tasks — to a fast model running locally.

It ships as two binaries: the `rayline` CLI and the `rld` daemon. Using it
locally through `rayline claude --local` needs no account and never connects to a
hosted service — everything runs with your own machine and credentials.

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

## Install

Release assets are published on
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
verify every downloaded binary against the release `SHA256SUMS` file.

## Quick Start

Start a Claude Code session with the auth-free local router. Your conversation
works exactly as it normally does — Rayline just routes background subagent work
to your on-device model:

```bash
rayline claude --local
```

Run it alongside a normal Claude Code session, in its own isolated config dir:

```bash
rayline claude --local --isolated
```

Check on the router, follow its logs, or stop it:

```bash
rayline router status
rayline router logs --lines 120
rayline router stop
```

Check for CLI updates:

```bash
rayline update --check
```

Run `rayline --help` for the full list of commands. For setup, configuration,
and provider endpoints, see the [Getting Started guide](docs/getting-started.md).

## How Routing Works

Three small flags on `rayline claude` decide where requests go. **Most people
only ever need `--local`** — the other two are advanced overrides.

| Flag | Question it answers | Values | Default |
| --- | --- | --- | --- |
| `--local` | Who decides routing? | on-device router when present, hosted cloud router when absent | cloud |
| `--via` | How does Claude Code connect? | `proxy`, `env` | `proxy` |
| `--route` | What flows through the router? | `all`, `subagents` | depends on router |

- `--local` runs the on-device static router: no login, nothing leaves your
  machine. Without it, the hosted cloud router at `api.rayline.ai` makes the
  decisions (needs `rayline auth login`).
- By default, local sessions are **hybrid**: your main agent stays on cloud
  Claude and only subagent traffic is routed. Pass `--route all` for a
  fully-local session.

The [Getting Started guide](docs/getting-started.md#choosing-where-requests-go)
has the full matrix and every valid combination.

## Use Rayline From Code or Agents

You can also send your own Anthropic API traffic through Rayline — from a script
or your own agent — using the official Anthropic SDKs. Examples come in Python
and TypeScript, grouped by routing path:

- **Cloud router** — point the SDK at `https://api.rayline.ai` with a router key:
  [examples/cloud/python](examples/cloud/python) ·
  [examples/cloud/typescript](examples/cloud/typescript)
- **Local routing** — start the router with `rayline router start`, then send the
  SDK through the proxy on `127.0.0.1:20810` and request model `rayline-local` so
  the call lands on your on-device model:
  [examples/local/python](examples/local/python) ·
  [examples/local/typescript](examples/local/typescript)

## Supported Clients

- Claude Code, Anthropic's CLI coding agent.

More clients may be supported over time.

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

## Documentation

- [Getting Started](docs/getting-started.md) — setup, routing flags, provider
  endpoints, logs, and troubleshooting.
- [Acceptance testing](docs/acceptance-testing.md) — validating the end-to-end
  Claude Code path through Rayline Local.
- [Release process](docs/release.md) — how releases are built and published.

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

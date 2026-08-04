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

Run Codex CLI through Rayline's local OpenAI Responses-compatible router. With
no `--config`, Rayline reuses Codex's existing ChatGPT/Codex subscription login
and forwards through the ChatGPT Codex backend:

```bash
rayline codex -- exec "summarize this repo"
```

Launch the Codex **desktop app** through Rayline in one step — it accepts the
same flags as `rayline codex`, starts the router, and points the app at Rayline:

```bash
rayline codex app                 # opens the desktop app routed through Rayline
rayline codex app ~/projects/app  # optionally open a workspace
```

Rayline sets the app up on an isolated Codex home so it doesn't disturb your
normal Codex configuration. Because the desktop app is single-instance, if it is
already running with a different configuration, `rayline codex app` prompts
before restarting it.

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

### Experimental C82 orchestrator

C82 is an experimental orchestrator that chooses among seven OpenRouter models
on every agent turn. Its small routing encoder runs on your GPU through native
libllama; the selected model call uses your OpenRouter account.

The C82 weights are not publicly downloadable yet. Before using it, your
Hugging Face account must have read access to the private
[`rayline-ai/mtrouter-c82`](https://huggingface.co/rayline-ai/mtrouter-c82)
repo. Create a read token for that account and expose it, along with your
OpenRouter key, to the Rayline process:

```bash
export HF_TOKEN="hf_..."
export OPENROUTER_API_KEY="sk-or-..."

rayline orchestrator doctor c82
```

`doctor` downloads the immutable C82 bundle on first use, verifies every
artifact hash, and confirms that the native encoder is active on Metal or CUDA.
Then launch Claude Code with one explicit routing scope:

```bash
# Let C82 choose the model for every Claude Code turn.
rayline claude --orchestrator c82 --route all

# Keep the main Claude session unchanged; use C82 for subagents only.
rayline claude --orchestrator c82 --route subagents
```

That is the complete setup: Rayline provisions and owns the local router
lifecycle. It never stores either key. `--route subagents` uses your normal
Claude login for the main agent. For diagnostics or constrained machines, add
`--router-device auto|mps|cuda|cpu` or `--router-memory-budget <GiB>` to either
command.

C82 currently targets Apple Silicon Metal and NVIDIA CUDA. The private bundle
pins the BF16 GGUF, Metal and CUDA helpers, libllama revision, policy weights,
provider order, retry behavior, and pricing snapshot. It is an experimental
serving path, not a production-promotion claim.

For the complete native and vLLM Semantic Router development environment,
including Modal ARC encoding, Codex/Claude Code smoke commands, and the
mmBERT-32K PII model, see the
[C82 development guide](docs/c82-development.md).

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
- **Codex / OpenAI Responses** — start the router with
  `rayline router start --mode codex` and point Codex at
  `http://127.0.0.1:20811/v1`, run `rayline codex ...` to have Rayline pass the
  provider overrides automatically, or `rayline codex app` to launch the Codex
  desktop app routed through Rayline. The default no-config path reuses
  Codex's ChatGPT subscription auth; explicit configs can route selected
  requests to local/API-key endpoints or to the hosted **cloud router (RCR)** at
  `api.rayline.ai` (the `R*` modes), which serves Codex natively over
  `/v1/responses` and picks a GPT model. Rayline supports Codex's Responses create
  stream, model catalog, compaction, memory-summary, images, and search provider
  calls, with native passthrough when the selected route is an `openai_responses`
  endpoint.

## Supported Clients

- Claude Code, Anthropic's CLI coding agent.
- Codex CLI and Codex app via OpenAI Responses-compatible local routing.

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

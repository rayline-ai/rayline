# Rayline Local Claude Routing

Rayline Local can run Claude Code through an explicit local static router.

- `rayline claude --local` starts the local static router path. It does
  not require hosted Rayline auth for routing decisions. (`--local-router` is a
  deprecated alias.)
- Built-in hosted Rayline auth uses Rayline-scoped CLI sessions. The CLI stores
  opaque `rls_`/`rlr_` session tokens and mints a separate `rlk-` router key for
  hosted data-plane requests.

## Quick Start

Run the local static router:

```bash
rayline claude --local
```

Run the local static router beside a normal Claude Code background daemon with
an isolated Claude config dir:

```bash
rayline claude --local --isolated
```

Start the local router + transparent proxy **without** launching Claude Code, for
use from your own code (e.g. an Anthropic SDK client pointed at the proxy on
`http://127.0.0.1:20810`). It routes every request through the router by default,
so requesting model `rayline-local` reaches your on-device model:

```bash
rayline router start                 # route all (default)
rayline router start --route subagents
rayline router stop                  # stop it when done
```

See [examples/cloud](../examples/cloud) and [examples/local](../examples/local)
for runnable Python and TypeScript clients.

Check for CLI updates:

```bash
rayline update --check
```

Inspect or stop Rayline Local router and proxy processes:

```bash
rayline router status
rayline router logs
rayline router logs --lines 120
rayline router top
rayline router stop
```

## Routing Arguments

Routing is controlled by three orthogonal flags on `rayline claude`. You rarely
need more than `--local`; the other two are advanced overrides.

| Flag | Axis | Values | Default |
| --- | --- | --- | --- |
| `--local` | **Router** — where routing decisions are made | flag (cloud when absent) | cloud (hosted) |
| `--via` | **Connection** — how Claude Code reaches the router | `proxy`, `env` | `proxy` |
| `--route` | **Scope** — what the proxy routes through the router | `all`, `subagents` | router-dependent (see below) |

### `--local` — router axis

- Absent: the **hosted cloud router** at `api.rayline.ai` makes routing and model
  decisions (requires `rayline auth login`).
- Present: the **on-device static router** decides locally. No login, nothing
  leaves your machine. `--local` forces the proxy (local inference is only
  reachable through it).

### `--via` — connection mechanism

How Rayline wires Claude Code to the router. Both are first-class Claude Code
injection points:

- `proxy` (default): a local proxy intercepts and forwards each request.
  **Required** to reach local inference or to route selectively. Enables
  `rayline top` and per-request metrics.
- `env`: points `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` at the router for the
  whole session — lightest weight, no background process. **Cloud-only**: it
  cannot reach local inference and cannot route selectively, so it is rejected
  when combined with `--local` or `--route subagents`.

### `--route` — proxy scope

What flows through the router versus passing straight through to Anthropic:

- `all`: route every request (main agent + subagents).
- `subagents`: route only subagent traffic; the main agent stays on cloud
  Claude. (The hybrid path — quality main agent, offloaded subagents.)

The default depends on the router, because the two are used differently:

- **Cloud router → `all`.** The hosted router does model selection; applying it
  universally is the intended behavior.
- **Local router → `subagents`.** Local sessions are hybrid by default; pass
  `--route all` for a fully-local session.

### Common combinations

| Command | Router | Connection | Scope |
| --- | --- | --- | --- |
| `rayline claude` | cloud | proxy | all |
| `rayline claude --via env` | cloud | env | all |
| `rayline claude --local` | local | proxy | subagents |
| `rayline claude --local --route all` | local | proxy | all |
| `rayline claude --route subagents` | cloud | proxy | subagents |

`rayline claude --via env --local` and `rayline claude --via env --route subagents`
are rejected: the env mechanism is cloud-only and cannot route selectively.

### Deprecated flags

These older flags still work for one release and print a one-line warning naming
the replacement:

| Deprecated | Use instead |
| --- | --- |
| `--local-router` | `--local` |
| `--no-proxy` | `--via env` |
| `--routing-mode override` | `--via env` |
| `--routing-mode proxy` | `--route all` |
| `--routing-mode proxy-subagents` | `--route subagents` |

## Hosted Auth and Proxy

Run `rayline auth login` to sign in to hosted Rayline. Browser login redeems a
PKCE code through `https://api.rayline.ai/v1/auth/cli/token`; device login uses
the Rayline-native `/v1/auth/cli/device/*` endpoints. The CLI stores
Rayline-scoped opaque session credentials, not Firebase or Google OAuth tokens.

Hosted Claude launches mint and store an `rlk-` router key separately from the
CLI session. Logging out calls `/v1/auth/cli/revoke` best-effort, clears the
local session, and drops the stored router key. The public local-router path
does not require hosted auth.

Custom hosted environments can still be configured in
`~/.config/rayline/settings.json` for internal/dev routing.

## Override-only Config

The router config is JSON layered over Rayline Local defaults. Save it wherever
you prefer. The examples below use `~/.config/rayline/local-router.json`.

Pass the file with `--router-config-path` when `routes.subagents` is meant to
limit which Claude Code subagents reach the local router. The local router can
also read `RAYLINE_ROUTER_CONFIG`, but the transparent proxy builds its
subagent allowlist only from `--router-config-path`.

This override keeps default Claude Code behavior default/passthrough, and routes
only an explicitly named `Explore` subagent to the active local model:

```json
{
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "local"
      }
    }
  }
}
```

Launch with that config:

```bash
rayline claude \
  --local \
  --isolated \
  --router-config-path ~/.config/rayline/local-router.json
```

Notes:

- `endpoint: "local"` means the managed local adapter. If `model` is omitted,
  Rayline Local fills in the active local model.
- In `proxy-subagents` mode, `routes.subagents` also becomes the proxy allowlist.
  The proxy matches the resolved Claude Code agent type case-insensitively, so
  `Explore` is enough when Claude Code has written subagent metadata. This
  allowlist behavior requires launching with `--router-config-path`; do not rely
  on `RAYLINE_ROUTER_CONFIG` alone for allowlists.
- If `routes.subagents` is omitted, all detected subagents can reach the local
  router. Main-thread Claude Code traffic still passes through normally in
  `proxy-subagents` mode.

## Provider Endpoints

Provider endpoints go in the same static router config. Use env vars for API
keys so secrets do not live in JSON. By default the local router sends
`api_key_env` as `x-api-key` for `anthropic_messages` endpoints and as bearer
auth for `openai_chat` endpoints. Set the optional per-endpoint `auth` field
(`"bearer"` or `"api_key"`) to override that default — for example OpenRouter's
Anthropic-native endpoint expects `"bearer"`.

Both protocols stream incrementally: `anthropic_messages` forwards the
upstream's native Anthropic SSE verbatim, and `openai_chat` translates the
upstream OpenAI Chat SSE into Anthropic SSE chunk by chunk (one
`content_block_delta` per fragment) so tokens reach Claude Code as they arrive.
`openai_chat` also forwards image blocks: Anthropic `image` blocks become OpenAI
`image_url` content parts (base64 sources become `data:` URLs).

Anthropic-compatible endpoint:

```json
{
  "endpoints": [
    {
      "id": "anthropic-compatible",
      "protocol": "anthropic_messages",
      "base_url": "https://anthropic-compatible.example.com",
      "api_key_env": "ANTHROPIC_COMPATIBLE_API_KEY",
      "models": ["claude-sonnet-4-6"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "anthropic-compatible",
        "model": "claude-sonnet-4-6"
      }
    }
  }
}
```

OpenRouter (recommended via its Anthropic-native endpoint). OpenRouter serves
`POST https://openrouter.ai/api/v1/messages`, a real Anthropic Messages
endpoint that returns native Anthropic SSE and supports images and tools. Use
`protocol: anthropic_messages` with `base_url: "https://openrouter.ai/api"` (the
router appends `/v1/messages`) and `auth: "bearer"` so the key is sent as
`Authorization: Bearer`. This gives true native streaming for free:

```json
{
  "endpoints": [
    {
      "id": "openrouter",
      "protocol": "anthropic_messages",
      "base_url": "https://openrouter.ai/api",
      "api_key_env": "OPENROUTER_API_KEY",
      "auth": "bearer",
      "models": ["anthropic/claude-sonnet-4.6"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "openrouter",
        "model": "anthropic/claude-sonnet-4.6"
      }
    }
  }
}
```

For OpenAI itself (which has no Anthropic-native endpoint), use the `openai_chat`
protocol with `https://api.openai.com/v1` and `OPENAI_API_KEY`. The router opens
the upstream with `stream: true` and translates the OpenAI Chat SSE into
Anthropic SSE in real time, so OpenAI also streams token by token:

```json
{
  "endpoints": [
    {
      "id": "openai",
      "protocol": "openai_chat",
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY",
      "models": ["gpt-4o-mini"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "openai",
        "model": "gpt-4o-mini"
      }
    }
  }
}
```

Arbitrary local OpenAI-compatible endpoint, including a separately managed
llama.cpp server:

```json
{
  "endpoints": [
    {
      "id": "local-llama",
      "protocol": "openai_chat",
      "base_url": "http://127.0.0.1:8080/v1",
      "api_key_env": "LLAMA_API_KEY",
      "models": ["qwen3.6-35b-local"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "local-llama",
        "model": "qwen3.6-35b-local"
      }
    }
  }
}
```

If that local server does not require auth, omit `api_key_env`.

Direct endpoints in the router JSON only control where the local router sends
matching requests. They do not control the daemon's managed local adapter. An
explicit `rayline claude --local` launch still starts the managed local
adapter with the active local-model config; without a custom local-model config,
that is the bundled llama.cpp path.

To use Rayline Local's managed local adapter, route to `endpoint: "local"` as
shown in the override-only example. If you want the managed adapter to use your
own server and skip the bundled GGUF model, configure a custom local model:

```bash
rayline local custom \
  --url http://127.0.0.1:8080 \
  --model qwen3.6-35b-local
```

The custom local-model endpoint must serve the Anthropic Messages API at
`/v1/messages`. The same config is stored under `local_model` in
`~/.config/rayline/settings.json`:

```json
{
  "local_model": {
    "mode": "custom",
    "base_url": "http://127.0.0.1:8080",
    "model": "qwen3.6-35b-local"
  }
}
```

## Logs

Router state lives under `~/.rayline/rld`.

| Component | Path |
| --- | --- |
| Local-router supervisor, adapter, injector, and local-router logs | `~/.rayline/rld/rl-rld.log` |
| Managed llama.cpp stdout/stderr | `~/.rayline/rld/llama-server.log` |
| Shared transparent proxy log | `~/.rayline/rld/rl-rld-proxy.log` |
| Isolated transparent proxy log | `~/.rayline/rld/cc/rl-rld-proxy.log` |

`rayline router logs` prints the combined `rl-rld.log`. Use `tail` for the
component-specific files:

```bash
tail -f ~/.rayline/rld/rl-rld.log
tail -f ~/.rayline/rld/llama-server.log
tail -f ~/.rayline/rld/rl-rld-proxy.log
tail -f ~/.rayline/rld/cc/rl-rld-proxy.log
```

Isolated launches use `~/.rayline/cc` as `CLAUDE_CONFIG_DIR` and keep their
proxy state in `~/.rayline/rld/cc`. The model/router process remains shared in
`~/.rayline/rld`; an isolated launch starts a separate proxy sidecar when needed.

`llama-server.log` is present only when the active local-model config uses the
bundled managed llama.cpp path. A direct endpoint in the router JSON can route
matching requests elsewhere, but it does not by itself stop the managed adapter
from starting. Configure `rayline local custom` to make the daemon use
`--upstream-url` and skip the bundled GGUF server.

## Manual Acceptance Checklist

1. Version and help:

   ```bash
   rayline --version
   rayline --help
   rayline claude --help
   rayline update --help
   ```

   Confirm the native Rayline Local CLI exposes `claude`, `router`, `local`, and
   `update`. Confirm `rayline claude --help` describes local routing and includes
   `--local`, `--isolated`, `--via`, `--route`, and `--router-config-path`.

2. Update check:

   ```bash
   rayline update --check
   ```

   Confirm the command reports either `Already on latest` with exit code 0 or
   `Update available` with exit code 1. For install-path validation without
   replacing the binary, run `rayline update --dry-run --version <version>`.

3. Local static-router isolated launch:

   ```bash
   rayline claude \
     --local \
     --isolated \
     --router-config-path ~/.config/rayline/local-router.json
   ```

   Confirm `~/.rayline/cc/settings.json` exists and
   `~/.rayline/rld/cc/rl-rld-proxy.log` is written.

4. Local default route:

   In the launched Claude Code session, run a normal main-thread prompt. Confirm
   the isolated proxy log shows passthrough routing, for example
   `selective_main_passthrough`, and the local-router log does not show that
   prompt as an `Explore` local route.

5. Local `Explore` route:

   Ask Claude Code to use the `Explore` subagent. Confirm the proxy log shows
   `selective_subagent_header` and the router log shows a local route with
   `policy=subagent:Explore` or another case-insensitive `Explore` match.

6. Log inspection:

   ```bash
   rayline router status
   rayline router logs --lines 120
   rayline router top
   tail -n 120 ~/.rayline/rld/rl-rld.log
   tail -n 120 ~/.rayline/rld/cc/rl-rld-proxy.log
   ```

   Confirm the status reports the expected ports and the logs identify the
   router config path, selected endpoint, and local model.

7. Parallel isolated sessions:

   Keep a normal Claude Code session running, then launch the isolated local
   router command in another terminal. Confirm the normal session keeps using
   its own daemon and the isolated session writes only under `~/.rayline/cc` and
   `~/.rayline/rld/cc` for Claude/proxy state. Start a second isolated launch
   with the same config and confirm it reuses or restarts only the isolated proxy,
   not the shared normal-session proxy.

## Troubleshooting

If local launch appears stuck, watch the combined router log and, for the
bundled managed llama.cpp path, the llama.cpp log:

```bash
tail -f ~/.rayline/rld/rl-rld.log
tail -f ~/.rayline/rld/llama-server.log
```

First model load can take a minute or two. If a custom local-model endpoint is
unreachable, check daemon status and probe the Anthropic Messages endpoint:

```bash
rayline router status
rayline local test --url http://127.0.0.1:8080 --model qwen3.6-35b-local
```

To force a clean restart:

```bash
rayline router stop
rayline claude --local --isolated --router-config-path ~/.config/rayline/local-router.json
```

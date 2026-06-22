# Rayline Local Claude Routing

Rayline Local can run Claude Code through an explicit local static router.

- `rayline claude --local-router` starts the local static router path. It does
  not require hosted Rayline auth for routing decisions.
- Built-in hosted Rayline auth uses Rayline-scoped CLI sessions. The CLI stores
  opaque `rls_`/`rlr_` session tokens and mints a separate `rlk-` router key for
  hosted data-plane requests.

## Quick Start

Run the local static router:

```bash
rayline claude --local-router
```

Run the local static router beside a normal Claude Code background daemon with
an isolated Claude config dir:

```bash
rayline claude --local-router --isolated
```

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
  --local-router \
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
keys so secrets do not live in JSON. The local router sends `api_key_env` as
`x-api-key` for `anthropic_messages` endpoints and as bearer auth for
`openai_chat` endpoints.

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

OpenRouter or OpenAI-compatible endpoint:

```json
{
  "endpoints": [
    {
      "id": "openrouter-fast",
      "protocol": "openai_chat",
      "base_url": "https://openrouter.ai/api/v1",
      "api_key_env": "OPENROUTER_API_KEY",
      "models": ["openai/gpt-5.2"]
    }
  ],
  "routes": {
    "subagents": {
      "Explore": {
        "endpoint": "openrouter-fast",
        "model": "openai/gpt-5.2"
      }
    }
  }
}
```

For OpenAI itself, use the same `openai_chat` protocol with
`https://api.openai.com/v1` and `OPENAI_API_KEY`.

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
explicit `rayline claude --local-router` launch still starts the managed local
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
   `--local-router`, `--isolated`,
   `--routing-mode`, and `--router-config-path`.

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
     --local-router \
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
rayline claude --local-router --isolated --router-config-path ~/.config/rayline/local-router.json
```

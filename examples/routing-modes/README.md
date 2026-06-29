# Routing-mode configs for `--config`

Each file here is a `RouterConfig` (`endpoints` + `routes`) you can drive with
either entry point:

- **interactive:** `rayline claude --config ./examples/routing-modes/RL.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/RL.json`
  (then point an Anthropic SDK client at the proxy on `127.0.0.1:20810`)

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagent`) from one file — the thing the old `--router-config-path` /
`settings.json` surfaces could not express (they are subagent-only).

## Routing

The mode name encodes three choices: 1st letter = the **agent** (main) provider,
2nd letter = the **subagent** provider (**R**ayline cloud / **A**nthropic /
**L**ocal), and the trailing number = the **local-model** toggle (`1` = off,
`2` = on). The `agent` / `subagent` / `local-model` / `router` columns describe
the *intent*; they are not CLI flags today — the one real surface is the
`CLI command` column (`rayline claude --config <file>`). Configs live in
`./examples/routing-modes/`.

| agent | subagent | local-model | router | Mode | Config | Main agent → | Subagents → | Local model | Auth | CLI command |
|---|---|---|---|---|---|---|---|---|---|---|
| `rayline` | `rayline` | off | cloud / local | **RR1** | [`RR.json`](./RR.json) | cloud model (via cloud router) | cloud model (via cloud router) | — | rayline | `rayline claude --config RR.json` |
| `rayline` | `rayline` | on | cloud / local | **RR2** | [`RR.json`](./RR.json) | cloud router picks: cloud **or local** | cloud router picks: cloud **or local** | main + subagents | rayline | `rayline claude --config RR.json` |
| `rayline` | `anthropic` | off | cloud / local | **RA1** † | [`RA.json`](./RA.json) | cloud model (via cloud router) | Anthropic (subscription) | — | rayline + Anthropic key | `rayline claude --config RA.json` |
| `rayline` | `anthropic` | on | cloud / local | **RA2** † | [`RA.json`](./RA.json) | cloud router picks: cloud **or local** | Anthropic (subscription) | main | rayline + Anthropic key | `rayline claude --config RA.json` |
| `rayline` | `local` | off | cloud / local | **RL1** | [`RL.json`](./RL.json) | cloud model (via cloud router) | local model | subagents | rayline | `rayline claude --config RL.json` |
| `rayline` | `local` | on | cloud / local | **RL2** | [`RL.json`](./RL.json) | cloud router picks: cloud **or local** | local model | main + subagents | rayline | `rayline claude --config RL.json` |
| `anthropic` | `rayline` | off | cloud / local | **AR1** | [`AR.json`](./AR.json) | Anthropic (subscription) | cloud model (via cloud router) | — | subscription + rayline | `rayline claude --config AR.json` |
| `anthropic` | `rayline` | on | cloud / local | **AR2** | [`AR.json`](./AR.json) | Anthropic (subscription) | cloud router picks: cloud **or local** | subagents | subscription + rayline | `rayline claude --config AR.json` |
| `anthropic` | `local` | N/A | N/A | **AL** | [`AL.json`](./AL.json) | Anthropic (subscription) | local model | subagents | subscription | `rayline claude --config AL.json` |
| `local` | `rayline` | off | cloud / local | **LR1** ‡ | [`LR.json`](./LR.json) | local model | cloud model (via cloud router) | main | rayline | `rayline claude --config LR.json` |
| `local` | `rayline` | on | cloud / local | **LR2** ‡ | [`LR.json`](./LR.json) | local model | cloud router picks: cloud **or local** | main + subagents | rayline | `rayline claude --config LR.json` |
| `local` | `anthropic` | N/A | N/A | **LA** † ‡ | [`LA.json`](./LA.json) | local model | Anthropic (subscription) | main | subscription | `rayline claude --config LA.json` |
| `local` | `local` | N/A | N/A | **LL** ‡ | [`LL.json`](./LL.json) | local model | local model | main + subagents | none | `rayline claude --config LL.json` |
| `rayline` | `local` (per-type) | off | cloud / local | **RL\*** | [`RL-per-type.json`](./RL-per-type.json) | cloud model (via cloud router) | `Explore`/`Plan` → local; others → cloud | subagents | rayline | `rayline claude --config RL-per-type.json` |

**† pathological — subscription on the subagent side is not expressible.**
Subagents are the *routed* class and the router cannot forward your Claude
subscription OAuth, so `RA1`/`RA2`/`LA` ship the **Anthropic API-key** variant
(`ANTHROPIC_API_KEY`) instead. Swap in a subscription and only the subagent leg
breaks; the intent column shows what the mode *means*.

**‡ expected-fail end-to-end today — `agent = local`.** These modes run the
**main** agent on a local model, and current small local models (e.g. qwen
7B/9B) cannot reliably drive Claude Code's tool-use protocol — they emit tool
calls as plain text instead of invoking tools, so the main agent rarely spawns
subagents or uses `Read`/`Edit`/`Bash` at all. The **routing** is verified for
these configs (see [Tests](#tests)); the local-main **capability** is not yet
viable, so a full interactive run is expected to fall short. Revisit once a
tool-capable local main is available — the live e2e test
(`it_local_main_e2e`, `#[ignore]`d) is the harness for that.

### Column legend

- **agent** — provider for the **main agent** (your top-level Claude Code
  conversation): `rayline` (the cloud router), `anthropic` (your subscription,
  via proxy passthrough), or `local` (an on-device model).
- **subagent** — provider for **subagents** (Task-tool agents like `Explore`,
  spawned by the main agent). Can be split per subagent **type** via
  `routes.subagents` (see `RL-per-type.json`).
- **local-model** — only meaningful for a class set to `rayline`: whether the
  cloud router *may* serve that class from a local model (`on`) or cloud only
  (`off`). This is a runtime cloud-router ("may-local") decision, so the `on`/`off`
  pair shares one config file. `N/A` for `anthropic` / `local`.
- **router** — only meaningful for a class set to `rayline`: which logic picks
  the model — `cloud` (the cloud router's intelligent choice) vs `local` (your
  on-device static rules). `N/A` for `anthropic` / `local`.

## Files ↔ modes

13 modes collapse to **9 config files**: the `local-model` `on`/`off` pair
(`RR1`/`RR2`, `RA1`/`RA2`, …) is a runtime cloud-router decision the static
config cannot distinguish, so each file covers both. `RL-per-type.json` is the
"+more" — a granular variant of `RL` that splits subagents by type.

| File | `routes.main` → | `routes.subagent` → | Covers | Status |
|---|---|---|---|---|
| [`RR.json`](./RR.json) | rayline-cloud | rayline-cloud | RR1, RR2 | ✅ works as-is |
| [`RA.json`](./RA.json) | rayline-cloud | anthropic (API key) | RA1, RA2 | ⚠ subscription not expressible |
| [`RL.json`](./RL.json) | rayline-cloud | ollama (local) | RL1, RL2 | ⚙ needs a local model |
| [`AR.json`](./AR.json) | subscription (passthrough) | rayline-cloud | AR1, AR2 | ✅ works as-is |
| [`AL.json`](./AL.json) | subscription (passthrough) | ollama (local) | AL | ✅ works as-is |
| [`LR.json`](./LR.json) | ollama (local) | rayline-cloud | LR1, LR2 | ⚙ needs a local model · ⛔ local-main e2e (‡) |
| [`LA.json`](./LA.json) | ollama (local) | anthropic (API key) | LA | ⚠ subscription not expressible · ⛔ local-main e2e (‡) |
| [`LL.json`](./LL.json) | ollama (local) | ollama (local) | LL | routing ✅ · ⛔ local-main e2e (‡) |
| [`RL-per-type.json`](./RL-per-type.json) | rayline-cloud | per-type: `Explore`/`Plan` → ollama, default → rayline-cloud | RL\* | ⚙ needs a local model |

The proxy **scope** is derived from `routes.main`:

- `routes.main.endpoint == "subscription"` (a reserved sentinel) **or absent** →
  the main agent passes through to your own Claude subscription
  (selective-subagents scope). You do **not** declare `subscription` under
  `endpoints`.
- `routes.main` → any real endpoint → the main agent is routed (route-all scope).

## Config model — `endpoints` + `routes`

Real `EndpointConfig` fields only: `id`, `protocol`
(`anthropic_messages` | `openai_chat`), `base_url`, `models`, `api_key_env`,
`auth` (`api_key` | `bearer`), `headers`. A route's `endpoint` is looked up by
`id`; its `model` is rewritten into the request body.

```jsonc
{
  "endpoints": [
    { "id": "rayline-cloud", "protocol": "anthropic_messages", "base_url": "https://api.rayline.ai",
      "models": ["rayline-router"], "api_key_env": "RAYLINE_ROUTER_API_KEY", "auth": "api_key" },
    { "id": "ollama", "protocol": "openai_chat", "base_url": "http://127.0.0.1:11434/v1",
      "models": ["qwen3.5:9b", "qwen2.5-coder:7b"] }     // local, no auth
  ],
  "routes": {
    "main":     { "endpoint": "rayline-cloud", "model": "rayline-router" },
    "subagent": { "endpoint": "ollama", "model": "qwen3.5:9b" },   // singular = subagent default
    "subagents": {                                                 // optional per-type overrides
      "Explore": { "endpoint": "ollama", "model": "qwen2.5-coder:7b" }
    }
  }
}
```

> Note: `routes.subagent` (singular) is the subagent **default**;
> `routes.subagents` (the map) is **only** for per-type overrides.

## Auth

- `rayline-cloud` reads `RAYLINE_ROUTER_API_KEY` (an `rlk-` key). For
  `rayline claude`, your `rayline auth login` session key is injected
  automatically, so the env var is optional in interactive use.
- `anthropic` reads `ANTHROPIC_API_KEY` (API key — the local router cannot use
  the subscription).
- `ollama` needs no key (point `base_url` at your server).

## Visual demo

[`visual-demo.sh`](./visual-demo.sh) records an asciinema cast of any mode in a
split-pane tmux session — the left pane runs `rayline claude --config <mode>` and
the right pane runs `rayline top`, so you can watch the routing live:

```bash
./examples/routing-modes/visual-demo.sh RR        # default mode is RR
./examples/routing-modes/visual-demo.sh RL        # any mode from the table
```

Requires `asciinema` and `tmux`. It forces `--via proxy` (so `rayline top` has
metrics to show) and writes `<MODE>-demo.cast`; play it with
`asciinema play <MODE>-demo.cast`.

## Tests

Routing is regression-tested hermetically (no credentials, loopback-only):

- **Every config in this directory** is swept in
  `rayline-local-router` unit tests — `config_mode_examples_route_main_and_subagents`
  loads each `*.json` and asserts the main + subagent (+ per-type `Explore`/`Plan`)
  routing decision, and `example_configs_parse` asserts they all deserialize.
- **Full HTTP path** in `crates/rayline-local-router/tests/it_mock_upstream.rs`
  (`config_routes_main_and_subagent_to_distinct_endpoints`): mock upstreams stand
  in for each endpoint; a main request (no agent headers) and a subagent request
  (`x-claude-code-agent-id` + `x-rayline-claude-code-agent-type`) prove each class
  routes to its configured endpoint over real HTTP.

The selective-main-subscription passthrough (`AR`/`AL` main) is a proxy-layer
behavior, covered in `crates/rayline-proxy`.

The full **interactive** end-to-end for the `agent = local` modes
(`LR`/`LA`/`LL`, marked ‡) is **expected to fail** with current small local
models and is kept `#[ignore]`d in
`crates/rayline-cli/tests/it_local_main_e2e.rs`. Run it once a tool-capable local
main is configured:

```bash
CLAUDE_BIN=/path/to/claude RAYLINE_LOCAL_MAIN_E2E=1 \
  cargo test -p rayline-cli --test it_local_main_e2e -- --ignored --nocapture
```

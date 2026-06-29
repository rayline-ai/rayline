# Routing-mode configs for `--config`

Each file here is a `RouterConfig` (`endpoints` + `routes`) you can drive with
either entry point:

- **interactive:** `rayline claude --config ./examples/routing-modes/RLC.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/RLC.json`
  (then point an Anthropic SDK client at the proxy on `127.0.0.1:20810`)

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagent`) from one file — the thing the old `--router-config-path` /
`settings.json` surfaces could not express (they are subagent-only).

> **Scope of this PR.** Only the **supported** modes (✅ in the table below) ship
> as config files and route end-to-end today. The unsupported modes (❌) need the
> two `rayline`-only sub-axes — **may-local from config** (`RRCL`/…) and the
> **on-device LSR decider** (`RRL`/…) — which land in a follow-up PR. They are
> listed here for completeness and have no `.json` yet. The full design lives in
> the routing-modes design doc.

## Mode names

The mode name encodes the routing choices, left to right:

1. **agent** (main) provider — **R**ayline / **A**nthropic / **L**ocal
2. **subagent** provider — **R**ayline / **A**nthropic / **L**ocal
3. **rayline engine** suffix (only when a class is `rayline`):
   - **C** — `router: rayline-cloud`, **local-model off** (cloud only)
   - **CL** — `router: rayline-cloud`, **local-model on** (may-local)
   - **L** — `router: rayline-local` (the on-device LSR decides)

Modes with no `rayline` class (`AL`, `LA`, `LL`) have no suffix — `router` and
`local-model` are `N/A` for `anthropic` / `local`.

### `rayline` is both a router *and* a provider

`anthropic` and `local` are fixed **destinations** — the provider *is* the
endpoint, so there is nothing more to decide. **`rayline` is different: it is a
routing *system*, not a destination.** Choosing `rayline` for a class therefore
opens **two independent sub-axes that apply only to `rayline`** — which is exactly
why the `router` and `local-model` columns exist and are `N/A` for
`anthropic` / `local`:

- **`router`** — *which rayline decider runs*: `rayline-cloud` = the hosted
  **RCR** (intelligent ML pick) vs `rayline-local` = the on-device **LSR** (your
  static rules). Two genuinely different deciders. The `rayline-` prefix keeps the
  *engine* distinct from the `local`/`anthropic` **providers** — `router: rayline-local`
  is not the same thing as `subagent: local`.
- **`local-model`** — a **sub-knob of `router: rayline-cloud`**: may the cloud RCR
  **redirect** that class to a local model (`on`) or stay cloud-only (`off`).
  **`N/A` when `router: rayline-local`** (the on-device router already routes locally
  itself) and N/A for `anthropic`/`local`.

The two sub-axes **nest** — `rayline` → `router` (`rayline-cloud`|`rayline-local`) →
*only under `rayline-cloud`* → `local-model` (`on`|`off`) — so a `rayline` class has
**three** distinct behaviours (the suffix `C`/`CL`/`L`), not four:

| `router` | `local-model` | a `rayline` class then… | suffix |
|---|---|---|---|
| rayline-cloud | off | RCR serves a **cloud** model only | `C` |
| rayline-cloud | on | RCR may **redirect to a local model** (may-local) | `CL` |
| rayline-local | — (N/A) | the on-device **LSR routes it itself** | `L` |

## Modes

| Mode | agent | subagent | router | local-model | Main agent → | Subagents → | Auth | Supported | Config |
|---|---|---|---|---|---|---|---|:--:|---|
| **RRC** | `rayline` | `rayline` | rayline-cloud | off | cloud (RCR) | cloud (RCR) | rayline | ✅ Y | [`RRC.json`](./RRC.json) |
| **RRCL** § | `rayline` | `rayline` | rayline-cloud | on | RCR picks cloud **or local** | RCR picks cloud **or local** | rayline | ✅ Y | [`RRCL.json`](./RRCL.json) |
| **RRL** | `rayline` | `rayline` | rayline-local | N/A | on-device LSR decides | on-device LSR decides | rayline | ❌ N | — (LSR decider) |
| **RAC** † | `rayline` | `anthropic` | rayline-cloud | off | cloud (RCR) | Anthropic (API key) | rayline + Anthropic key | ✅ Y | [`RAC.json`](./RAC.json) |
| **RACL** † | `rayline` | `anthropic` | rayline-cloud | on | RCR picks cloud **or local** | Anthropic (API key) | rayline + Anthropic key | ❌ N | — (may-local) |
| **RAL** † | `rayline` | `anthropic` | rayline-local | N/A | on-device LSR decides | Anthropic (API key) | rayline + Anthropic key | ❌ N | — (LSR decider) |
| **RLC** | `rayline` | `local` | rayline-cloud | off | cloud (RCR) | local model | rayline | ✅ Y | [`RLC.json`](./RLC.json) |
| **RLCL** | `rayline` | `local` | rayline-cloud | on | RCR picks cloud **or local** | local model | rayline | ❌ N | — (may-local) |
| **RLL** | `rayline` | `local` | rayline-local | N/A | on-device LSR decides | local model | rayline | ❌ N | — (LSR decider) |
| **ARC** | `anthropic` | `rayline` | rayline-cloud | off | Anthropic (subscription) | cloud (RCR) | subscription + rayline | ✅ Y | [`ARC.json`](./ARC.json) |
| **ARCL** | `anthropic` | `rayline` | rayline-cloud | on | Anthropic (subscription) | RCR picks cloud **or local** | subscription + rayline | ❌ N | — (may-local) |
| **ARL** | `anthropic` | `rayline` | rayline-local | N/A | Anthropic (subscription) | on-device LSR decides | subscription + rayline | ❌ N | — (LSR decider) |
| **AL** | `anthropic` | `local` | N/A | N/A | Anthropic (subscription) | local model | subscription | ✅ Y | [`AL.json`](./AL.json) |
| **LRC** ‡ | `local` | `rayline` | rayline-cloud | off | local model | cloud (RCR) | rayline | ✅ Y | [`LRC.json`](./LRC.json) |
| **LRCL** ‡ | `local` | `rayline` | rayline-cloud | on | local model | RCR picks cloud **or local** | rayline | ❌ N | — (may-local) |
| **LRL** ‡ | `local` | `rayline` | rayline-local | N/A | local model | on-device LSR decides | rayline | ❌ N | — (LSR decider) |
| **LA** † ‡ | `local` | `anthropic` | N/A | N/A | local model | Anthropic (API key) | subscription / API key | ✅ Y | [`LA.json`](./LA.json) |
| **LL** ‡ | `local` | `local` | N/A | N/A | local model | local model | none | ✅ Y | [`LL.json`](./LL.json) |

Plus a granular variant of `RLC` that splits subagents by **type**
(`Explore`/`Plan` → distinct local models, everything else → cloud):
[`RLC-per-type.json`](./RLC-per-type.json) — ✅ supported.

**† subscription on the subagent side is not expressible.** Subagents are the
*routed* class and the router cannot forward your Claude subscription OAuth, so
`RA*`/`LA` ship the **Anthropic API-key** variant (`ANTHROPIC_API_KEY`) instead.
Swap in a subscription and only the subagent leg breaks; the intent columns show
what the mode *means*.

**‡ expected-fail end-to-end today — `agent = local`.** These modes run the
**main** agent on a local model, and current small local models (e.g. qwen
7B/9B) cannot reliably drive Claude Code's tool-use protocol — they emit tool
calls as plain text instead of invoking tools, so the main agent rarely spawns
subagents or uses `Read`/`Edit`/`Bash` at all. The **routing** is verified for the
supported configs (see [Tests](#tests)); the local-main **capability** is not yet
viable, so a full interactive run is expected to fall short. The live e2e test
(`it_local_main_e2e`, `#[ignore]`d) is the harness for that.

**§ may-local from config — the redirect itself is hosted/account-gated.**
`RRCL` wires the **client** contract from config: a `rayline-cloud` route carrying
`local_models` stands up a custom adapter fronting the named local endpoint and
advertises it to the RCR (`x-rayline-local-available` + the model id), decoupled
from the `rayline local on/off` account toggle. Whether a turn is actually
redirected to local is the **hosted RCR's** runtime decision and is account-gated
today — so without it (or with the account flag off) `RRCL` behaves like `RRC`
(cloud only). The advertisement + redirect *plumbing* is hermetically tested; the
end-to-end redirect decision is exercised only by the ignored live test. The other
`*CL` modes (`RACL`/`RLCL`/`ARCL`/`LRCL`) reuse this once their non-may-local base
(`RAC`/`RLC`/…) is combined with the same advertisement — tracked separately.

### What "Supported" means

- **✅ Y** — routable by the shipping `--config` engine today; a config file is
  provided and exercised by the hermetic tests below. (`RRCL` is supported for the
  client/advertisement contract; its actual local redirect is hosted-gated — see §.)
- **❌ N** — needs a `rayline`-only sub-axis not yet wired:
  - **may-local on a non-`RR` base** (`RACL`/`RLCL`/`ARCL`/`LRCL`) — the `RRCL`
    advertisement combined with that mode's base routing; not yet wired.
  - **on-device LSR decider** (`*L` modes) — `router: rayline-local` needs a new
    on-device selection policy in the LSR (today the LSR routes by static rule
    only). This is the **RRL** follow-up.

## Files ↔ modes

The supported modes ship as **10 config files** (the `❌` modes have none yet):

| File | `routes.main` → | `routes.subagent` → | Mode |
|---|---|---|---|
| [`RRC.json`](./RRC.json) | rayline-cloud | rayline-cloud | RRC |
| [`RRCL.json`](./RRCL.json) | rayline-cloud (+ `local_models`) | rayline-cloud (+ `local_models`) | RRCL § |
| [`RAC.json`](./RAC.json) | rayline-cloud | anthropic (API key) | RAC |
| [`RLC.json`](./RLC.json) | rayline-cloud | ollama (local) | RLC |
| [`ARC.json`](./ARC.json) | subscription (passthrough) | rayline-cloud | ARC |
| [`AL.json`](./AL.json) | subscription (passthrough) | ollama (local) | AL |
| [`LRC.json`](./LRC.json) | ollama (local) | rayline-cloud | LRC |
| [`LA.json`](./LA.json) | ollama (local) | anthropic (API key) | LA |
| [`LL.json`](./LL.json) | ollama (local) | ollama (local) | LL |
| [`RLC-per-type.json`](./RLC-per-type.json) | rayline-cloud | per-type: `Explore`/`Plan` → ollama, default → rayline-cloud | RLC\* |

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

### `rayline`-only route fields (v2)

A route targeting the `rayline` cloud endpoint accepts two optional fields:

```jsonc
"main": {
  "endpoint": "rayline-cloud", "model": "rayline-router",
  "router": "rayline-cloud",              // rayline-cloud (default) | rayline-local (RRL — not yet)
  "local_models": ["qwen2.5-coder:7b"]    // non-empty ⇒ may-local ON (RRCL); must be served by a declared local endpoint
}
```

- **`router`** — which rayline decider runs. `rayline-cloud` (or absent) = the
  hosted RCR. `rayline-local` (the on-device LSR decider) is **not yet supported**.
- **`local_models`** — the model ids the cloud RCR may redirect this class to
  (may-local). A non-empty list turns may-local **on** and advertises
  `local_models[0]`; the id must appear in a declared local endpoint's `models`
  (that endpoint's `base_url` is the redirect target). Both fields are ignored for
  `anthropic`/`local` endpoints and do not change the local router's own routing.

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
./examples/routing-modes/visual-demo.sh RRC       # default mode is RRC
./examples/routing-modes/visual-demo.sh RLC       # any supported mode from the table
```

Requires `asciinema` and `tmux`. It forces `--via proxy` (so `rayline top` has
metrics to show) and writes `<MODE>-demo.cast`; play it with
`asciinema play <MODE>-demo.cast`.

## Tests

Routing is regression-tested hermetically (no credentials, loopback-only):

- **Every supported config in this directory** is swept in `rayline-local-router`
  unit tests — `config_mode_examples_route_main_and_subagents` loads each `*.json`
  and asserts the main + subagent (+ per-type `Explore`/`Plan`) routing decision,
  and `example_configs_parse` asserts they all deserialize.
- **Per-mode CLI derivation** in `rayline-cli` — `example_mode_configs_derive_expected_routing`
  cross-checks each config's derived proxy scope, local-router engagement, and
  cloud-key need against the mode's intent.
- **Full HTTP path** in `crates/rayline-local-router/tests/it_mock_upstream.rs`
  (`config_routes_main_and_subagent_to_distinct_endpoints`): mock upstreams stand
  in for each endpoint; a main request (no agent headers) and a subagent request
  (`x-claude-code-agent-id` + `x-rayline-claude-code-agent-type`) prove each class
  routes to its configured endpoint over real HTTP.

The selective-main-subscription passthrough (`ARC`/`AL` main) is a proxy-layer
behavior, covered in `crates/rayline-proxy`.

**may-local — `RRCL` (§).** The config→advertisement mapping is unit-tested in
`rayline-cli` (`router_config::tests::may_local_*` and `rrcl_example_resolves_may_local`):
a `rayline-cloud` route with `local_models` resolves to the advertised model + the
local endpoint's upstream URL, and `rayline-local`/no-`local_models` routes resolve
to none. `config_mode_examples_route_main_and_subagents` also asserts the
`router`/`local_models` fields **do not** change the LSR's routing (RRCL still
routes main + subagents to cloud). The proxy half — advertising
`x-rayline-local-available` and following the router's `307` to the local adapter —
is hermetically tested in `crates/rayline-proxy`
(`proxy_stashes_router_auth_for_local_307`,
`local_proxy_redirect_uses_shared_router_auth_for_usage_update`). The **actual
redirect decision** is the hosted RCR's call and is account-gated, so the true
end-to-end (whether a turn lands on local) is not — and cannot be — a hermetic
config test; that path is exercised by the ignored live test
`crates/rayline-proxy/tests/it_claude_live.rs`.

The full **interactive** end-to-end for the `agent = local` modes
(`LRC`/`LA`/`LL`, marked ‡) is **expected to fail** with current small local
models and is kept `#[ignore]`d in
`crates/rayline-cli/tests/it_local_main_e2e.rs`. Run it once a tool-capable local
main is configured:

```bash
CLAUDE_BIN=/path/to/claude RAYLINE_LOCAL_MAIN_E2E=1 \
  cargo test -p rayline-cli --test it_local_main_e2e -- --ignored --nocapture
```

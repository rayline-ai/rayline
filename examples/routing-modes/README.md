# Routing-mode configs for `--config`

Each file here is a `RouterConfig` (`endpoints` + `routes`) you can drive with
either entry point:

- **interactive:** `rayline claude --config ./examples/routing-modes/RLC.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/RLC.json`
  (then point an Anthropic SDK client at the proxy on `127.0.0.1:20810`)

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagent`) from one file — the thing the old `--router-config-path` /
`settings.json` surfaces could not express (they are subagent-only).

> **Scope of this PR.** The **supported** modes (✅ in the table below) ship as
> config files and route end-to-end today — including `RRCL`/`ARCL` (may-local from
> config) and `RRL` (on-device LSR static routing). The remaining unsupported modes
> (❌) are listed for completeness and have no `.json` yet. The full design lives in
> the routing-modes design doc.

## Mode names

The mode name encodes the routing choices, left to right:

1. **agent** (main) provider — **R**ayline / **A**nthropic / **L**ocal
2. **subagent** provider — **R**ayline / **A**nthropic / **L**ocal
3. **rayline engine** suffix (only when a class is `rayline`):
   - **C** — `router: rayline-cloud`, **local-model off** (cloud only)
   - **CL** — `router: rayline-cloud`, **local-model on** (may-local)
   - **L** — `router: rayline-local` (the on-device LSR is the router: it routes the
     class **statically per the JSON** and pins the route's `model`, instead of the
     hosted RCR deciding — even when the endpoint is `rayline-cloud`)

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
  **Today may-local only takes effect for exploration subagents (`Explore`) — the
  main agent is always cloud** (a config-declared local model is a *custom*
  endpoint, which the RCR delegates exploration-only). **`N/A` when
  `router: rayline-local`** and for `anthropic`/`local`.

The two sub-axes **nest** — `rayline` → `router` (`rayline-cloud`|`rayline-local`) →
*only under `rayline-cloud`* → `local-model` (`on`|`off`) — so a `rayline` class has
**three** distinct behaviours (the suffix `C`/`CL`/`L`), not four:

| `router` | `local-model` | a `rayline` class then… | suffix |
|---|---|---|---|
| rayline-cloud | off | RCR serves a **cloud** model only | `C` |
| rayline-cloud | on | RCR may **redirect to a local model** (may-local) — today **`Explore` subagents only; main stays cloud** | `CL` |
| rayline-local | — (N/A) | the on-device **LSR routes it itself** | `L` |

## Modes

| Mode | agent | subagent | router | local-model | Main agent → | Subagents → | Auth | Supported | Config |
|---|---|---|---|---|---|---|---|:--:|---|
| **RRC** | `rayline` | `rayline` | rayline-cloud | off | cloud (RCR) | cloud (RCR) | rayline | ✅ Y | [`RRC.json`](./RRC.json) |
| **RRCL** § | `rayline` | `rayline` | rayline-cloud | on | cloud (RCR) § | RCR may send a subagent → local | rayline | ✅ Y | [`RRCL.json`](./RRCL.json) |
| **RRL** | `rayline` | `rayline` | rayline-local | N/A | cloud model (via local router) | cloud model (via local router) | rayline | ✅ Y | [`RRL.json`](./RRL.json) |
| **RAC** † | `rayline` | `anthropic` | rayline-cloud | off | cloud (RCR) | Anthropic (API key) | rayline + Anthropic key | ✅ Y | [`RAC.json`](./RAC.json) |
| **RACL** † | `rayline` | `anthropic` | rayline-cloud | on | cloud (RCR) § | Anthropic (API key) | rayline + Anthropic key | ❌ N | — (may-local) |
| **RAL** † | `rayline` | `anthropic` | rayline-local | N/A | cloud model (via local router) | Anthropic (API key) | rayline + Anthropic key | ✅ Y | [`RAL.json`](./RAL.json) |
| **RLC** | `rayline` | `local` | rayline-cloud | off | cloud (RCR) | local model | rayline | ✅ Y | [`RLC.json`](./RLC.json) |
| **RLCL** | `rayline` | `local` | rayline-cloud | on | cloud (RCR) § | local model | rayline | ❌ N | — (may-local) |
| **RLL** | `rayline` | `local` | rayline-local | N/A | cloud model (via local router) | local model | rayline | ✅ Y | [`RLL.json`](./RLL.json) |
| **ARC** | `anthropic` | `rayline` | rayline-cloud | off | Anthropic (subscription) | cloud (RCR) | subscription + rayline | ✅ Y | [`ARC.json`](./ARC.json) |
| **ARCL** § | `anthropic` | `rayline` | rayline-cloud | on | Anthropic (subscription) | RCR may send a subagent → local | subscription + rayline | ✅ Y | [`ARCL.json`](./ARCL.json) |
| **ARL** | `anthropic` | `rayline` | rayline-local | N/A | Anthropic (subscription) | cloud model (via local router) | subscription + rayline | ✅ Y | [`ARL.json`](./ARL.json) |
| **AL** | `anthropic` | `local` | N/A | N/A | Anthropic (subscription) | local model | subscription | ✅ Y | [`AL.json`](./AL.json) |
| **LRC** ‡ | `local` | `rayline` | rayline-cloud | off | local model | cloud (RCR) | rayline | ✅ Y | [`LRC.json`](./LRC.json) |
| **LRCL** ‡ | `local` | `rayline` | rayline-cloud | on | local model | subagents → local (on-device, per `local_models`) | rayline | ✅ Y | [`LRCL.json`](./LRCL.json) |
| **LRL** ‡ | `local` | `rayline` | rayline-local | N/A | local model | cloud model (via local router) | rayline | ✅ Y | [`LRL.json`](./LRL.json) |
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

**§ may-local from config — custom-endpoint scope + hosted decision.**
`RRCL` wires the **client** contract from config: a `rayline-cloud` route carrying
`local_models` stands up a custom adapter fronting the named local endpoint and
advertises it to the RCR (`x-rayline-local-available` + the model id), decoupled
from the `rayline local on/off` account toggle. Two important limits:

- **Exploration subagents only (custom endpoint).** A config `local_models` points
  at a *custom* local endpoint (your ollama), so the proxy sends
  `x-rayline-local-custom` and the RCR **only delegates exploration subagents
  (`Explore`) to it — never the main agent** or other classes (`proxy/src/lib.rs`
  `custom_mode`). The "Main agent →" column above is therefore the *intent*; in
  practice the main turn stays cloud. Main-agent may-local would need a
  trusted/bundled model path or a hosted change.
- **The redirect is the hosted RCR's call.** Whether a (qualifying) turn is
  actually sent to local is the RCR's runtime decision; without it (or with the
  account flag off) `RRCL` behaves like `RRC`.

Verified on-device: `rayline claude --config RRCL.json` with an `Explore`-spawning
task → adapter forwards to `http://<endpoint>/v1/messages` and `rayline top` shows
`model=<local>, target=local, agent_type=Explore` while main turns stay cloud.
**`ARCL`** works the same way (main passes through to the subscription; the cloud
subagent advertises may-local) and is likewise verified on-device. The
advertisement + redirect *plumbing* is hermetically tested; the end-to-end redirect
is exercised only by the ignored live test. The remaining `*CL` modes
(`RACL`/`RLCL`/`LRCL`) reuse this once their non-may-local base (`RAC`/`RLC`/…) is
combined with the same advertisement — tracked separately.

### What "Supported" means

- **✅ Y** — routable by the shipping `--config` engine today; a config file is
  provided and exercised by the hermetic tests below. (`RRCL` is supported for the
  client/advertisement contract; its actual local redirect is hosted-gated — see §.)
  All `router: rayline-local` modes (`RRL`/`RAL`/`RLL`/`ARL`/`LRL`) are supported:
  `router: rayline-local` is **static LSR routing** — the JSON is the decider (the
  LSR routes each class per the config and pins its `model`), no ML policy needed.
  **`LRCL`** is supported too: when a `rayline-cloud` **subagent** route carries
  `local_models` *and* the run is LSR-routed (main is local), the LSR sends those
  subagents to the local model on-device (`policy=subagent:may-local`). Which
  subagents is **config-driven** — `local_models` on `routes.subagent` covers all
  subagents; on a `routes.subagents.<type>` entry covers just that type (no
  hard-coded agent type). On-device and deterministic, unlike the hosted RCR's
  discretionary, Explore-only redirect for cloud-routed `RRCL`/`ARCL`.
- **❌ N** — by design, not a gap:
  - **may-local on the `main` (agent) class** (`RACL`/`RLCL`) — `--local-model=on`
    is on the *agent*, but the main agent never goes local (may-local is
    Explore-subagents-only). So the `CL` distinction is inert — these behave exactly
    like `RAC`/`RLC`. Unsupported-by-design until main-agent may-local exists
    (hosted-side).

## Files ↔ modes

The supported modes ship as **17 config files** (the `❌` modes have none yet):

| File | `routes.main` → | `routes.subagent` → | Mode |
|---|---|---|---|
| [`RRC.json`](./RRC.json) | rayline-cloud | rayline-cloud | RRC |
| [`RRCL.json`](./RRCL.json) | rayline-cloud (+ `local_models`) | rayline-cloud (+ `local_models`) | RRCL § |
| [`RRL.json`](./RRL.json) | rayline-cloud, `router: rayline-local` (model pinned) | rayline-cloud, `router: rayline-local` (default + `Explore` per-type) | RRL |
| [`RAC.json`](./RAC.json) | rayline-cloud | anthropic (API key) | RAC |
| [`RAL.json`](./RAL.json) | rayline-cloud, `router: rayline-local` (model pinned) | anthropic (API key) | RAL |
| [`RLC.json`](./RLC.json) | rayline-cloud | ollama (local) | RLC |
| [`RLL.json`](./RLL.json) | rayline-cloud, `router: rayline-local` (model pinned) | ollama (local) | RLL |
| [`ARC.json`](./ARC.json) | subscription (passthrough) | rayline-cloud | ARC |
| [`ARCL.json`](./ARCL.json) | subscription (passthrough) | rayline-cloud (+ `local_models`) | ARCL § |
| [`ARL.json`](./ARL.json) | subscription (passthrough) | rayline-cloud, `router: rayline-local` (model pinned) | ARL |
| [`AL.json`](./AL.json) | subscription (passthrough) | ollama (local) | AL |
| [`LRC.json`](./LRC.json) | ollama (local) | rayline-cloud | LRC |
| [`LRCL.json`](./LRCL.json) | ollama (local) | rayline-cloud (+ `local_models` → subagents local on-device) | LRCL |
| [`LRL.json`](./LRL.json) | ollama (local) | rayline-cloud, `router: rayline-local` (model pinned) | LRL |
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
  "router": "rayline-cloud",              // rayline-cloud (default) | rayline-local (RRL)
  "local_models": ["qwen2.5-coder:7b"]    // non-empty ⇒ may-local ON (RRCL); must be served by a declared local endpoint
}
```

- **`router`** — which rayline decider runs. `rayline-cloud` (or absent) = the
  hosted RCR picks the model. `rayline-local` (RRL) = the **on-device LSR** is the
  router: it engages even when the endpoint is `rayline-cloud`, routing the class
  statically per the JSON and **pinning the route's `model`** (so the RCR doesn't
  pick). The `model` you set is sent as-is to the endpoint.
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
./examples/routing-modes/visual-demo.sh RRC                       # default mode + default prompt
./examples/routing-modes/visual-demo.sh ARCL                      # any supported mode from the table
./examples/routing-modes/visual-demo.sh ARCL "your prompt here"   # 2nd arg overrides the prompt
```

The **default prompt spawns one `Explore` subagent**, so subagent routing — and
may-local (e.g. `RRCL`/`ARCL`, where only `Explore` subagents go local) — is
actually visible in `rayline top`. Pass a 2nd arg to use your own prompt.

Requires `asciinema` and `tmux` (and a TTY — `asciinema rec`/`tmux attach` need
one). It forces `--via proxy` (so `rayline top` has metrics to show) and writes
`<MODE>-demo.cast`; play it with `asciinema play <MODE>-demo.cast`. To verify a
mode without recording, run the demo's core command directly:
`rayline claude --config <MODE>.json --via proxy -p "<prompt>"` and read
`rayline top --all`.

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

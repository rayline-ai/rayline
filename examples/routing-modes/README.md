# Routing-mode configs for `--config`

Each file here is a `RouterConfig` (`endpoints` + `routes`) you can drive with
any of these entry points (each is a column in the [Modes](#modes) table):

- **interactive:** `rayline claude --config ./examples/routing-modes/RLC.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/RLC.json`
  (then point an Anthropic SDK client at the proxy on `127.0.0.1:20810`)
- **Codex subscription:** `rayline codex --auth subscription --config ./examples/routing-modes/AL.json`
  materializes the same `subscription` sentinel into a Codex
  `client_bearer` endpoint, so Codex's ChatGPT subscription auth is reused for the
  main leg. Current Codex spawns subagents (observed: `collab_spawn`) and Rayline
  routes them via `routes.subagent`/`routes.subagents`, so a main≠subagent split
  is exercised — **subscription-main (`A*`) modes work fully (⁵), cloud-RCR-main
  (`R*`) modes route Codex natively to the hosted RCR — the `--config` codex
  materialization flips the hosted endpoint to `openai_responses` (bearer auth,
  `x-rayline-client: codex`), so a Codex `/v1/responses` main forwards natively and
  the RCR picks the model, no down-translation to Anthropic (✅) — and local-main
  (`L*`) modes route to the on-device model (🟡, ⁴)**.

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagent`) from one file — the thing the old `--router-config-path` /
`settings.json` surfaces could not express (they are subagent-only).

> **Scope.** The modes that ship a config file route end-to-end today — the
> per-entry-point **Claude / Codex / Router** columns below say where each works
> (see [What the columns mean](#what-the-columns-mean)) — including `RRCL`/`ARCL`
> (may-local from config) and `RRL` (on-device LSR static routing). The modes with
> no `.json` yet (the fully-❌ rows) are listed for completeness. The full design
> lives in the routing-modes design doc.

## Mode names

The mode name encodes the routing choices, left to right:

1. **agent** (main) provider — **R**ayline / **A**nthropic/OpenAI / **L**ocal
2. **subagent** provider — **R**ayline / **A**nthropic/OpenAI / **L**ocal

   The **A** class is your **first-party frontier subscription/provider** — it
   resolves to **Anthropic** under `rayline claude` and to **OpenAI (ChatGPT)**
   under `rayline codex`. The table columns show it as `anthropic/openai` for that
   reason. (On the *subagent* side it's the API-key variant — see †.)

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

The three support columns are the three entry points that drive a config:
**Claude** (`rayline claude --config`), **Codex** (`rayline codex --config`),
**Router** (`rayline router start --config`, then point an SDK client at the
proxy). Per column: ✅ = works end-to-end · 🟡 = routes correctly, capability-limited
by the local model (see ¹/⁴) · ❌ = not supported. See
[What the columns mean](#what-the-columns-mean).

| Mode | agent | subagent | router | local-model | Main agent → | Subagents → | Auth | Claude | Codex | Router | Config |
|---|---|---|---|---|---|---|---|:--:|:--:|:--:|---|
| **RRC** | `rayline` | `rayline` | rayline-cloud | off | cloud (RCR) | cloud (RCR) | rayline | ✅ | ✅ | ✅ | [`RRC.json`](./RRC.json) |
| **RRCL** § | `rayline` | `rayline` | rayline-cloud | on | cloud (RCR) § | cloud model (RCR may send a subagent → local) | rayline | ✅ | ✅ | ✅ | [`RRCL.json`](./RRCL.json) |
| **RRL** | `rayline` | `rayline` | rayline-local | N/A | cloud model (via local router) | cloud model (via local router) | rayline | ✅ | ✅ | ✅ | [`RRL.json`](./RRL.json) |
| **RAC** † | `rayline` | `anthropic/openai` | rayline-cloud | off | cloud (RCR) | Anthropic (API key) | rayline + Anthropic key | ✅ | ✅ | ✅ | [`RAC.json`](./RAC.json) |
| **RACL** ² | `rayline` | `anthropic/openai` | rayline-cloud | on | cloud model (RCR may send a agent → local) | Anthropic (API key) | rayline + Anthropic key | ❌ | ❌ | ❌ | — (may-local) |
| **RAL** † | `rayline` | `anthropic/openai` | rayline-local | N/A | cloud model (via local router) | Anthropic (API key) | rayline + Anthropic key | ✅ | ✅ | ✅ | [`RAL.json`](./RAL.json) |
| **RLC** | `rayline` | `local` | rayline-cloud | off | cloud (RCR) | local model | rayline | ✅ | ✅ | ✅ | [`RLC.json`](./RLC.json) |
| **RLCL** ² | `rayline` | `local` | rayline-cloud | on | cloud model (RCR may send a agent → local) | local model | rayline | ❌ | ❌ | ❌ | — (may-local) |
| **RLL** | `rayline` | `local` | rayline-local | N/A | cloud model (via local router) | local model | rayline | ✅ | ✅ | ✅ | [`RLL.json`](./RLL.json) |
| **ARC** | `anthropic/openai` | `rayline` | rayline-cloud | off | subscription (Claude / ChatGPT) | cloud (RCR) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`ARC.json`](./ARC.json) |
| **ARCL** § | `anthropic/openai` | `rayline` | rayline-cloud | on | subscription (Claude / ChatGPT) | cloud model (RCR may send a subagent → local) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`ARCL.json`](./ARCL.json) |
| **ARL** | `anthropic/openai` | `rayline` | rayline-local | N/A | subscription (Claude / ChatGPT) | cloud model (via local router) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`ARL.json`](./ARL.json) |
| **AL** | `anthropic/openai` | `local` | N/A | N/A | subscription (Claude / ChatGPT) | local model | subscription | ✅ | ✅ ⁵ | ✅ | [`AL.json`](./AL.json) |
| **LRC** ¹ | `local` | `rayline` | rayline-cloud | off | local model | cloud (RCR) | rayline | 🟡 | 🟡 ⁴ | 🟡 | [`LRC.json`](./LRC.json) |
| **LRCL** ³ | `local` | `rayline` | rayline-cloud | on | local model | cloud model (RCR may send a subagent → local) | rayline | ❌ | ❌ | ❌ | — (may-local) |
| **LRL** ¹ | `local` | `rayline` | rayline-local | N/A | local model | cloud model (via local router) | rayline | 🟡 | 🟡 ⁴ | 🟡 | [`LRL.json`](./LRL.json) |
| **LA** ¹ | `local` | `anthropic/openai` | N/A | N/A | local model | Anthropic (API key) | subscription / API key | 🟡 | 🟡 ⁴ | 🟡 | [`LA.json`](./LA.json) |
| **LL** ¹ | `local` | `local` | N/A | N/A | local model | local model | none | 🟡 | 🟡 ⁴ | 🟡 | [`LL.json`](./LL.json) |

Plus three granular **per-type** variants that split subagents by **type**
instead of one blanket default:

- [`RLC-per-type.json`](./RLC-per-type.json) — `RLC-per-type`: main cloud;
  `Explore`/`Plan` → distinct local models, everything else → cloud. Claude ✅ ·
  Router ✅ · Codex ✅ (cloud-RCR main, native Responses).
- [`AL-per-type.json`](./AL-per-type.json) — `AL-per-type`: main on your Claude
  **subscription**; only `Explore` → local, and **every other subagent passes
  through to the subscription** (no `routes.subagent` default). Claude ✅ · Router ✅
  · Codex ✅ (subscription main — ⁵; Codex's subagents (`collab_spawn`) route via
  `routes.subagent`/`routes.subagents` when their identifier matches a named
  entry, else pass through to the subscription).
  This is the selective counterpart of `AL` (which sends *all* subagents local):
  because subagents can't be *routed* to the subscription (the `†` rule), the
  non-local ones are left un-routed so they pass through with the main. Verified
  on-device: main + `general-purpose` → `target=anthropic` (subscription),
  `Explore` → `target=remote model=qwen2.5-coder:7b` (local).
- [`ARC-per-type.json`](./ARC-per-type.json) — `ARC-per-type`: main on your Claude
  **subscription**; only `Explore` → the **cloud router (RCR)**, every other
  subagent passes through to the subscription (no `routes.subagent` default).
  Claude ✅ · Router ✅ · Codex ✅ (subscription main — ⁵; Codex's subagents route
  via `routes.subagent`/`routes.subagents` per their identifier, else pass through
  to the subscription). Selective counterpart of `ARC` (which sends *all* subagents to the
  RCR) — same "un-routed ⇒ passthrough" trick as `AL-per-type`, pointed at
  `rayline-cloud` instead of local. Verified on-device: main + `general-purpose`
  → `target=anthropic` (subscription), `Explore` → the cloud router.

**† subscription on the subagent side is not expressible.** Subagents are the
*routed* class and the router cannot forward your Claude subscription OAuth, so
`RAC`/`RAL` ship the **Anthropic API-key** variant (`ANTHROPIC_API_KEY`) instead.
Swap in a subscription and only the subagent leg breaks; the intent columns show
what the mode *means*.

**¹ 🟡 — routes correctly, main only (today) — `agent = local` (`LRC`/`LRL`/`LA`/`LL`);
applies to the Claude *and* Router columns.** These run the **main** agent on a
local model, and it **works** for direct (non-subagent) work — but current small
local models (e.g. qwen 7B/9B) cannot reliably drive the harness's tool-use
protocol: they emit tool calls as plain text instead of invoking tools, so the main
agent **does not spawn subagents** (and rarely uses `Read`/`Edit`/`Bash`). The
subagent leg (cloud / pinned / Anthropic / local, per the mode) is therefore never
reached. This is a property of the local **model**, not the entry point: it happens
whether you launch via `rayline claude --config` **or** `rayline router start
--config` with an agent client attached (and equally with the existing
`rayline claude --local` / `--local --route all`). The **routing** itself is
verified for these configs (see [Tests](#tests)) — the router *would* route a
subagent-tagged request correctly; nothing generates one. A more capable local main
would spawn subagents and lift all four to ✅ on Claude/Router. The live e2e test
(`it_local_main_e2e`, `#[ignore]`d) is the harness for that.

**² ❌ N — may-local is inert (`RACL`/`RLCL`).** may-local (`CL`) only ever redirects
**cloud-routed `Explore` subagents** to local — never the main agent. In `RACL`/`RLCL`
the only `rayline-cloud` class is the **main** (the subagent is Anthropic in `RACL`,
already local in `RLCL`), so the `CL` lands on the main, where it has no effect — and
there is no cloud-routed subagent to carry it instead. The flag does nothing:
`RACL` ≡ `RAC` and `RLCL` ≡ `RLC`. They aren't shipped as distinct modes, to avoid
implying a behavior that doesn't exist. (Contrast `RRCL`/`ARCL`, whose subagent
**is** cloud-routed, so may-local applies and they ship — see §.)

**³ ❌ N — may-local handoff not built (`LRCL`).** Different reason from ²: here the
**main** is local, which engages the **on-device** router for the whole run. Making
the cloud subagent honor may-local would require the on-device LSR to advertise the
local model and **defer the decision back to the hosted RCR** (a 307 handoff) —
that path isn't implemented, so `LRCL` is not shipped as a distinct mode.

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
is exercised only by the ignored live test.

**⁴ 🟡 Codex, local main (`L*`: `LRC`/`LRL`/`LA`/`LL`) — capability-limited.** Codex
sends a **sentinel `--model`** (`rayline-local` by default) on every turn; the
`--config` codex materialization pins that sentinel to `routes.main` for **main**
turns (the same helper the subscription path uses), while the router skips that
model_route on **subagent** turns so subagent routing applies (Codex spawns
subagents via `collab_spawn`, routed through `routes.subagent`/`routes.subagents`).
For the local-main modes the sentinel routes to the config's on-device endpoint and
the local model answers — marked 🟡, not ✅, for the **same reason as the Claude
column (¹)**: whether a small local model can reliably drive Codex's agentic tool
loop is a model-capability question. Verified on-device: `rayline codex --config
LL.json` → `codex route endpoint:ollama requested=rayline-local selected=qwen3.5:9b`
→ reply returned.

The **cloud-main (`R*`) modes are plain ✅** — the codex `--config` materialization
flips the hosted RCR endpoint to `openai_responses` (bearer auth, `x-rayline-client:
codex`), so a Codex `/v1/responses` main forwards natively (`POST /v1/responses`)
and the RCR picks the model — no down-translation to Anthropic. The RCR reports the
chosen model back via `x-rayline-selected-model`, so `rayline top` shows the real
model. Only a user's own **custom** `anthropic_messages` endpoint is left untouched
(host-guarded to `api.rayline.ai` / `api-dev.rayline.ai`). Verified end-to-end
against prod (`api.rayline.ai`) and dev (`api-dev.rayline.ai`): `rayline codex
--config RRC.json` drives a real Codex session through the RCR (trivial prompt →
cheap tier, hard prompt → frontier — the RCR tiers per request).

**⁵ ✅ Codex — subscription main via `--auth subscription`.** The `A*` modes route
`routes.main` to the `subscription` sentinel, which `rayline codex --auth
subscription` materializes into a Codex `client_bearer` endpoint
(`chatgpt.com/backend-api/codex`) and pins the sentinel model to it for main
turns. Subagent turns skip that pin and follow `routes.subagent`/`routes.subagents`,
so `ARC`/`ARCL`/`ARL`/`AL` **can now diverge on the subagent leg** (main → your
ChatGPT subscription, subagents per config). Verified on-device: `rayline codex --auth
subscription --config AL.json` (and `ARC.json`) → `codex route
endpoint:codex-subscription requested=rayline-local selected=gpt-5.4` → reply
returned. Run it exactly as written — the default model routes correctly (the
per-config subscription materialization injects the `rayline-local`/`rayline-codex`
→ subscription model routes).

### What the columns mean

Each mode is scored against the **three entry points** that can drive its config:

- **Claude** — `rayline claude --config <mode>.json` (the full Claude Code agent,
  main + subagents).
- **Codex** — `rayline codex --config <mode>.json` (Codex CLI; `--auth
  subscription` for the `A*` modes). Current Codex spawns subagents
  (`collab_spawn`), which Rayline routes via `routes.subagent`/`routes.subagents`,
  so a main≠subagent split is exercised. See ⁴/⁵.
- **Router** — `rayline router start --config <mode>.json`, then point an Anthropic
  SDK client at the proxy. Pure routing engine; no Claude Code agent driving it.

The three share one routing engine, so they agree except where an entry point adds
a constraint the engine can't lift (Codex's sentinel-model rule; Claude's
local-main capability limit). Per-cell status:

- **✅** — works end-to-end. Every shipped config's routing is exercised by the
  hermetic tests below, and where a *capable* main drives the run the agent loop
  completes too. **Codex** is ✅ for the cloud-RCR mains (`R*`, native Responses)
  and the subscription mains (`A*`, ⁵). Modes with a
  cloud/capable main and `router: rayline-local` (`RRL`/`RAL`/`RLL`/`ARL`) are ✅ for
  Claude/Router: `router: rayline-local` is **static LSR routing** — the JSON is the
  decider, no ML policy needed. (`RRCL` is ✅ for the client/advertisement contract;
  its actual local redirect is hosted-gated — see §.)
- **🟡** — *routes correctly, capability-limited by the local model*. For **Claude
  and Router** it's `agent = local` (`LRC`/`LRL`/`LA`/`LL`): the local main runs and
  the router routes every class correctly (hermetic tests), but small local models
  can't drive the harness's `Task` tool, so **no subagents spawn** — regardless of
  `rayline claude` vs `rayline router start` (the limit is the local *model*, see ¹).
  For **Codex** it's the *same four local-main modes*: the sentinel now routes to the
  on-device model (⁴), but whether it can drive Codex's agentic tool loop is likewise
  a model-capability question.
- **❌** — not supported. For **Codex**, a mode that ships no config (the
  cloud-RCR-main (`R*`) and subscription-main (`A*`, ⁵) paths both route Codex). For
  **Claude/Router**, a `rayline`-only sub-axis isn't wired yet, for two reasons:
  - **may-local is inert** — the `CL` lands on the main agent (the only `rayline-cloud`
    class), where may-local never applies (`RACL` ≡ `RAC`, `RLCL` ≡ `RLC`) — see ².
    (`RRCL`/`ARCL` are cloud-routed on the subagent, so may-local applies and they ship.)
  - **may-local handoff not built** for a local main (`LRCL`) — see ³.

## Files ↔ modes

The supported modes ship as **18 config files** (the `❌` modes have none yet):

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
| [`LRL.json`](./LRL.json) | ollama (local) | rayline-cloud, `router: rayline-local` (model pinned) | LRL |
| [`LA.json`](./LA.json) | ollama (local) | anthropic (API key) | LA |
| [`LL.json`](./LL.json) | ollama (local) | ollama (local) | LL |
| [`RLC-per-type.json`](./RLC-per-type.json) | rayline-cloud | per-type: `Explore`/`Plan` → ollama, default → rayline-cloud | RLC\* |
| [`AL-per-type.json`](./AL-per-type.json) | subscription (passthrough) | per-type: `Explore` → ollama, all other subagents → subscription (passthrough) | AL\* |
| [`ARC-per-type.json`](./ARC-per-type.json) | subscription (passthrough) | per-type: `Explore` → rayline-cloud (RCR), all other subagents → subscription (passthrough) | ARC\* |

The proxy **scope** is derived from `routes.main`:

- `routes.main.endpoint == "subscription"` (a reserved sentinel) **or absent** →
  the main agent passes through to your own Claude subscription
  (selective-subagents scope). You do **not** declare `subscription` under
  `endpoints`.
- `routes.main` → any real endpoint → the main agent is routed (route-all scope).

## Config model — `endpoints` + `routes`

Real `EndpointConfig` fields only: `id`, `protocol`
(`anthropic_messages` | `openai_chat` | `openai_responses`), `base_url`,
`models`, `api_key_env`, `auth` (`api_key` | `bearer`), `headers`. A route's
`endpoint` is looked up by `id`; its `model` is rewritten into the request body.

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

### `rayline`-only route fields

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

[`visual-test.sh`](./visual-test.sh) records an asciinema cast of any mode in a
split-pane tmux session — the left pane runs `rayline claude --config <mode>` and
the right pane runs `rayline top`, so you can watch the routing live:

```bash
./examples/routing-modes/visual-test.sh RRC                       # default mode + default prompt
./examples/routing-modes/visual-test.sh ARCL                      # any supported mode from the table
./examples/routing-modes/visual-test.sh ARCL "your prompt here"   # 2nd arg overrides the prompt
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
(`LRC`/`LRL`/`LA`/`LL`, marked ¹) is **expected to fail** with current small local
models and is kept `#[ignore]`d in
`crates/rayline-cli/tests/it_local_main_e2e.rs`. Run it once a tool-capable local
main is configured:

```bash
CLAUDE_BIN=/path/to/claude RAYLINE_LOCAL_MAIN_E2E=1 \
  cargo test -p rayline-cli --test it_local_main_e2e -- --ignored --nocapture
```

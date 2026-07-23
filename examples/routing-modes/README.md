# Routing-mode configs for `--config`

Each file here is a `RouterConfig` (`endpoints` + `routes`) you can drive with
any of these entry points (each is a column in the [Modes](#modes) table):

- **interactive:** `rayline claude --config ./examples/routing-modes/Rc-L.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/Rc-L.json`
  (then point an Anthropic SDK client at the proxy on `127.0.0.1:20810`)
- **Codex subscription:** `rayline codex --auth subscription --config ./examples/routing-modes/S-L.json`
  materializes the same `subscription` sentinel into a Codex
  `client_bearer` endpoint, so Codex's ChatGPT subscription auth is reused for the
  main leg. Current Codex spawns subagents (observed: `collab_spawn`) and Rayline
  routes them via `routes.subagent`/`routes.subagents`, so a main≠subagent split
  is exercised — **subscription-main (`S-*`) modes work fully (⁵), cloud-RCR-main
  (`Rc*`/`Rcl*`/`Rl*`) modes route Codex natively to the hosted RCR — the `--config` codex
  materialization flips the hosted endpoint to `openai_responses` (bearer auth,
  `x-rayline-client: codex`), so a Codex `/v1/responses` main forwards natively and
  the RCR picks the model, no down-translation to Anthropic (✅) — and local-main
  (`L-*`) modes route to the on-device model (🟡, ⁴)**.

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagent`) from one file — the thing the old `--router-config-path` /
`settings.json` surfaces could not express (they are subagent-only).

`routes.subagent` is **optional**: with only `routes.main`, subagents inherit the
main route, so a one-model config needs a single entry (see
[`K-K.json`](./K-K.json), the single-endpoint mode). The split modes below spell
both out precisely because main and subagents differ.

> **Scope.** The modes that ship a config file route end-to-end today — the
> per-entry-point **Claude / Codex / Router** columns below say where each works
> (see [What the columns mean](#what-the-columns-mean)) — including `Rcl-Rcl`/`S-Rcl`
> (may-local from config) and `Rl-Rl` (on-device LSR static routing). The modes with
> no `.json` yet (the fully-❌ rows) are listed for completeness. The full design
> lives in the routing-modes design doc.

## Mode names

The mode name is two tokens joined by a dash — **`main-subagent`** — naming the
routing class for the **main** agent (left of the dash) and its **subagents**
(right), e.g. `Rc-L` (cloud-router main, local subagents) or `S-Rc` (subscription
main, cloud-router subagents).

Each token is one of four provider classes:

- **S — Subscription** (first-party frontier OAuth): resolves to **Anthropic
  (Claude)** under `rayline claude` and to **OpenAI (ChatGPT)** under
  `rayline codex`. Uses your interactive login. **Main leg only** — a subagent can't
  be a subscription (see †).
- **K — Keyed** provider API: the same first-party providers (Anthropic / OpenAI)
  **or any OpenAI-compatible endpoint** (e.g. OpenRouter), authenticated by an
  **API key** (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, …) rather than a
  subscription.
- **R — Rayline**: rayline is a routing *system*, not a single destination, so this
  class always carries an **engine** suffix glued onto the `R` — `Rc`, `Rcl`, or
  `Rl` (below). `R` never appears bare.
- **L — Local** on-device model (your ollama).

**S and K are the same providers split by auth mechanism** — S is your subscription
OAuth, K is an API key. The split is not cosmetic: the two legs differ. A **main**
agent can pass through your subscription (`S`), but a **subagent** is the *routed*
class and the router cannot forward subscription OAuth, so a routed first-party
subagent is always **`K`** (API key). There is therefore **no `-S` subagent form** —
see †.

### The `R` engine suffix

`R` is glued to one of three engine tokens, because a `rayline` class can route
three ways:

- **Rc** — `router: rayline-cloud`, **local-model off**: the hosted **RCR**
  (intelligent ML pick) serves a **cloud** model only.
- **Rcl** — `router: rayline-cloud`, **local-model on** (may-local): the RCR may
  **redirect** the class to a local model. Today may-local only fires for
  exploration subagents (`Explore`) — the main agent is always cloud.
- **Rl** — `router: rayline-local`: the on-device **LSR** is the router — it routes
  the class **statically per the JSON** and pins the route's `model`, instead of the
  hosted RCR deciding (even when the endpoint is `rayline-cloud`).

Only the **R** class takes a suffix; **S**, **K**, **L** are fixed destinations —
`router` and `local-model` are `N/A` for them.

### `rayline` is both a router *and* a provider

**S**, **K**, and **L** are fixed **destinations** — the provider *is* the
endpoint, so there is nothing more to decide. **`rayline` is different: it is a
routing *system*, not a destination.** Choosing `rayline` (`R`) for a class
therefore opens **two independent sub-axes that apply only to `rayline`** — which is
exactly why the `router` and `local-model` columns exist and are `N/A` for
`S` / `K` / `L`:

- **`router`** — *which rayline decider runs*: `rayline-cloud` = the hosted
  **RCR** (intelligent ML pick) vs `rayline-local` = the on-device **LSR** (your
  static rules). Two genuinely different deciders. The `rayline-` prefix keeps the
  *engine* distinct from the `local` **provider** — `router: rayline-local`
  is not the same thing as `subagent: local` (the `L` class).
- **`local-model`** — a **sub-knob of `router: rayline-cloud`**: may the cloud RCR
  **redirect** that class to a local model (`on`) or stay cloud-only (`off`).
  **Today may-local only takes effect for exploration subagents (`Explore`) — the
  main agent is always cloud** (a config-declared local model is a *custom*
  endpoint, which the RCR delegates exploration-only). **`N/A` when
  `router: rayline-local`** and for `S`/`K`/`L`.

The two sub-axes **nest** — `rayline` → `router` (`rayline-cloud`|`rayline-local`) →
*only under `rayline-cloud`* → `local-model` (`on`|`off`) — so a `rayline` class has
**three** distinct behaviours (the engine token `Rc`/`Rcl`/`Rl`), not four:

| `router` | `local-model` | a `rayline` class is then… | token |
|---|---|---|---|
| rayline-cloud | off | RCR serves a **cloud** model only | `Rc` |
| rayline-cloud | on | RCR may **redirect to a local model** (may-local) — today **`Explore` subagents only; main stays cloud** | `Rcl` |
| rayline-local | — (N/A) | the on-device **LSR routes it itself** | `Rl` |

## Modes

The three support columns are the three entry points that drive a config:
**Claude** (`rayline claude --config`), **Codex** (`rayline codex --config`),
**Router** (`rayline router start --config`, then point an SDK client at the
proxy). Per column: ✅ = works end-to-end · 🟡 = routes correctly, capability-limited
by the local model (see ¹/⁴) · ❌ = not supported. See
[What the columns mean](#what-the-columns-mean).

| Mode | agent | subagent | router | local-model | Main agent → | Subagents → | Auth | Claude | Codex | Router | Config |
|---|---|---|---|---|---|---|---|:--:|:--:|:--:|---|
| **Rc-Rc** | `rayline` | `rayline` | rayline-cloud | off | cloud (RCR) | cloud (RCR) | rayline | ✅ | ✅ | ✅ | [`Rc-Rc.json`](./Rc-Rc.json) |
| **Rcl-Rcl** § | `rayline` | `rayline` | rayline-cloud | on | cloud (RCR) § | cloud model (RCR may send a subagent → local) | rayline | ✅ | ✅ | ✅ | [`Rcl-Rcl.json`](./Rcl-Rcl.json) |
| **Rl-Rl** | `rayline` | `rayline` | rayline-local | N/A | cloud model (via local router) | cloud model (via local router) | rayline | ✅ | ✅ | ✅ | [`Rl-Rl.json`](./Rl-Rl.json) |
| **Rc-K** † | `rayline` | `keyed` | rayline-cloud | off | cloud (RCR) | Anthropic (API key) | rayline + Anthropic key | ✅ | ✅ | ✅ | [`Rc-K.json`](./Rc-K.json) |
| **Rcl-K** ² | `rayline` | `keyed` | rayline-cloud | on | cloud model (RCR may send an agent → local) | Anthropic (API key) | rayline + Anthropic key | ❌ | ❌ | ❌ | — (may-local) |
| **Rl-K** † | `rayline` | `keyed` | rayline-local | N/A | cloud model (via local router) | Anthropic (API key) | rayline + Anthropic key | ✅ | ✅ | ✅ | [`Rl-K.json`](./Rl-K.json) |
| **Rc-L** | `rayline` | `local` | rayline-cloud | off | cloud (RCR) | local model | rayline | ✅ | ✅ | ✅ | [`Rc-L.json`](./Rc-L.json) |
| **Rcl-L** ² | `rayline` | `local` | rayline-cloud | on | cloud model (RCR may send an agent → local) | local model | rayline | ❌ | ❌ | ❌ | — (may-local) |
| **Rl-L** | `rayline` | `local` | rayline-local | N/A | cloud model (via local router) | local model | rayline | ✅ | ✅ | ✅ | [`Rl-L.json`](./Rl-L.json) |
| **S-Rc** | `subscription` | `rayline` | rayline-cloud | off | subscription (Claude / ChatGPT) | cloud (RCR) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`S-Rc.json`](./S-Rc.json) |
| **S-Rcl** § | `subscription` | `rayline` | rayline-cloud | on | subscription (Claude / ChatGPT) | cloud model (RCR may send a subagent → local) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`S-Rcl.json`](./S-Rcl.json) |
| **S-Rl** | `subscription` | `rayline` | rayline-local | N/A | subscription (Claude / ChatGPT) | cloud model (via local router) | subscription + rayline | ✅ | ✅ ⁵ | ✅ | [`S-Rl.json`](./S-Rl.json) |
| **S-L** | `subscription` | `local` | N/A | N/A | subscription (Claude / ChatGPT) | local model | subscription | ✅ | ✅ ⁵ | ✅ | [`S-L.json`](./S-L.json) |
| **K-K** ⁶ | `keyed` | `keyed` | N/A | N/A | keyed provider (API key) | inherits main (keyed) | provider API key | ✅ | ✅ ⁶ | ✅ | [`K-K.json`](./K-K.json) |
| **L-Rc** ¹ | `local` | `rayline` | rayline-cloud | off | local model | cloud (RCR) | rayline | 🟡 | 🟡 ⁴ | 🟡 | [`L-Rc.json`](./L-Rc.json) |
| **L-Rcl** ³ | `local` | `rayline` | rayline-cloud | on | local model | cloud model (RCR may send a subagent → local) | rayline | ❌ | ❌ | ❌ | — (may-local) |
| **L-Rl** ¹ | `local` | `rayline` | rayline-local | N/A | local model | cloud model (via local router) | rayline | 🟡 | 🟡 ⁴ | 🟡 | [`L-Rl.json`](./L-Rl.json) |
| **L-K** ¹ | `local` | `keyed` | N/A | N/A | local model | Anthropic (API key) | API key | 🟡 | 🟡 ⁴ | 🟡 | [`L-K.json`](./L-K.json) |
| **L-L** ¹ | `local` | `local` | N/A | N/A | local model | local model | none | 🟡 | 🟡 ⁴ | 🟡 | [`L-L.json`](./L-L.json) |

Plus three granular **per-type** variants that split subagents by **type**
instead of one blanket default:

- [`Rc-L-per-type.json`](./Rc-L-per-type.json) — `Rc-L-per-type`: main cloud;
  `Explore`/`Plan` → distinct local models, everything else → cloud. Claude ✅ ·
  Router ✅ · Codex ✅ (cloud-RCR main, native Responses).
- [`S-L-per-type.json`](./S-L-per-type.json) — `S-L-per-type`: main on your Claude
  **subscription**; only `Explore` → local, and **every other subagent passes
  through to the subscription** (no `routes.subagent` default). Claude ✅ · Router ✅
  · Codex ✅ (subscription main — ⁵; Codex's subagents (`collab_spawn`) route via
  `routes.subagent`/`routes.subagents` when their identifier matches a named
  entry, else pass through to the subscription).
  This is the selective counterpart of `S-L` (which sends *all* subagents local):
  because subagents can't be *routed* to the subscription (the `†` rule), the
  non-local ones are left un-routed so they pass through with the main. Verified
  on-device: main + `general-purpose` → `target=anthropic` (subscription),
  `Explore` → `target=remote model=qwen2.5-coder:7b` (local).
- [`S-Rc-per-type.json`](./S-Rc-per-type.json) — `S-Rc-per-type`: main on your Claude
  **subscription**; only `Explore` → the **cloud router (RCR)**, every other
  subagent passes through to the subscription (no `routes.subagent` default).
  Claude ✅ · Router ✅ · Codex ✅ (subscription main — ⁵; Codex's subagents route
  via `routes.subagent`/`routes.subagents` per their identifier, else pass through
  to the subscription). Selective counterpart of `S-Rc` (which sends *all* subagents to the
  RCR) — same "un-routed ⇒ passthrough" trick as `S-L-per-type`, pointed at
  `rayline-cloud` instead of local. Verified on-device: main + `general-purpose`
  → `target=anthropic` (subscription), `Explore` → the cloud router.

**† there is no `-S` (subscription subagent) form.** Subagents are the *routed*
class and the router cannot forward your Claude/ChatGPT subscription OAuth, so a
routed first-party subagent authenticates by **API key** — the **`-K`** form
(`Rc-K`/`Rl-K`, and `L-K`, all `ANTHROPIC_API_KEY`). Swap a subscription in on the
subagent leg and only that leg breaks; use `-K` there instead. (`S` still works on
the **main** leg, which passes through rather than being routed.) The intent columns
show what the mode *means*.

**¹ 🟡 — routes correctly, main only (today) — `agent = local` (`L-Rc`/`L-Rl`/`L-K`/`L-L`);
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

**² ❌ N — may-local is inert (`Rcl-K`/`Rcl-L`).** may-local (`Rcl`) only ever redirects
**cloud-routed `Explore` subagents** to local — never the main agent. In `Rcl-K`/`Rcl-L`
the only `rayline-cloud` class is the **main** (the subagent is Anthropic in `Rcl-K`,
already local in `Rcl-L`), so the `Rcl` engine lands on the main, where it has no effect — and
there is no cloud-routed subagent to carry it instead. The flag does nothing:
`Rcl-K` ≡ `Rc-K` and `Rcl-L` ≡ `Rc-L`. They aren't shipped as distinct modes, to avoid
implying a behavior that doesn't exist. (Contrast `Rcl-Rcl`/`S-Rcl`, whose subagent
**is** cloud-routed, so may-local applies and they ship — see §.)

**³ ❌ N — may-local handoff not built (`L-Rcl`).** Different reason from ²: here the
**main** is local, which engages the **on-device** router for the whole run. Making
the cloud subagent honor may-local would require the on-device LSR to advertise the
local model and **defer the decision back to the hosted RCR** (a 307 handoff) —
that path isn't implemented, so `L-Rcl` is not shipped as a distinct mode.

**§ may-local from config — custom-endpoint scope + hosted decision.**
`Rcl-Rcl` wires the **client** contract from config: a `rayline-cloud` route carrying
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
  account flag off) `Rcl-Rcl` behaves like `Rc-Rc`.

Verified on-device: `rayline claude --config Rcl-Rcl.json` with an `Explore`-spawning
task → adapter forwards to `http://<endpoint>/v1/messages` and `rayline top` shows
`model=<local>, target=local, agent_type=Explore` while main turns stay cloud.
**`S-Rcl`** works the same way (main passes through to the subscription; the cloud
subagent advertises may-local) and is likewise verified on-device. The
advertisement + redirect *plumbing* is hermetically tested; the end-to-end redirect
is exercised only by the ignored live test.

**⁴ 🟡 Codex, local main (`L-*`: `L-Rc`/`L-Rl`/`L-K`/`L-L`) — capability-limited.** Codex
sends a **sentinel `--model`** (`rayline-local` by default) on every turn; the
`--config` codex materialization pins that sentinel to `routes.main` for **main**
turns (the same helper the subscription path uses), while the router skips that
model_route on **subagent** turns so subagent routing applies (Codex spawns
subagents via `collab_spawn`, routed through `routes.subagent`/`routes.subagents`).
For the local-main modes the sentinel routes to the config's on-device endpoint and
the local model answers — marked 🟡, not ✅, for the **same reason as the Claude
column (¹)**: whether a small local model can reliably drive Codex's agentic tool
loop is a model-capability question. Verified on-device: `rayline codex --config
L-L.json` → `codex route endpoint:ollama requested=rayline-local selected=qwen3.5:9b`
→ reply returned.

The **cloud-main (`Rc*`/`Rcl*`/`Rl*`) modes are plain ✅** — the codex `--config` materialization
flips the hosted RCR endpoint to `openai_responses` (bearer auth, `x-rayline-client:
codex`), so a Codex `/v1/responses` main forwards natively (`POST /v1/responses`)
and the RCR picks the model — no down-translation to Anthropic. The RCR reports the
chosen model back via `x-rayline-selected-model`, so `rayline top` shows the real
model. Only a user's own **custom** `anthropic_messages` endpoint is left untouched
(host-guarded to `api.rayline.ai` / `api-dev.rayline.ai`). Verified end-to-end
against prod (`api.rayline.ai`) and dev (`api-dev.rayline.ai`): `rayline codex
--config Rc-Rc.json` drives a real Codex session through the RCR (trivial prompt →
cheap tier, hard prompt → frontier — the RCR tiers per request).

**⁵ ✅ Codex — subscription main via `--auth subscription`.** The `S-*` modes route
`routes.main` to the `subscription` sentinel, which `rayline codex --auth
subscription` materializes into a Codex `client_bearer` endpoint
(`chatgpt.com/backend-api/codex`) and pins the sentinel model to it for main
turns. Subagent turns skip that pin and follow `routes.subagent`/`routes.subagents`,
so `S-Rc`/`S-Rcl`/`S-Rl`/`S-L` **can now diverge on the subagent leg** (main → your
ChatGPT subscription, subagents per config). Verified on-device: `rayline codex --auth
subscription --config S-L.json` (and `S-Rc.json`) → `codex route
endpoint:codex-subscription requested=rayline-local selected=gpt-5.4` → reply
returned. Run it exactly as written — the default model routes correctly (the
per-config subscription materialization injects the `rayline-local`/`rayline-codex`
→ subscription model routes).

**⁶ K-K — single keyed endpoint (subagents inherit main).** `K-K` declares one
**keyed** provider endpoint (the shipped config uses OpenRouter + `moonshotai/kimi-k2.6`,
`OPENROUTER_API_KEY`) and only `routes.main`; with no `routes.subagent`, **subagents
inherit the main route**, so both legs land on the same endpoint + model. It is the
minimal one-model config — swap the endpoint/`models` for any OpenAI-compatible or
Anthropic-API-key provider. On **Codex**, a keyed `anthropic_messages` main is served
via the standard down-translation path (Responses → Anthropic Messages → provider),
not the RCR-native Responses path reserved for the hosted router (⁴).

### What the columns mean

Each mode is scored against the **three entry points** that can drive its config:

- **Claude** — `rayline claude --config <mode>.json` (the full Claude Code agent,
  main + subagents).
- **Codex** — `rayline codex --config <mode>.json` (Codex CLI; `--auth
  subscription` for the `S-*` modes). Current Codex spawns subagents
  (`collab_spawn`), which Rayline routes via `routes.subagent`/`routes.subagents`,
  so a main≠subagent split is exercised. See ⁴/⁵.
- **Router** — `rayline router start --config <mode>.json`, then point an Anthropic
  SDK client at the proxy. Pure routing engine; no Claude Code agent driving it.

The three share one routing engine, so they agree except where an entry point adds
a constraint the engine can't lift (Codex's sentinel-model rule; Claude's
local-main capability limit). Per-cell status:

- **✅** — works end-to-end. Every shipped config's routing is exercised by the
  hermetic tests below, and where a *capable* main drives the run the agent loop
  completes too. **Codex** is ✅ for the cloud-RCR mains (`Rc*`/`Rcl*`/`Rl*`, native Responses)
  and the subscription mains (`S-*`, ⁵). Modes with a
  cloud/capable main and `router: rayline-local` (`Rl-Rl`/`Rl-K`/`Rl-L`/`S-Rl`) are ✅ for
  Claude/Router: `router: rayline-local` is **static LSR routing** — the JSON is the
  decider, no ML policy needed. (`Rcl-Rcl` is ✅ for the client/advertisement contract;
  its actual local redirect is hosted-gated — see §.)
- **🟡** — *routes correctly, capability-limited by the local model*. For **Claude
  and Router** it's `agent = local` (`L-Rc`/`L-Rl`/`L-K`/`L-L`): the local main runs and
  the router routes every class correctly (hermetic tests), but small local models
  can't drive the harness's `Task` tool, so **no subagents spawn** — regardless of
  `rayline claude` vs `rayline router start` (the limit is the local *model*, see ¹).
  For **Codex** it's the *same four local-main modes*: the sentinel now routes to the
  on-device model (⁴), but whether it can drive Codex's agentic tool loop is likewise
  a model-capability question.
- **❌** — not supported. For **Codex**, a mode that ships no config (the
  cloud-RCR-main (`Rc*`/`Rcl*`/`Rl*`) and subscription-main (`S-*`, ⁵) paths both route Codex). For
  **Claude/Router**, a `rayline`-only sub-axis isn't wired yet, for two reasons:
  - **may-local is inert** — the `Rcl` engine lands on the main agent (the only `rayline-cloud`
    class), where may-local never applies (`Rcl-K` ≡ `Rc-K`, `Rcl-L` ≡ `Rc-L`) — see ².
    (`Rcl-Rcl`/`S-Rcl` are cloud-routed on the subagent, so may-local applies and they ship.)
  - **may-local handoff not built** for a local main (`L-Rcl`) — see ³.

## Files ↔ modes

The supported modes ship as **19 config files** (the `❌` modes have none yet):

| File | `routes.main` → | `routes.subagent` → | Mode |
|---|---|---|---|
| [`Rc-Rc.json`](./Rc-Rc.json) | rayline-cloud | rayline-cloud | Rc-Rc |
| [`Rcl-Rcl.json`](./Rcl-Rcl.json) | rayline-cloud (+ `local_models`) | rayline-cloud (+ `local_models`) | Rcl-Rcl § |
| [`Rl-Rl.json`](./Rl-Rl.json) | rayline-cloud, `router: rayline-local` (model pinned) | rayline-cloud, `router: rayline-local` (default + `Explore` per-type) | Rl-Rl |
| [`Rc-K.json`](./Rc-K.json) | rayline-cloud | anthropic (API key) | Rc-K |
| [`Rl-K.json`](./Rl-K.json) | rayline-cloud, `router: rayline-local` (model pinned) | anthropic (API key) | Rl-K |
| [`Rc-L.json`](./Rc-L.json) | rayline-cloud | ollama (local) | Rc-L |
| [`Rl-L.json`](./Rl-L.json) | rayline-cloud, `router: rayline-local` (model pinned) | ollama (local) | Rl-L |
| [`S-Rc.json`](./S-Rc.json) | subscription (passthrough) | rayline-cloud | S-Rc |
| [`S-Rcl.json`](./S-Rcl.json) | subscription (passthrough) | rayline-cloud (+ `local_models`) | S-Rcl § |
| [`S-Rl.json`](./S-Rl.json) | subscription (passthrough) | rayline-cloud, `router: rayline-local` (model pinned) | S-Rl |
| [`S-L.json`](./S-L.json) | subscription (passthrough) | ollama (local) | S-L |
| [`K-K.json`](./K-K.json) | openrouter (keyed, kimi-k2.6) | inherits main (no `routes.subagent`) | K-K ⁶ |
| [`L-Rc.json`](./L-Rc.json) | ollama (local) | rayline-cloud | L-Rc |
| [`L-Rl.json`](./L-Rl.json) | ollama (local) | rayline-cloud, `router: rayline-local` (model pinned) | L-Rl |
| [`L-K.json`](./L-K.json) | ollama (local) | anthropic (API key) | L-K |
| [`L-L.json`](./L-L.json) | ollama (local) | ollama (local) | L-L |
| [`Rc-L-per-type.json`](./Rc-L-per-type.json) | rayline-cloud | per-type: `Explore`/`Plan` → ollama, default → rayline-cloud | Rc-L\* |
| [`S-L-per-type.json`](./S-L-per-type.json) | subscription (passthrough) | per-type: `Explore` → ollama, all other subagents → subscription (passthrough) | S-L\* |
| [`S-Rc-per-type.json`](./S-Rc-per-type.json) | subscription (passthrough) | per-type: `Explore` → rayline-cloud (RCR), all other subagents → subscription (passthrough) | S-Rc\* |

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
  "router": "rayline-cloud",              // rayline-cloud (default) | rayline-local (Rl)
  "local_models": ["qwen2.5-coder:7b"]    // non-empty ⇒ may-local ON (Rcl); must be served by a declared local endpoint
}
```

- **`router`** — which rayline decider runs. `rayline-cloud` (or absent) = the
  hosted RCR picks the model. `rayline-local` (`Rl`) = the **on-device LSR** is the
  router: it engages even when the endpoint is `rayline-cloud`, routing the class
  statically per the JSON and **pinning the route's `model`** (so the RCR doesn't
  pick). The `model` you set is sent as-is to the endpoint.
- **`local_models`** — the model ids the cloud RCR may redirect this class to
  (may-local, `Rcl`). A non-empty list turns may-local **on** and advertises
  `local_models[0]`; the id must appear in a declared local endpoint's `models`
  (that endpoint's `base_url` is the redirect target). Both fields are ignored for
  `S`/`K`/`L` endpoints and do not change the local router's own routing.

## Auth

- `rayline-cloud` reads `RAYLINE_ROUTER_API_KEY` (an `rlk-` key). For
  `rayline claude`, your `rayline auth login` session key is injected
  automatically, so the env var is optional in interactive use.
- `keyed` (`K`) reads the provider's API key from the endpoint's `api_key_env`
  (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, …) — the local router cannot use a
  subscription (see †).
- `ollama` needs no key (point `base_url` at your server).

## Visual demo

[`visual-test.sh`](./visual-test.sh) records an asciinema cast of any mode in a
split-pane tmux session — the left pane runs `rayline claude --config <mode>` and
the right pane runs `rayline top`, so you can watch the routing live:

```bash
./examples/routing-modes/visual-test.sh Rc-Rc                       # default mode + default prompt
./examples/routing-modes/visual-test.sh S-Rcl                       # any supported mode from the table
./examples/routing-modes/visual-test.sh S-Rcl "your prompt here"    # 2nd arg overrides the prompt
```

The **default prompt spawns one `Explore` subagent**, so subagent routing — and
may-local (e.g. `Rcl-Rcl`/`S-Rcl`, where only `Explore` subagents go local) — is
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

The selective-main-subscription passthrough (`S-Rc`/`S-L` main) is a proxy-layer
behavior, covered in `crates/rayline-proxy`.

**may-local — `Rcl-Rcl` (§).** The config→advertisement mapping is unit-tested in
`rayline-cli` (`router_config::tests::may_local_*` and `rrcl_example_resolves_may_local`):
a `rayline-cloud` route with `local_models` resolves to the advertised model + the
local endpoint's upstream URL, and `rayline-local`/no-`local_models` routes resolve
to none. `config_mode_examples_route_main_and_subagents` also asserts the
`router`/`local_models` fields **do not** change the LSR's routing (Rcl-Rcl still
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
(`L-Rc`/`L-Rl`/`L-K`/`L-L`, marked ¹) is **expected to fail** with current small local
models and is kept `#[ignore]`d in
`crates/rayline-cli/tests/it_local_main_e2e.rs`. Run it once a tool-capable local
main is configured:

```bash
CLAUDE_BIN=/path/to/claude RAYLINE_LOCAL_MAIN_E2E=1 \
  cargo test -p rayline-cli --test it_local_main_e2e -- --ignored --nocapture
```

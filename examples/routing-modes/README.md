# Routing-mode configs for `--config`

Each file is a `RouterConfig` (`endpoints` + `routes`) you can drive with either
entry point:

- **interactive:** `rayline claude --config ./examples/routing-modes/RL.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/RL.json`
  (then point an SDK at the proxy on `127.0.0.1:20810`)

`--config` drives **both** the main agent (`routes.main`) and subagents. The
subagent default is **`routes.subagent`** (singular); **`routes.subagents`** (the
map) is only for per-type overrides (see `RL-per-type.json`). The proxy scope is
derived from `routes.main`:

- `routes.main.endpoint == "subscription"` (reserved) **or absent** → the main
  agent passes through to your own Claude subscription (proxy selective-subagents).
- `routes.main` → any real endpoint → the main agent is routed (route-all).

`subscription` is a reserved sentinel, never a real endpoint, so you do not
declare it under `endpoints`.

## Files ↔ modes (see rayline-routing.html)

Naming = `--agent` letter + `--subagent` letter; the `1`/`2` suffix (local-model
off/on) is dropped because it's a runtime cloud-router decision, not a config
difference.

| file | `routes.main` → | `routes.subagent` → | covers | status |
|---|---|---|---|---|
| `RR.json` | rayline-cloud | rayline-cloud | RR1, RR2 | ✅ works as-is |
| `RA.json` | rayline-cloud | anthropic (API key) | RA1, RA2 | ⚠ subscription not expressible |
| `RL.json` | rayline-cloud | ollama (local) | RL1, RL2 | ⚙ needs local model |
| `AR.json` | subscription (passthrough) | rayline-cloud | AR1, AR2 | ✅ works as-is |
| `AL.json` | subscription (passthrough) | ollama (local) | AL | ✅ works as-is |
| `LR.json` | ollama (local) | rayline-cloud | LR1, LR2 | ⚙ needs local model |
| `LA.json` | ollama (local) | anthropic (API key) | LA | ⚠ subscription not expressible |
| `LL.json` | ollama (local) | ollama (local) | LL | ✅ fully on-device |

**`RL-per-type.json`** — same as `RL.json` but splits subagents by type
(`Explore`/`Plan` → local, everything else → cloud). A granular variant of RL, not
a distinct mode.

Notes:
- **`⚠ subscription not expressible`** (RA/LA): the subagent-on-subscription
  intent is structurally inexpressible — subagents are routed and the router can't
  forward your Claude subscription OAuth — so these ship the **API-key** variant
  (`ANTHROPIC_API_KEY`).
- **may-local** (the `RR2`/`AR2`/`RL2`/`LR2` "on" variants) is an intelligent
  decision the **rayline cloud router** makes at runtime; the static config can't
  distinguish it from the `off` sibling, so each file covers both.

## Auth

- `rayline-cloud` reads `RAYLINE_ROUTER_API_KEY` (or, for `rayline claude`, the
  `rayline auth login` key is injected automatically).
- `anthropic` reads `ANTHROPIC_API_KEY`.
- `ollama` needs no key (point `base_url` at your server).

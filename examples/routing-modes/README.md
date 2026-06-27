# Routing-mode configs for `--config`

Each file is a `RouterConfig` (`endpoints` + `routes`) you can drive with either
entry point:

- **interactive:** `rayline claude --config ./examples/routing-modes/RL.json`
- **headless / agents:** `rayline router start --config ./examples/routing-modes/RL.json`
  (then point an SDK at the proxy on `127.0.0.1:20810`)

`--config` drives **both** the main agent (`routes.main`) and subagents
(`routes.subagents`, per type). The proxy scope is derived from `routes.main`:

- `routes.main.endpoint == "subscription"` (reserved) **or absent** → the main
  agent passes through to your own Claude subscription (proxy selective-subagents).
- `routes.main` → any real endpoint → the main agent is routed (route-all).

`subscription` is a reserved endpoint id: it is a passthrough marker, never a real
endpoint, so you do not declare it under `endpoints`.

## Files ↔ modes (see rayline-routing.html)

| file | main → | subagents → | modes covered |
|---|---|---|---|
| `RR.json` | rayline cloud | rayline cloud | RR1, RR2 |
| `RL.json` | rayline cloud | local (ollama) | RL1, RL2 |
| `RL-per-type.json` | rayline cloud | per-type: Explore/Plan → local, rest → cloud | RL1 (granular) |
| `RA.json` | rayline cloud | Anthropic (API key) | RA1, RA2 (API-key variant) |
| `AR.json` | subscription (passthrough) | rayline cloud | AR1, AR2 |
| `AL.json` | subscription (passthrough) | local (ollama) | AL |
| `LR.json` | local (ollama) | rayline cloud | LR1, LR2 |
| `LL.json` | local (ollama) | local (ollama) | LL |
| `LA.json` | local (ollama) | Anthropic (API key) | LA (API-key variant) |

Note: the `--local-model` "may-local" variants (RR2/AR2/RL2/LR2) are an
intelligent-routing decision the **rayline cloud router** makes at runtime; the
static config cannot distinguish them from their `off` siblings, so each file
covers both. The `…(sub)` Anthropic-subscription pathological modes can't be
expressed (subagents are routed; the router can't forward OAuth) — `RA.json` /
`LA.json` use an Anthropic **API key** instead.

## Auth

- `rayline-cloud` reads `RAYLINE_ROUTER_API_KEY` (or, for `rayline claude`, the
  `rayline auth login` key is injected automatically).
- `anthropic` reads `ANTHROPIC_API_KEY`.
- `ollama` needs no key (point `base_url` at your server).

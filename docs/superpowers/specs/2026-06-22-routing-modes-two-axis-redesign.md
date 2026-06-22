# Routing modes: two-axis redesign

**Date:** 2026-06-22
**Status:** Proposed (design)

## Problem

The user-facing routing model is expressed as a single enum,
`RoutingMode { Override, Proxy, ProxySubagents }`
(`crates/rayline-cli/src/claude.rs:108-113`), but it actually encodes **two
independent decisions** jammed into one list, with a third decision (cloud vs
local router) carried on a *separate* flag. This produces several concrete
defects:

1. **The literal default is dead.** The parser defaults `routing_mode` to
   `RoutingMode::Proxy` (`crates/rayline-cli/src/lib.rs:843`), then dispatch
   silently rewrites it to `ProxySubagents` whenever the flag was not explicit
   (`lib.rs:317-320`, via `routing_mode_explicit` at `lib.rs:1373`). A reader
   trusting line 843 is simply wrong about the effective default.

2. **Two spellings for one state.** `--no-proxy` (`lib.rs:941-944`) and
   `--routing-mode override` (`lib.rs:1001`) both set `RoutingMode::Override`.
   `routing_mode_explicit` then has to special-case both (`lib.rs:1380`).

3. **A misleading translation.** `proxy_routing_mode_name`
   (`claude.rs:2147-2152`) maps `RoutingMode::Override` to the proxy wire value
   `"all"` even though `Override` starts **no proxy at all**. It is only safe
   because dispatch branches on `Override` earlier (`claude.rs:427-440`), but
   the mapping reads as a lie.

4. **The router axis is detached.** Whether routing decisions come from the
   hosted cloud router (api.rayline.ai, requires login) or the on-device local
   router is selected by `--local-router` (`lib.rs:933`) and a separate
   `decision_plane` string (`crates/rayline-cli/src/router.rs:61`), entirely
   outside the `RoutingMode` enum it is conceptually paired with.

5. **An impossible state is representable.** Local inference is only reachable
   *through the proxy* (something must intercept each request and forward it to
   a local LLM endpoint). Nothing in the current types prevents "local router +
   no proxy"; it is merely avoided by convention.

## Goal

Re-express the user-facing surface as the **two orthogonal axes it really is**,
make the one impossible combination unrepresentable, and give the proxy scope a
**router-dependent default** that matches how each router is meant to be used.

Out of scope: the proxy wire format ("all"/"selective-subagents"), the
local-router `select_route` logic, the daemon's internal `ProxyRoutingMode`
data path, and the duplicate `RouteTarget`/`RouteDecision` type names across
crates (a separate cleanup). This spec changes the **CLI surface and the
`rayline-cli` `RoutingMode` type**, not the data plane.

## The two axes

**Axis 1 — Router (decision plane):** where routing decisions and model
selection happen.

- **Cloud** — hosted router at api.rayline.ai. Requires login/auth.
- **Local** — on-device static router. No login; everything stays local.

**Axis 2 — Connection mechanism (`--via`):** *how* rayline wires Claude Code
to the router. These are the two first-class injection points Claude Code
itself supports; neither is inferior — they are different mechanisms with
different reach.

- **`env`** — set `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` + auth so Claude Code
  talks to the router directly. This is Claude Code's own **endpoint override /
  "LLM gateway" mechanism** (see *Naming* below). Whole-session, lightest, no
  per-request interception. **Cloud only** — cannot reach local inference and
  cannot do per-request selective routing.
- **`proxy`** — a local proxy intercepts and forwards each request. **Required**
  to reach local inference *or* to route selectively (main agent vs subagents).
  Carries a *scope* (below).

### Why "via", not "no-proxy" — naming

The previous name `--no-proxy` had three problems: it was a negation, it forced
the user to understand what a proxy is, and — critically — it **collides with
Claude Code's own vocabulary**. In Claude Code's docs, "proxy" means
`HTTPS_PROXY` *network/transport* interception
(`code.claude.com/docs/en/network-config`, "Proxy configuration"). The thing
`--no-proxy` actually toggled — pointing `ANTHROPIC_BASE_URL` at the router — is
a *different*, application-layer concept Anthropic documents separately as the
**endpoint override / LLM gateway** mechanism
(`code.claude.com/docs/en/llm-gateway`). So `--no-proxy` mis-named the very
thing it controlled.

`--via env|proxy` fixes all three: it is a symmetric axis with two equal noun
values (neither editorializes the other as worse), it names the *mechanism* the
user is choosing, and it reads naturally ("connect **via** env"). It is
orthogonal to `--local` (router axis) and `--route` (scope), which is the
orthogonality the old flag violated.

### Auto-select (the default)

`--via` defaults to **`proxy`**. The proxy is the default mechanism because it
is what makes `rayline top` and per-request metrics work out of the box (see
the always-on proxy-metrics design), and it is the only mechanism that supports
local inference and selective routing. Auto-select is therefore trivial:

- **Default → `proxy`** for every session.
- **`--via env`** is the explicit opt-in to the lightweight native
  endpoint-override path: no background process, but cloud-only, route-all, and
  no metrics.

`--via` is an **advanced override**; the typical user runs the proxy and never
touches it.

### The validity matrix

|                    | `--via env`             | `--via proxy`                          |
|--------------------|-------------------------|----------------------------------------|
| **Cloud router**   | ✅ simplest hosted      | ✅ hosted; default scope = **all**     |
| **Local router**   | ❌ **impossible**       | ✅ fully local; default scope = **subagents-only** |

Three of four cells are valid. **Local + `env` is impossible** and must be
unrepresentable in the type.

## Proxy scope and its asymmetric default

When interception is Proxy, a *scope* selects what gets routed through the
router versus passed straight through to Anthropic:

- **All** — route every request (main agent + subagents) through the router.
- **SubagentsOnly** — route only subagent traffic; the main agent passes
  through to cloud Claude. (Today's `selective-subagents`.)

The **default scope depends on the router**, because the two routers are used
differently:

- **Cloud router → default `All`.** The hosted router performs model selection;
  applying it universally is the intended behavior and is cheap. *(This flips
  the current effective default — see Migration.)*
- **Local router → default `SubagentsOnly`.** The local path is a **hybrid** by
  default: the main agent stays on cloud Claude (quality), and only subagents
  are offloaded to on-device models. Override to `All` for a fully-local
  session.

Both directions are overridable via `--route {all|subagents}`.

## Proposed CLI surface

| Command                                | Router | Mechanism (`--via`)  | Scope          |
|----------------------------------------|--------|----------------------|----------------|
| `rayline claude` *(default)*           | Cloud  | proxy                | **all**        |
| `rayline claude --via env`             | Cloud  | env (forced)         | all            |
| `rayline claude --local`               | Local  | proxy                | **subagents-only** |
| `rayline claude --local --route all`   | Local  | proxy                | all            |
| `rayline claude --route subagents`     | Cloud  | proxy                | subagents-only |

- `rayline claude` with no flags uses the **proxy** (default mechanism), so
  `rayline top` / metrics work out of the box.
- `--local` is the natural name for "local router" and **forces `proxy`**
  (local inference is unreachable via `env`). (Today the real flag is
  `--local-router`; `--local` currently warns as removed at `lib.rs:927-932` —
  this reclaims it.)
- `--via env|proxy` is the advanced mechanism override. `--via env` is rejected
  when combined with `--local` or `--route subagents` (env cannot serve either)
  — with a clear error pointing at the conflict.
- `--route {all|subagents}` overrides the router's default scope. `subagents`
  implies proxy (auto-upgrades the mechanism); it conflicts with `--via env`.

## Design

### 1. Replace the single enum with a two-axis config

File: `crates/rayline-cli/src/claude.rs`

Replace `RoutingMode { Override, Proxy, ProxySubagents }` with a *resolved*
config plus the raw user intent the parser collects.

```rust
pub enum Router { Cloud, Local }

pub enum ProxyScope { All, SubagentsOnly }

/// The resolved connection mechanism (after auto-select).
pub enum Connection {
    Env,                         // endpoint override; cloud-only, no proxy
    Proxy { scope: ProxyScope },
}

pub struct RoutingConfig {
    pub router: Router,
    pub connection: Connection,
}
```

- The invariant **`Local ⇒ Proxy`** is enforced by construction: there is no
  `Router::Local` + `Connection::Env` value reachable through the constructor.
  `RoutingConfig::new(router, connection)` returns an error for that pair, so
  the impossible cell cannot exist downstream.
- Scope default is computed from the router, not a constant:

```rust
fn default_scope(router: Router) -> ProxyScope {
    match router {
        Router::Cloud => ProxyScope::All,
        Router::Local => ProxyScope::SubagentsOnly,
    }
}
```

- `default_model_for_routing_mode` (`claude.rs:119-124`) becomes a function of
  `(router, scope)`. The existing constants `DEFAULT_MODEL = "rayline-router"`
  and `DEFAULT_PROXY_SUBAGENTS_MODEL = "claude-sonnet-4-6"` (`claude.rs:22-23`)
  carry over: subagents-only scope keeps the Sonnet main-agent default; all
  scope uses the virtual `rayline-router` model.

### 2. Resolution

The parser collects raw intent — `router`, an optional explicit
`via: Option<Via>` (`Env`/`Proxy`), and an optional explicit
`scope: Option<ProxyScope>` — then resolves it into a `RoutingConfig`:

```rust
fn resolve(router, via, scope) -> Result<RoutingConfig> {
    let scope = scope.unwrap_or(default_scope(router));
    let connection = match via {
        Some(Via::Env)            => Connection::Env,   // validated below
        Some(Via::Proxy) | None   => Connection::Proxy { scope }, // proxy is the default
    };
    RoutingConfig::new(router, connection) // enforces Local ⇒ Proxy
}
```

The default mechanism is **proxy**; `env` is reachable only by explicit
`--via env`. Conflicts with `--via env` are rejected with a clear, specific
error (not a generic parse fail):
- `--via env` + `--local` → "env cannot reach local inference; drop `--local`
  or `--via env`."
- `--via env` + `--route subagents` → "selective routing needs the proxy."

### 3. Flag parsing

File: `crates/rayline-cli/src/lib.rs`

- Remove `--routing-mode`/`parse_routing_mode` and `--no-proxy` from the primary
  surface (keep both as deprecated aliases — see §5). Remove the dead parser
  default (`lib.rs:843`) and the silent rewrite in dispatch (`lib.rs:317-320`);
  the default is **no explicit `via`/`scope`** → resolves to
  `{ Cloud, Proxy { All } }` for the bare `rayline claude`.
- `--via env|proxy` → sets `via`. Parsed like other value flags (both
  `--via=env` and `--via env` forms, mirroring `--routing-mode` at
  `lib.rs:891,923`). Name chosen to avoid colliding with the existing
  `--env <profile>` flag (`lib.rs:911`).
- `--local` → `router = Local` (auto-select forces proxy).
- `--route all|subagents` → sets explicit `scope`.
- `RunRequest.routing_mode: RoutingMode` (`claude.rs:100`) becomes
  `routing_config: RoutingConfig`. Update construction at `lib.rs:981-996` and
  the `--local-router`/`local_router` field plumbing (`lib.rs:933,987`), which
  collapses into `router: Local`.

### 4. Translation to the proxy wire value

File: `crates/rayline-cli/src/claude.rs`

- `is_proxy_routing_mode` (`claude.rs:115`) → `matches!(connection,
  Connection::Proxy { .. })`.
- `proxy_routing_mode_name` (`claude.rs:2147-2152`) maps **scope**, not the old
  enum: `ProxyScope::All → "all"`, `ProxyScope::SubagentsOnly →
  "selective-subagents"`. `Connection::Env` no longer participates (it starts no
  proxy), so the misleading `Override → "all"` mapping is deleted outright.
- The dispatch branch that today switches on `Override` vs `Proxy|ProxySubagents`
  (`claude.rs:427-490`) switches on `connection` instead. The cloud/local
  decision (`decision_plane`, `router.rs:61,139`) is set from `router`.

### 5. Backward compatibility (deprecated aliases)

Keep existing entry points working for one release, each emitting a single
deprecation warning that names the replacement:

- `--no-proxy` → `--via env`
- `--routing-mode override` → `--via env`
- `--routing-mode proxy` → `--route all` (or just the default)
- `--routing-mode proxy-subagents` → `--route subagents`
- `--local-router` → `--local`
- `RAYLINE_CLAUDE_ROUTING_MODE` env round-trip (`claude.rs:761-762, 1723-1729`)
  continues to read/write the same string values; only the internal type it
  maps to changes.

The daemon's `--proxy-routing-mode` / `RAYLINE_PROXY_ROUTING_MODE`
(`crates/rayline-daemon/src/main.rs:231-232, 275-276`) and the
`all`/`selective-subagents` wire vocabulary are **unchanged** — this redesign
stops at the CLI/`rayline-cli` boundary.

## Verification

- **Unit:** `RoutingConfig::new(Local, Connection::Env)` returns an error;
  `default_scope(Cloud) == All`, `default_scope(Local) == SubagentsOnly`.
- **Resolution tests:** `resolve` yields `Proxy{All}` for bare cloud,
  `Proxy{SubagentsOnly}` for `--route subagents`, `Proxy{SubagentsOnly}` for
  local, and `Env` only for explicit `--via env`.
- **Parse table tests** for each row of the CLI surface table above, plus the
  rejection cases (`--via env --local`, `--via env --route subagents`).
- **Deprecated-alias tests:** each old flag/env value maps to the expected new
  `RoutingConfig` and emits the warning.
- **Default guard:** a test asserting `rayline claude` with no flags resolves to
  `{ Cloud, Proxy { All } }` (catches regressions of both the proxy default and
  the cloud scope-flip).
- Existing `it_proxy.rs` / `it_claude_live.rs` integration tests should pass
  unchanged once the wire-value mapping is confirmed identical.

## Migration note (behavior change)

This **flips the effective cloud default** from selective-subagents (today's
silently-applied `ProxySubagents`) to **route-all**. Existing cloud users will
see main-agent traffic begin flowing through the cloud router instead of
straight to Anthropic. This is intended (the hosted router does model
selection), but warrants a changelog entry and a `--route subagents` escape
hatch for anyone who wants the old behavior.

# Metrics always available in proxy mode

**Date:** 2026-06-22
**Status:** Approved (design)

## Problem

`rayline top` reads router metrics from an HTTP metrics-control server at
`/v1/router/top/snapshot`. Today that server is stood up **only** by the
`rld serve` local-router daemon (`rayline-daemon/src/main.rs:411-415`). The
standalone `rld proxy` (cloud / isolated proxy mode) never hosts a metrics
server — it only forwards updates to a serve daemon via `--metrics-url`, and
only when that URL is present (`main.rs:760-763`).

Consequence: any proxy-mode launch that is **not** routing to a local model has
no metrics server. The motivating case is `rayline claude --isolated`, which
runs cloud-only (the implicit account-local path is gated off under
`--isolated`, `rayline-cli/src/claude.rs:322`), so `--metrics-url` resolves to
`None`, `opts.metrics` is `None`, and `rayline top` fails with:

```
router metrics endpoint is not available: error sending request for url
(http://127.0.0.1:20813/v1/router/top/snapshot)
```

## Goal

Make metrics work for **every** proxy-mode launch, including cloud-only and
`--isolated` sessions, so `rayline top` can monitor requests regardless of
whether a local router is engaged.

Out of scope: changing the metrics model, the serve-daemon path, the existing
proxy→serve forwarding path, or override (non-proxy) routing mode.

## Principle

The proxy self-hosts its own metrics-control server **whenever it is not already
forwarding to a serve daemon**. Monitoring becomes a property of any proxy-mode
launch, not just local-routing launches.

## Port map (existing + new)

| Port  | Owner                         |
|-------|-------------------------------|
| 20808 | adapter                       |
| 20809 | injector                      |
| 20810 | proxy (non-isolated)          |
| 20811 | local-router                  |
| 20812 | proxy (isolated)              |
| 20813 | metrics (non-isolated)        |
| 20814 | metrics (isolated) — **new**  |

## Design

### 1. `rayline-daemon` — `run_proxy` self-hosts when not forwarding

File: `crates/rayline-daemon/src/main.rs`

- Add a hidden `--metrics-port` arg to `ProxyArgs`, mirroring `ServeArgs`:
  `#[arg(long, env = METRICS_PORT_ENV, default_value_t = rayline_metrics::DEFAULT_METRICS_PORT, hide = true)]`.
- In `run_proxy`, decide the metrics wiring from `args.metrics_url`:
  - **`metrics_url` present (forwarding):** unchanged. `opts.metrics =
    Some(HttpMetricsSink::new(url))`. Do **not** self-host.
  - **`metrics_url` absent (self-host):** create
    `RouterMetrics::new("rayline-proxy")`, attempt to bind the metrics-control
    server on `args.metrics_port`, spawn it (reuse existing
    `bind_metrics_control` / `spawn_metrics_control` / `serve_metrics_control` /
    `handle_metrics_control`), and set `opts.metrics` to that in-process sink.
- **Robustness — best-effort bind:** unlike `run_serve` (which does
  `bind_metrics_control(...).await?` and hard-fails), the proxy must treat a
  metrics bind failure as non-fatal: log a warning and continue serving the
  proxy with `opts.metrics = None`. Monitoring must never take down the proxy
  data path. Implement by matching on the bind `Result` instead of `?`.
- No change needed to request accounting: `proxy_owns_metrics_for_route`
  (`rayline-proxy/src/lib.rs:787-789`) already returns `true` for all routes
  when `local_router_owns_metrics` is false (the cloud-proxy case), so
  passthrough requests are recorded into the self-hosted sink.

### 2. `rayline-cli` — port allocation, proxy meta, `top` discovery

Files: `crates/rayline-cli/src/router.rs`, `crates/rayline-cli/src/claude.rs`

- **Isolated metrics port:** add const `DEFAULT_ISOLATED_METRICS_PORT: u16 =
  20814` and env `RAYLINE_ISOLATED_METRICS_PORT`. Add
  `resolve_metrics_port(isolated: bool) -> Result<u16, _>` mirroring
  `resolve_proxy_port` (env override → default; isolated selects the isolated
  default + env var).
- **Pass the port:** `spawn_proxy` adds `--metrics-port <resolve_metrics_port(isolated)>`
  to the `rld proxy` command.
- **Record in meta only when self-hosting:** `proxy_meta` gains a
  `self_hosted_metrics_port: Option<u16>` parameter. It writes
  `metrics_port=<n>` into the proxy meta **only** when set — i.e. only when
  `metrics_url` is `None` (proxy is self-hosting). When forwarding, the port is
  omitted so `top`'s fallback never points at a port the proxy does not own.
  Callers (`spawn_proxy`, `start_proxy_from_home_with_client` via
  `requested_meta`) pass `Some(port)` iff `metrics_url.is_none()`.
- **Auto-detect discovery in `render_top`:** replace the single serve-meta read
  (`router.rs:258-260`) with a resolver that builds an ordered, de-duplicated
  candidate list:
  1. `metrics_port` from serve meta (`RouterPaths::new(home).meta_file`)
  2. `metrics_port` from non-isolated proxy meta
     (`RouterPaths::new(home).proxy_meta_file`)
  3. `metrics_port` from isolated proxy meta
     (`RouterPaths::new_isolated(home).proxy_meta_file`)
  4. `DEFAULT_METRICS_PORT` (final fallback)

  Probe each candidate with a short-timeout snapshot GET; use the first that
  responds `200`. If none respond, use the highest-precedence candidate so the
  existing "not available" error still names a sensible port.

  Factor the ordering into a pure helper
  `metrics_port_candidates(serve_meta, proxy_meta, isolated_proxy_meta) ->
  Vec<u16>` (reads the three meta maps, no network) so it is unit-testable; the
  probing wrapper lives in `render_top_from_home` where the reqwest client
  already exists. Resolve the port once at startup and hand the single URL to
  the existing snapshot/TUI paths.

### Resulting behavior

| Launch                                   | metrics server               | `top` finds it via        |
|------------------------------------------|------------------------------|---------------------------|
| `rayline claude` (local routing engaged) | serve daemon `:20813`        | serve meta (unchanged)    |
| `rayline claude` cloud-only, no serve    | proxy self-hosts `:20813`    | non-isolated proxy meta   |
| `rayline claude --isolated` (cloud-only) | isolated proxy self-hosts `:20814` | isolated proxy meta |

## Testing

- **Unit (pure):** `metrics_port_candidates` precedence — serve > non-isolated
  proxy > isolated proxy > default, plus de-dup and empty-meta fallback. Built
  from in-memory meta maps, no network.
- **Integration (cut-point):** launch `rld proxy` with no `--metrics-url` on
  ephemeral proxy + metrics ports, GET `/v1/router/top/snapshot`, assert `200`
  and a well-formed `MetricsSnapshot` (has `ok`, `totals`, `active`, `recent`).
  Pins the self-hosting behavior end-to-end.

## Risks / notes

- Metrics-port collision when an isolated and non-isolated cloud-only proxy run
  at once is avoided by the separate `20814` isolated default. A collision
  beyond that (e.g. an unrelated process on the port) degrades to "no metrics
  for this session" via the best-effort bind, never a proxy failure.
- Auto-detect picks one server when multiple sessions run simultaneously
  (accepted trade-off per design decision). Probing reachability makes it prefer
  a live server over a stale meta entry.

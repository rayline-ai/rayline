//! Effective router-config resolution and the default `~/.config/rayline/router.json`.
//!
//! Routing is config-file driven: an explicit `--router-config-path` wins, else the
//! default file at [`default_config_path`] (auto-created on first cloud launch with
//! content that reproduces today's behavior — everything through the hosted cloud
//! router). The on-device router is engaged only when the effective config routes
//! something *away* from the hosted cloud router (see [`config_needs_local_router`]);
//! a pure default stays on today's hosted path with no local process.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Default user-editable router config: `~/.config/rayline/router.json`.
pub fn default_config_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("router.json")
}

/// Default content: reproduce today's `rayline claude` — route everything to the
/// hosted cloud router. Users edit this to send subagents/main to local or custom
/// endpoints.
pub fn default_config_json() -> Value {
    json!({
        "endpoints": [
            {
                "id": "rayline-cloud",
                "protocol": "anthropic_messages",
                "base_url": crate::ROUTER_PROD_URL,
                "api_key_env": "RAYLINE_ROUTER_API_KEY",
                "models": ["rayline-router"]
            }
        ],
        "routes": {
            "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
            "default": { "endpoint": "rayline-cloud", "model": "rayline-router" },
            "subagents": {}
        }
    })
}

/// Create the default config file if absent. Idempotent; never overwrites user edits.
pub fn ensure_default_config(home: &Path) -> io::Result<PathBuf> {
    let path = default_config_path(home);
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_vec_pretty(&default_config_json()).map_err(io::Error::other)?;
        std::fs::write(&path, body)?;
    }
    Ok(path)
}

/// Resolve the effective config path: explicit flag wins, else the default file if
/// it exists. `None` means neither is present (caller stays on today's behavior).
pub fn resolve_config_path(flag: Option<&Path>, home: &Path) -> Option<PathBuf> {
    if let Some(path) = flag {
        return Some(path.to_path_buf());
    }
    let default = default_config_path(home);
    default.exists().then_some(default)
}

/// Whether the config requires the on-device router. True if any route targets an
/// endpoint that is *not* the hosted cloud router (`api.rayline.ai`) — including the
/// bundled `"local"` endpoint, a custom/loopback provider, or a direct-Anthropic
/// endpoint, all of which the hosted plane cannot serve on its own — **or** if any
/// route declares `router: rayline-local` (the on-device LSR is explicitly the
/// router, even when it forwards to the `rayline-cloud` endpoint: it pins the
/// route's `model` on-device instead of letting the hosted RCR pick). A pure
/// everything-to-the-cloud-router config with no `rayline-local` route returns
/// `false` → stay on today's hosted path.
pub fn config_needs_local_router(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    config_value_needs_local_router(&cfg)
}

fn config_value_needs_local_router(cfg: &Value) -> bool {
    if config_value_uses_local_decider(cfg) {
        return true;
    }
    let cloud_ids = cloud_router_endpoint_ids(cfg);
    route_target_endpoints(cfg)
        .into_iter()
        .any(|endpoint| !cloud_ids.contains(&endpoint))
}

/// Whether any route names the on-device LSR as its router (`router: rayline-local`).
/// Such a route is decided + has its `model` pinned on-device — so the LSR must be
/// engaged even if the route's endpoint is the hosted cloud router.
fn config_value_uses_local_decider(cfg: &Value) -> bool {
    let Some(routes) = cfg.get("routes") else {
        return false;
    };
    let singletons = ["main", "subagent", "default"]
        .into_iter()
        .filter_map(|key| routes.get(key));
    let maps = ["subagents", "model_routes"]
        .into_iter()
        .filter_map(|key| routes.get(key))
        .filter_map(Value::as_object)
        .flat_map(|map| map.values());
    singletons
        .chain(maps)
        .any(|route| route.get("router").and_then(Value::as_str) == Some(ROUTER_RAYLINE_LOCAL))
}

/// Whether any route targets the hosted cloud router (so its key should be
/// resolved from `rayline auth login`).
pub fn config_uses_cloud_router(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let cloud_ids = cloud_router_endpoint_ids(&cfg);
    route_target_endpoints(&cfg)
        .into_iter()
        .any(|endpoint| cloud_ids.contains(&endpoint))
}

/// Whether any route targets the bundled `"local"` endpoint (needs a configured
/// local model).
pub fn config_uses_local_endpoint(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    route_target_endpoints(&cfg)
        .into_iter()
        .any(|endpoint| endpoint == "local")
}

/// `router` value selecting the hosted cloud decider (the default when absent).
pub const ROUTER_RAYLINE_CLOUD: &str = "rayline-cloud";
/// `router` value selecting the on-device LSR decider (RRL): the LSR routes the
/// class per the static JSON and pins its `model`, rather than the hosted RCR.
pub const ROUTER_RAYLINE_LOCAL: &str = "rayline-local";

/// The local model the hosted cloud router may redirect a `rayline` class to
/// ("may-local"), resolved from the config. Returns the advertised model id and
/// the base URL of the local endpoint that serves it (the redirect target the
/// proxy fronts via a custom-mode adapter). `None` when no route turns may-local
/// on (no `router: rayline-cloud` route carries a non-empty `local_models`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MayLocal {
    pub model: String,
    pub upstream_url: String,
}

/// Resolve [`MayLocal`] from a config file. See [`MayLocal`].
pub fn config_may_local(path: &Path) -> Option<MayLocal> {
    let raw = std::fs::read(path).ok()?;
    let cfg: Value = serde_json::from_slice(&raw).ok()?;
    config_value_may_local(&cfg)
}

fn config_value_may_local(cfg: &Value) -> Option<MayLocal> {
    let model = config_advertised_local_model(cfg)?;
    let raw_base = endpoint_base_url_for_model(cfg, &model)?;
    // The custom-mode adapter appends `/v1/messages` to the upstream base
    // (`rayline-adapter` `format!("{}/v1/messages", target)`), so the upstream must
    // be the server *root*. Normalize exactly like `rayline local custom` does —
    // strip a trailing `/v1` — so an `openai_chat` endpoint's `…/v1` base_url does
    // not become `…/v1/v1/messages`.
    let upstream_url = crate::local_model::normalize_base_url(&raw_base);
    Some(MayLocal {
        model,
        upstream_url,
    })
}

/// First local model advertised by a may-local route: a route whose `router` is
/// `rayline-cloud` (or absent → the cloud default) carrying a non-empty
/// `local_models`. Routes are scanned `main`, `subagent`, `default`, then the
/// `subagents`/`model_routes` maps. `rayline-local` routes are skipped (may-local
/// is `N/A` there).
fn config_advertised_local_model(cfg: &Value) -> Option<String> {
    let routes = cfg.get("routes")?;
    let singletons = ["main", "subagent", "default"]
        .into_iter()
        .filter_map(|key| routes.get(key));
    let maps = ["subagents", "model_routes"]
        .into_iter()
        .filter_map(|key| routes.get(key))
        .filter_map(Value::as_object)
        .flat_map(|map| map.values());
    singletons
        .chain(maps)
        .filter_map(route_advertised_local_model)
        .next()
}

/// A single route's advertised local model, if it has may-local on.
fn route_advertised_local_model(route: &Value) -> Option<String> {
    // `rayline-local` routes never advertise may-local (N/A).
    if route.get("router").and_then(Value::as_str) == Some(ROUTER_RAYLINE_LOCAL) {
        return None;
    }
    route
        .get("local_models")
        .and_then(Value::as_array)?
        .iter()
        .find_map(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Base URL of the (non-cloud) endpoint that lists `model` in its `models`. This
/// is the upstream the proxy's may-local redirect is fronted onto. The hosted
/// cloud router is excluded — a local model is served by a local endpoint.
fn endpoint_base_url_for_model(cfg: &Value, model: &str) -> Option<String> {
    let cloud_ids = cloud_router_endpoint_ids(cfg);
    cfg.get("endpoints")
        .and_then(Value::as_array)?
        .iter()
        .find(|endpoint| {
            let id = endpoint.get("id").and_then(Value::as_str);
            let is_cloud = id.is_some_and(|id| cloud_ids.iter().any(|cloud| cloud == id));
            !is_cloud
                && endpoint
                    .get("models")
                    .and_then(Value::as_array)
                    .is_some_and(|models| models.iter().any(|m| m.as_str() == Some(model)))
        })
        .and_then(|endpoint| endpoint.get("base_url").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

/// Reserved `routes.main.endpoint` value meaning "do not route the main agent —
/// let it pass through to the caller's own Claude subscription/credential".
/// `RouterConfig` cannot express a credential-passthrough endpoint, so the CLI
/// reads this sentinel to pick the proxy scope (selective-subagents) instead of
/// emitting it into the on-device router config.
pub const SUBSCRIPTION_MAIN: &str = "subscription";

/// Whether the main agent should pass through to the caller's own subscription
/// rather than be routed by the on-device router. True when `routes.main` is
/// absent, or its `endpoint` is the reserved [`SUBSCRIPTION_MAIN`] sentinel.
/// Drives `RoutingMode::ProxySubagents` (main passthrough) vs `Proxy` (route all).
pub fn config_main_is_passthrough(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return true;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return true;
    };
    config_value_main_is_passthrough(&cfg)
}

fn config_value_main_is_passthrough(cfg: &Value) -> bool {
    match cfg.get("routes").and_then(|routes| routes.get("main")) {
        None => true,
        Some(main) => route_endpoint(main).as_deref() == Some(SUBSCRIPTION_MAIN),
    }
}

/// Produce the config the on-device router should load. When `routes.main` is the
/// passthrough sentinel (or absent) — main stays on the caller's subscription,
/// handled by the proxy's selective-subagents scope — strip `routes.main` so the
/// local router (which has no `subscription` endpoint) doesn't reject it during
/// normalization. Otherwise the original file is used verbatim.
///
/// The derived file is written to `~/.rayline/rld/config-routes.json`.
pub fn materialize_for_local_router(path: &Path, home: &Path) -> io::Result<PathBuf> {
    if !config_main_is_passthrough(path) {
        return Ok(path.to_path_buf());
    }
    let raw = std::fs::read(path)?;
    let mut cfg: Value = serde_json::from_slice(&raw).map_err(io::Error::other)?;
    if let Some(routes) = cfg.get_mut("routes").and_then(Value::as_object_mut) {
        routes.remove("main");
    }
    let out = home.join(".rayline").join("rld").join("config-routes.json");
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&cfg).map_err(io::Error::other)?;
    std::fs::write(&out, body)?;
    Ok(out)
}

/// Endpoint ids whose `base_url` host is the hosted cloud router.
fn cloud_router_endpoint_ids(cfg: &Value) -> Vec<String> {
    let cloud_host = host_of(crate::ROUTER_PROD_URL);
    let Some(endpoints) = cfg.get("endpoints").and_then(Value::as_array) else {
        return Vec::new();
    };
    endpoints
        .iter()
        .filter_map(|endpoint| {
            let id = endpoint.get("id").and_then(Value::as_str)?;
            let base_url = endpoint.get("base_url").and_then(Value::as_str)?;
            (host_of(base_url) == cloud_host && cloud_host.is_some()).then(|| id.to_owned())
        })
        .collect()
}

/// Every endpoint id referenced by any route (`main`, `default`, `subagent`,
/// `subagents.*`, `model_routes.*`).
fn route_target_endpoints(cfg: &Value) -> Vec<String> {
    let Some(routes) = cfg.get("routes") else {
        return Vec::new();
    };
    let mut endpoints = Vec::new();
    for key in ["main", "default", "subagent"] {
        if let Some(endpoint) = routes.get(key).and_then(route_endpoint) {
            endpoints.push(endpoint);
        }
    }
    for key in ["subagents", "model_routes"] {
        if let Some(map) = routes.get(key).and_then(Value::as_object) {
            endpoints.extend(map.values().filter_map(route_endpoint));
        }
    }
    // The `subscription` sentinel is a proxy-passthrough marker, not a real
    // endpoint — drop it so it never counts toward local/cloud endpoint detection.
    endpoints.retain(|endpoint| endpoint != SUBSCRIPTION_MAIN);
    endpoints
}

fn route_endpoint(route: &Value) -> Option<String> {
    route
        .get("endpoint")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(ToOwned::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_does_not_need_local_router() {
        assert!(!config_value_needs_local_router(&default_config_json()));
    }

    #[test]
    fn subagent_to_local_needs_local_router() {
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                "subagents": { "Explore": { "endpoint": "local" } }
            }
        });
        assert!(config_value_needs_local_router(&cfg));
    }

    #[test]
    fn subagent_to_loopback_endpoint_needs_local_router() {
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] },
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                "subagents": { "Explore": { "endpoint": "ollama", "model": "qwen2.5-coder:7b" } }
            }
        });
        assert!(config_value_needs_local_router(&cfg));
    }

    #[test]
    fn direct_anthropic_main_needs_local_router() {
        // Routing main at a non-cloud-router endpoint also requires the local router.
        let cfg = json!({
            "endpoints": [
                { "id": "anthropic", "protocol": "anthropic_messages",
                  "base_url": "https://api.anthropic.com", "models": ["claude-sonnet-4-6"] }
            ],
            "routes": { "main": { "endpoint": "anthropic", "model": "claude-sonnet-4-6" } }
        });
        assert!(config_value_needs_local_router(&cfg));
    }

    fn tmp_home() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("rl-router-config-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_default_config_creates_non_engaging_file() {
        let home = tmp_home();
        let path = ensure_default_config(&home).unwrap();
        assert!(path.exists());
        assert_eq!(path, default_config_path(&home));
        // The default file reproduces today's behavior → no local router.
        assert!(!config_needs_local_router(&path));
        assert!(config_uses_cloud_router(&path));
        assert!(!config_uses_local_endpoint(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_path_prefers_flag_then_default() {
        let home = tmp_home();
        // Neither present.
        assert!(resolve_config_path(None, &home).is_none());
        // Flag wins even when absent on disk.
        let flag = PathBuf::from("/tmp/explicit.json");
        assert_eq!(resolve_config_path(Some(&flag), &home), Some(flag));
        // Default file is picked up once it exists.
        let default = ensure_default_config(&home).unwrap();
        assert_eq!(resolve_config_path(None, &home), Some(default));
        let _ = std::fs::remove_dir_all(&home);
    }

    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/routing-modes")
    }

    /// Every shipped example config must parse, and the CLI's per-mode derivations
    /// (passthrough scope, local-plane engagement, cloud-key need) must match the
    /// mode's intent. This is the config↔mode cross-check.
    #[test]
    fn example_mode_configs_derive_expected_routing() {
        // (file, main_is_passthrough, needs_local_router, uses_cloud_router)
        let cases = [
            ("RRC.json", false, false, true),
            // RRCL: may-local routes stay on the cloud router (the `ollama` endpoint
            // is a redirect target, not a route) → no on-device router engaged.
            ("RRCL.json", false, false, true),
            // RRL: router rayline-local engages the on-device router even though
            // both routes target rayline-cloud (it pins the model on-device).
            ("RRL.json", false, true, true),
            ("RLC.json", false, true, true),
            ("RLC-per-type.json", false, true, true),
            ("RAC.json", false, true, true),
            // RAL/RLL/ARL/LRL: router rayline-local on the rayline class → on-device
            // routing; the other class is anthropic (API key) / ollama / subscription.
            ("RAL.json", false, true, true),
            ("RLL.json", false, true, true),
            ("ARL.json", true, true, true),
            ("LRL.json", false, true, true),
            ("ARC.json", true, false, true),
            ("AL.json", true, true, false),
            ("LRC.json", false, true, true),
            ("LL.json", false, true, false),
            ("LA.json", false, true, false),
        ];
        for (file, passthrough, needs_local, uses_cloud) in cases {
            let path = examples_dir().join(file);
            assert!(path.exists(), "missing example config {file}");
            // Must be valid JSON the router can read.
            let raw = std::fs::read(&path).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(
                config_main_is_passthrough(&path),
                passthrough,
                "{file}: main passthrough"
            );
            assert_eq!(
                config_needs_local_router(&path),
                needs_local,
                "{file}: needs local router"
            );
            assert_eq!(
                config_uses_cloud_router(&path),
                uses_cloud,
                "{file}: uses cloud router"
            );
            // None of the examples use the bundled `"local"` endpoint (they name
            // ollama explicitly), so none require an on-device model.
            assert!(
                !config_uses_local_endpoint(&path),
                "{file}: should not use bundled local endpoint"
            );
        }
    }

    #[test]
    fn cl_modes_may_local_wiring() {
        // For each `--local-model=on` (*CL) config, report what the CLI derives:
        //   resolves    = config_may_local resolves a model+upstream
        //   needs_local = a route targets a non-cloud endpoint (engages the LSR)
        //   fires       = the CLI actually wires may-local advertisement, which is
        //                 gated on the cloud-only path (!needs_local) in claude.rs.
        // (file, expect_resolves, expect_needs_local, expect_fires)
        let cases = [
            ("RRCL.json", true, false, true),
            ("ARCL.json", true, false, true),
            ("RACL.json", true, true, false),
            ("RLCL.json", true, true, false),
            ("LRCL.json", true, true, false),
        ];
        for (file, resolves, needs_local, fires) in cases {
            let path = examples_dir().join(file);
            assert!(path.exists(), "missing {file}");
            assert_eq!(
                config_may_local(&path).is_some(),
                resolves,
                "{file}: resolves"
            );
            assert_eq!(
                config_needs_local_router(&path),
                needs_local,
                "{file}: needs_local"
            );
            let actually_fires =
                !config_needs_local_router(&path) && config_may_local(&path).is_some();
            assert_eq!(actually_fires, fires, "{file}: may-local wired by CLI");
        }
    }

    #[test]
    fn rrl_example_engages_local_router() {
        // RRL: `router: rayline-local` makes the on-device LSR the router even though
        // the routes target the hosted `rayline-cloud` endpoint — so the LSR must be
        // engaged (it pins the route's `model` on-device instead of letting the RCR
        // pick). It is not may-local, not a passthrough main, and uses the cloud key.
        let path = examples_dir().join("RRL.json");
        assert!(path.exists(), "missing example config RRL.json");
        serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            config_needs_local_router(&path),
            "RRL: router rayline-local must engage the on-device router"
        );
        assert!(!config_main_is_passthrough(&path), "RRL: main is routed");
        assert!(
            config_uses_cloud_router(&path),
            "RRL: forwards to the cloud key"
        );
        assert!(
            !config_uses_local_endpoint(&path),
            "RRL: no bundled local model"
        );
        assert_eq!(config_may_local(&path), None, "RRL: not may-local");
    }

    #[test]
    fn rayline_local_router_engages_even_when_all_cloud() {
        // An all-cloud config normally stays on the hosted path...
        let all_cloud = json!({
            "endpoints": [{ "id": "rayline-cloud", "protocol": "anthropic_messages",
                "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });
        assert!(!config_value_needs_local_router(&all_cloud));
        // ...but `router: rayline-local` forces on-device routing.
        let mut local_decider = all_cloud.clone();
        local_decider["routes"]["main"]["router"] = json!("rayline-local");
        assert!(config_value_needs_local_router(&local_decider));
        assert!(config_value_uses_local_decider(&local_decider));
    }

    #[test]
    fn rrcl_example_resolves_may_local() {
        // The shipped RRCL config advertises a local model fronted by the `ollama`
        // endpoint, and stays cloud-only for routing (no on-device router engaged).
        let path = examples_dir().join("RRCL.json");
        assert!(path.exists(), "missing example config RRCL.json");
        assert_eq!(
            config_may_local(&path),
            Some(MayLocal {
                model: "qwen2.5-coder:7b".to_owned(),
                upstream_url: "http://127.0.0.1:11434".to_owned(),
            })
        );
        // RRC (no may-local) must not resolve one.
        assert_eq!(config_may_local(&examples_dir().join("RRC.json")), None);
    }

    #[test]
    fn materialize_strips_subscription_main_for_local_router() {
        let home = tmp_home();
        // ARC: main = subscription (passthrough) → stripped; subagent stays.
        let out = materialize_for_local_router(&examples_dir().join("ARC.json"), &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert!(
            cfg["routes"].get("main").is_none(),
            "subscription main must be stripped"
        );
        assert_eq!(cfg["routes"]["subagent"]["endpoint"], "rayline-cloud");
        // RLC: main is a real endpoint → file used verbatim (path unchanged).
        let rl = examples_dir().join("RLC.json");
        assert_eq!(materialize_for_local_router(&rl, &home).unwrap(), rl);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn may_local_resolves_model_and_upstream_for_rrcl() {
        // RRCL: rayline-cloud routes carrying `local_models`, plus a local endpoint
        // that serves the advertised model.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] },
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-cloud",
                          "local_models": ["qwen2.5-coder:7b"] },
                "subagent": { "endpoint": "rayline", "router": "rayline-cloud",
                              "local_models": ["qwen2.5-coder:7b"] }
            }
        });
        assert_eq!(
            config_value_may_local(&cfg),
            Some(MayLocal {
                model: "qwen2.5-coder:7b".to_owned(),
                upstream_url: "http://127.0.0.1:11434".to_owned(),
            })
        );
    }

    #[test]
    fn may_local_off_when_no_local_models() {
        // RRC: rayline-cloud, no `local_models` → may-local off.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-cloud" },
                "subagent": { "endpoint": "rayline", "router": "rayline-cloud" }
            }
        });
        assert_eq!(config_value_may_local(&cfg), None);
        assert_eq!(config_advertised_local_model(&cfg), None);
    }

    #[test]
    fn may_local_ignored_for_rayline_local_router() {
        // RRL-shaped: `rayline-local` routes never advertise may-local (N/A), even
        // if a stray `local_models` is present.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] },
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-local",
                          "local_models": ["qwen2.5-coder:7b"] }
            }
        });
        assert_eq!(config_value_may_local(&cfg), None);
    }

    #[test]
    fn may_local_none_when_model_endpoint_undeclared() {
        // `local_models` names a model no local endpoint serves → cannot resolve an
        // upstream, so may-local does not engage (the CLI surfaces a clear error
        // path instead of silently advertising an unreachable model).
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-cloud",
                          "local_models": ["qwen2.5-coder:7b"] }
            }
        });
        assert_eq!(
            config_advertised_local_model(&cfg),
            Some("qwen2.5-coder:7b".to_owned())
        );
        assert_eq!(endpoint_base_url_for_model(&cfg, "qwen2.5-coder:7b"), None);
        assert_eq!(config_value_may_local(&cfg), None);
    }

    #[test]
    fn all_cloud_router_routes_stay_remote() {
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": "https://api.rayline.ai", "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                "default": { "endpoint": "rayline-cloud", "model": "rayline-router" }
            }
        });
        assert!(!config_value_needs_local_router(&cfg));
    }
}

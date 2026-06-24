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
/// endpoint, all of which the hosted plane cannot serve on its own. A pure
/// everything-to-the-cloud-router config returns `false` → stay on today's path.
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
    let cloud_ids = cloud_router_endpoint_ids(cfg);
    route_target_endpoints(cfg)
        .into_iter()
        .any(|endpoint| !cloud_ids.contains(&endpoint))
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

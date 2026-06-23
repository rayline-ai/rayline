//! First-class local model providers.
//!
//! The bundled llama.cpp path is `LlamaCpp` (managed adapter + curated catalog).
//! `Ollama` and `LmStudio` are discovered over loopback HTTP endpoints and
//! routed through the local router's existing `openai_chat` endpoint support.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderId {
    LlamaCpp,
    Ollama,
    LmStudio,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llamacpp",
            Self::Ollama => "ollama",
            Self::LmStudio => "lmstudio",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LlamaCpp => "Built-in (managed llama.cpp)",
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "llamacpp" | "llama.cpp" | "llama-cpp" => Some(Self::LlamaCpp),
            "ollama" => Some(Self::Ollama),
            "lmstudio" | "lm-studio" | "lm_studio" => Some(Self::LmStudio),
            _ => None,
        }
    }

    pub fn start_hint(self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("ollama serve"),
            Self::LmStudio => {
                Some("the LM Studio app (Developer -> Start Server), or `lms server start`")
            }
            Self::LlamaCpp => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointSource {
    Default,
    Env(&'static str),
    Explicit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEndpoint {
    pub provider: ProviderId,
    /// Server root, normalized with no trailing slash and no trailing `/v1`.
    pub base_url: String,
    pub source: EndpointSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderModel {
    pub provider: ProviderId,
    pub model: String,
    pub size_bytes: Option<u64>,
}

pub fn parse_ollama_tags(body: &Value) -> Vec<ProviderModel> {
    body.get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    let name = model.get("name").and_then(Value::as_str)?.to_owned();
                    Some(ProviderModel {
                        provider: ProviderId::Ollama,
                        model: name,
                        size_bytes: model.get("size").and_then(Value::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_lmstudio_models(body: &Value) -> Vec<ProviderModel> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    let id = model.get("id").and_then(Value::as_str)?.to_owned();
                    Some(ProviderModel {
                        provider: ProviderId::LmStudio,
                        model: id,
                        size_bytes: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn provider_endpoint(id: ProviderId) -> Result<Option<ProviderEndpoint>, String> {
    let (raw, source) = match id {
        ProviderId::Ollama => match std::env::var("OLLAMA_HOST") {
            Ok(value) => (value, EndpointSource::Env("OLLAMA_HOST")),
            Err(_) => ("http://localhost:11434".to_owned(), EndpointSource::Default),
        },
        ProviderId::LmStudio => match std::env::var("LMSTUDIO_HOST") {
            Ok(value) => (value, EndpointSource::Env("LMSTUDIO_HOST")),
            Err(_) => ("http://localhost:1234".to_owned(), EndpointSource::Default),
        },
        ProviderId::LlamaCpp => return Ok(None),
    };
    let base_url = normalize_provider_root(&raw)?;
    validate_auto_discovery_loopback(&base_url)?;
    Ok(Some(ProviderEndpoint {
        provider: id,
        base_url,
        source,
    }))
}

pub fn explicit_provider_endpoint(id: ProviderId, raw: &str) -> Result<ProviderEndpoint, String> {
    let base_url = normalize_provider_root(raw)?;
    Ok(ProviderEndpoint {
        provider: id,
        base_url,
        source: EndpointSource::Explicit,
    })
}

fn normalize_provider_root(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let stripped = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    if stripped.is_empty() {
        return Err("provider URL must not be empty".to_owned());
    }
    let with_scheme = if stripped.contains("://") {
        stripped.to_owned()
    } else {
        format!("http://{stripped}")
    };
    let mut parsed = reqwest::Url::parse(&with_scheme)
        .map_err(|error| format!("invalid provider URL {stripped:?}: {error}"))?;
    if parsed
        .host_str()
        .and_then(parse_host_ip)
        .is_some_and(|ip| ip.is_unspecified())
    {
        parsed
            .set_host(Some("localhost"))
            .map_err(|_| format!("invalid provider URL {stripped:?}: could not set host"))?;
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

fn validate_auto_discovery_loopback(base_url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|error| format!("invalid provider URL {base_url:?}: {error}"))?;
    let Some(host) = parsed.host_str() else {
        return Err("provider URL must include a host".to_owned());
    };
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || parse_host_ip(host).is_some_and(|ip| ip.is_loopback());
    if !is_loopback {
        return Err(format!(
            "Refusing to auto-probe non-loopback provider URL {base_url}. Use an explicit custom URL path for remote providers instead."
        ));
    }
    Ok(())
}

fn parse_host_ip(host: &str) -> Option<IpAddr> {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.parse().ok()
}

async fn get_json(url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(response.status().to_string());
    }
    response.json().await.map_err(|error| error.to_string())
}

pub async fn detect(id: ProviderId) -> bool {
    if id == ProviderId::LlamaCpp {
        return true;
    }
    list_models(id).await.is_ok()
}

pub async fn list_models(id: ProviderId) -> Result<Vec<ProviderModel>, String> {
    let endpoint = provider_endpoint(id)?
        .ok_or_else(|| "managed provider uses the curated catalog".to_owned())?;
    list_models_at(&endpoint).await
}

pub async fn list_models_at(endpoint: &ProviderEndpoint) -> Result<Vec<ProviderModel>, String> {
    match endpoint.provider {
        ProviderId::Ollama => Ok(parse_ollama_tags(
            &get_json(&format!("{}/api/tags", endpoint.base_url)).await?,
        )),
        ProviderId::LmStudio => Ok(parse_lmstudio_models(
            &get_json(&format!("{}/v1/models", endpoint.base_url)).await?,
        )),
        ProviderId::LlamaCpp => Err("built-in uses the curated catalog".to_owned()),
    }
}

pub async fn discover_all() -> Vec<(ProviderId, Vec<ProviderModel>)> {
    let (ollama, lmstudio) = tokio::join!(
        async {
            list_models(ProviderId::Ollama)
                .await
                .ok()
                .filter(|models| !models.is_empty())
                .map(|models| (ProviderId::Ollama, models))
        },
        async {
            list_models(ProviderId::LmStudio)
                .await
                .ok()
                .filter(|models| !models.is_empty())
                .map(|models| (ProviderId::LmStudio, models))
        },
    );
    [ollama, lmstudio].into_iter().flatten().collect()
}

pub fn provider_openai_base(endpoint: &ProviderEndpoint) -> String {
    format!("{}/v1", endpoint.base_url.trim_end_matches('/'))
}

pub fn provider_routes_json(id: ProviderId, base_url_v1: &str, model: &str) -> Value {
    let endpoint_id = id.as_str();
    let mut subagents = serde_json::Map::new();
    for name in crate::onboarding::LOCAL_DEFAULT_SUBAGENTS {
        subagents.insert(
            (*name).to_owned(),
            json!({ "endpoint": endpoint_id, "model": model }),
        );
    }
    let target = json!({ "endpoint": endpoint_id, "model": model });
    json!({
        "endpoints": [{
            "id": endpoint_id,
            "protocol": "openai_chat",
            "base_url": base_url_v1,
            "models": [model],
        }],
        "routes": {
            "subagent": target,
            "model_routes": {
                "rayline-subagent": { "endpoint": endpoint_id, "model": model },
                "rayline-local": { "endpoint": endpoint_id, "model": model },
            },
            "subagents": Value::Object(subagents),
        },
    })
}

pub fn provider_routes_json_with_explicit_config(
    id: ProviderId,
    base_url_v1: &str,
    model: &str,
    explicit: &Value,
) -> Value {
    let mut generated = provider_routes_json(id, base_url_v1, model);
    merge_explicit_endpoints(&mut generated, explicit, id);
    merge_explicit_routes(&mut generated, id, model, explicit);
    generated
}

fn merge_explicit_endpoints(
    generated: &mut Value,
    explicit: &Value,
    selected_provider: ProviderId,
) {
    let Some(explicit_endpoints) = explicit.get("endpoints").and_then(Value::as_array) else {
        return;
    };
    let Some(generated_endpoints) = generated.get_mut("endpoints").and_then(Value::as_array_mut)
    else {
        return;
    };

    for endpoint in explicit_endpoints {
        let endpoint = endpoint.clone();
        let id = endpoint.get("id").and_then(Value::as_str);
        if id == Some(selected_provider.as_str()) {
            continue;
        }
        if let Some(index) = id.and_then(|id| {
            generated_endpoints
                .iter()
                .position(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
        }) {
            generated_endpoints[index] = endpoint;
        } else {
            generated_endpoints.push(endpoint);
        }
    }
}

fn merge_explicit_routes(generated: &mut Value, id: ProviderId, model: &str, explicit: &Value) {
    let Some(explicit_routes) = explicit.get("routes").and_then(Value::as_object) else {
        return;
    };
    let routes = generated
        .as_object_mut()
        .expect("provider routes are an object")
        .entry("routes")
        .or_insert_with(|| json!({}));
    let routes = routes
        .as_object_mut()
        .expect("provider route routes field is an object");

    for key in ["main", "subagent", "default"] {
        if let Some(target) = explicit_routes.get(key) {
            routes.insert(
                key.to_owned(),
                rewrite_provider_route_target(id, model, target),
            );
        }
    }

    if let Some(model_routes) = explicit_routes
        .get("model_routes")
        .and_then(Value::as_object)
    {
        let target_map = routes
            .entry("model_routes")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("provider model_routes field is an object");
        for (key, target) in model_routes {
            target_map.insert(
                key.clone(),
                rewrite_provider_route_target(id, model, target),
            );
        }
    }

    if let Some(subagents) = explicit_routes.get("subagents").and_then(Value::as_object) {
        let rewritten = subagents
            .iter()
            .map(|(key, target)| {
                (
                    key.clone(),
                    rewrite_provider_route_target(id, model, target),
                )
            })
            .collect();
        routes.insert("subagents".to_owned(), Value::Object(rewritten));
    }
}

fn rewrite_provider_route_target(id: ProviderId, model: &str, target: &Value) -> Value {
    let mut target = target.clone();
    let Some(object) = target.as_object_mut() else {
        return target;
    };
    let endpoint = object.get("endpoint").and_then(Value::as_str);
    if endpoint == Some("local") {
        object.insert("endpoint".to_owned(), Value::String(id.as_str().to_owned()));
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    } else if endpoint == Some(id.as_str())
        && object
            .get("model")
            .and_then(Value::as_str)
            .is_none_or(|model| model.trim().is_empty())
    {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }
    target
}

pub fn write_provider_routes(
    home: &Path,
    id: ProviderId,
    base_url_v1: &str,
    model: &str,
) -> io::Result<PathBuf> {
    write_provider_routes_for_config(home, id, base_url_v1, model, None)
}

pub fn write_provider_routes_for_config(
    home: &Path,
    id: ProviderId,
    base_url_v1: &str,
    model: &str,
    explicit_config_path: Option<&Path>,
) -> io::Result<PathBuf> {
    let dir = home.join(crate::ROUTER_STATE_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("provider-routes.json");
    let value = match explicit_config_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)?;
            let explicit = parse_explicit_router_config(path, &raw)?;
            provider_routes_json_with_explicit_config(id, base_url_v1, model, &explicit)
        }
        None => provider_routes_json(id, base_url_v1, model),
    };
    let body = serde_json::to_vec_pretty(&value).map_err(io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

fn parse_explicit_router_config(path: &Path, raw: &str) -> io::Result<Value> {
    let explicit = serde_json::from_str::<Value>(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse router config {}: {error}", path.display()),
        )
    })?;
    serde_json::from_value::<rayline_local_router::RouterConfig>(explicit.clone()).map_err(
        |error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse router config {}: {error}", path.display()),
            )
        },
    )?;
    Ok(explicit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<T>(key: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let previous = std::env::var_os(key);
        match value {
            Some(value) => {
                // SAFETY: this helper serializes environment mutation with a
                // process-local mutex and restores the prior value before returning.
                unsafe { std::env::set_var(key, value) };
            }
            None => {
                // SAFETY: this helper serializes environment mutation with a
                // process-local mutex and restores the prior value before returning.
                unsafe { std::env::remove_var(key) };
            }
        }
        let result = f();
        match previous {
            Some(previous) => {
                // SAFETY: see the safety note above; restoration is under the same lock.
                unsafe { std::env::set_var(key, previous) };
            }
            None => {
                // SAFETY: see the safety note above; restoration is under the same lock.
                unsafe { std::env::remove_var(key) };
            }
        }
        result
    }

    fn temp_home() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rayline-provider-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parse_ollama_tags_extracts_name_and_size() {
        let body = json!({"models":[
            {"name":"qwen3-coder:30b","size":18_000_000_000u64},
            {"name":"llama3.3:70b","size":40_000_000_000u64}
        ]});
        let models = parse_ollama_tags(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider, ProviderId::Ollama);
        assert_eq!(models[0].model, "qwen3-coder:30b");
        assert_eq!(models[0].size_bytes, Some(18_000_000_000));
    }

    #[test]
    fn parse_lmstudio_models_extracts_ids() {
        let body = json!({"object":"list","data":[
            {"id":"qwen2.5-coder-7b","object":"model"},
            {"id":"llama-3.2-3b","object":"model"}
        ]});
        let models = parse_lmstudio_models(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider, ProviderId::LmStudio);
        assert_eq!(models[0].model, "qwen2.5-coder-7b");
        assert_eq!(models[0].size_bytes, None);
    }

    #[test]
    fn parse_handles_empty_or_malformed() {
        assert!(parse_ollama_tags(&json!({})).is_empty());
        assert!(parse_lmstudio_models(&json!({"data": "nope"})).is_empty());
    }

    #[test]
    fn provider_id_parse_accepts_aliases() {
        assert_eq!(ProviderId::parse("ollama"), Some(ProviderId::Ollama));
        assert_eq!(ProviderId::parse("lm-studio"), Some(ProviderId::LmStudio));
        assert_eq!(ProviderId::parse("llama.cpp"), Some(ProviderId::LlamaCpp));
        assert_eq!(ProviderId::parse("bogus"), None);
    }

    #[test]
    fn provider_endpoint_defaults_are_loopback_roots() {
        with_env("OLLAMA_HOST", None, || {
            let endpoint = provider_endpoint(ProviderId::Ollama).unwrap().unwrap();
            assert_eq!(endpoint.base_url, "http://localhost:11434");
            assert_eq!(endpoint.source, EndpointSource::Default);
            assert_eq!(provider_openai_base(&endpoint), "http://localhost:11434/v1");
        });
    }

    #[test]
    fn provider_endpoint_accepts_loopback_env() {
        with_env("OLLAMA_HOST", Some("http://127.0.0.1:11434/v1"), || {
            let endpoint = provider_endpoint(ProviderId::Ollama).unwrap().unwrap();
            assert_eq!(endpoint.base_url, "http://127.0.0.1:11434");
            assert_eq!(endpoint.source, EndpointSource::Env("OLLAMA_HOST"));
        });
    }

    #[test]
    fn provider_endpoint_accepts_bare_host_port_env() {
        with_env("OLLAMA_HOST", Some("127.0.0.1:11434"), || {
            let endpoint = provider_endpoint(ProviderId::Ollama).unwrap().unwrap();
            assert_eq!(endpoint.base_url, "http://127.0.0.1:11434");
            assert_eq!(endpoint.source, EndpointSource::Env("OLLAMA_HOST"));
        });
    }

    #[test]
    fn provider_endpoint_maps_unspecified_bind_env_to_localhost() {
        with_env("OLLAMA_HOST", Some("0.0.0.0:11434"), || {
            let endpoint = provider_endpoint(ProviderId::Ollama).unwrap().unwrap();
            assert_eq!(endpoint.base_url, "http://localhost:11434");
            assert_eq!(endpoint.source, EndpointSource::Env("OLLAMA_HOST"));
        });
        with_env("OLLAMA_HOST", Some("[::]:11434"), || {
            let endpoint = provider_endpoint(ProviderId::Ollama).unwrap().unwrap();
            assert_eq!(endpoint.base_url, "http://localhost:11434");
            assert_eq!(endpoint.source, EndpointSource::Env("OLLAMA_HOST"));
        });
    }

    #[test]
    fn provider_endpoint_rejects_remote_env_for_auto_discovery() {
        with_env("OLLAMA_HOST", Some("http://10.0.0.4:11434"), || {
            assert!(provider_endpoint(ProviderId::Ollama).is_err());
        });
    }

    #[test]
    fn provider_openai_base_appends_v1_without_double_suffix() {
        let endpoint = ProviderEndpoint {
            provider: ProviderId::LmStudio,
            base_url: "http://localhost:1234".to_owned(),
            source: EndpointSource::Default,
        };
        assert_eq!(provider_openai_base(&endpoint), "http://localhost:1234/v1");
    }

    #[test]
    fn provider_routes_json_targets_openai_endpoint_for_allowlist() {
        let value = provider_routes_json(
            ProviderId::Ollama,
            "http://localhost:11434/v1",
            "qwen3-coder:30b",
        );
        let endpoint = &value["endpoints"][0];
        assert_eq!(endpoint["id"], "ollama");
        assert_eq!(endpoint["protocol"], "openai_chat");
        assert_eq!(endpoint["base_url"], "http://localhost:11434/v1");
        let subagents = value["routes"]["subagents"].as_object().unwrap();
        assert_eq!(
            subagents.len(),
            crate::onboarding::LOCAL_DEFAULT_SUBAGENTS.len()
        );
        assert_eq!(subagents["Explore"]["endpoint"], "ollama");
        assert_eq!(subagents["Explore"]["model"], "qwen3-coder:30b");
        assert_eq!(value["routes"]["subagent"]["endpoint"], "ollama");
        assert_eq!(value["routes"]["subagent"]["model"], "qwen3-coder:30b");
        assert_eq!(
            value["routes"]["model_routes"]["rayline-subagent"]["endpoint"],
            "ollama"
        );
        assert_eq!(
            value["routes"]["model_routes"]["rayline-local"]["endpoint"],
            "ollama"
        );
    }

    #[test]
    fn provider_routes_merge_explicit_config_preserves_subagent_allowlist() {
        let explicit = json!({
            "endpoints": [{
                "id": "openrouter",
                "protocol": "openai_chat",
                "base_url": "https://openrouter.ai/api/v1",
                "models": ["openai/gpt-5.2"]
            }],
            "routes": {
                "subagents": {
                    "Explore": { "endpoint": "local" }
                },
                "model_routes": {
                    "custom-reviewer": {
                        "endpoint": "openrouter",
                        "model": "openai/gpt-5.2"
                    }
                }
            }
        });
        let value = provider_routes_json_with_explicit_config(
            ProviderId::Ollama,
            "http://localhost:11434/v1",
            "qwen3-coder:30b",
            &explicit,
        );
        let subagents = value["routes"]["subagents"].as_object().unwrap();
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents["Explore"]["endpoint"], "ollama");
        assert_eq!(subagents["Explore"]["model"], "qwen3-coder:30b");
        assert!(subagents.get("codebase-locator").is_none());
        assert_eq!(
            value["routes"]["model_routes"]["rayline-subagent"]["endpoint"],
            "ollama"
        );
        assert_eq!(
            value["routes"]["model_routes"]["custom-reviewer"]["endpoint"],
            "openrouter"
        );
        let endpoints = value["endpoints"].as_array().unwrap();
        assert!(endpoints.iter().any(|endpoint| endpoint["id"] == "ollama"));
        assert!(
            endpoints
                .iter()
                .any(|endpoint| endpoint["id"] == "openrouter")
        );
    }

    #[test]
    fn provider_routes_merge_keeps_generated_provider_endpoint_on_id_collision() {
        let explicit = json!({
            "endpoints": [{
                "id": "ollama",
                "protocol": "openai_chat",
                "base_url": "http://stale.example/v1",
                "models": ["stale-model"]
            }],
            "routes": {
                "subagents": {
                    "Explore": { "endpoint": "local" }
                }
            }
        });
        let value = provider_routes_json_with_explicit_config(
            ProviderId::Ollama,
            "http://localhost:11434/v1",
            "qwen3-coder:30b",
            &explicit,
        );
        let endpoints = value["endpoints"].as_array().unwrap();
        let ollama = endpoints
            .iter()
            .find(|endpoint| endpoint["id"] == "ollama")
            .unwrap();

        assert_eq!(ollama["base_url"], "http://localhost:11434/v1");
        assert_eq!(ollama["models"][0], "qwen3-coder:30b");
        assert_eq!(
            value["routes"]["subagents"]["Explore"]["endpoint"],
            "ollama"
        );
        assert_eq!(
            value["routes"]["subagents"]["Explore"]["model"],
            "qwen3-coder:30b"
        );
    }

    #[test]
    fn write_provider_routes_rejects_malformed_explicit_config() {
        let home = temp_home();
        let config_path = home.join("bad-routes.json");
        std::fs::write(&config_path, r#"{"routes":{"subagents":["Explore"]}}"#).unwrap();

        let error = write_provider_routes_for_config(
            &home,
            ProviderId::Ollama,
            "http://localhost:11434/v1",
            "qwen3-coder:30b",
            Some(&config_path),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("parse router config"));
        assert!(
            !home
                .join(crate::ROUTER_STATE_DIR)
                .join("provider-routes.json")
                .exists()
        );
    }
}

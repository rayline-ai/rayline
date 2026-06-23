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
    reqwest::Url::parse(stripped)
        .map_err(|error| format!("invalid provider URL {stripped:?}: {error}"))?;
    Ok(stripped.trim_end_matches('/').to_owned())
}

fn validate_auto_discovery_loopback(base_url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|error| format!("invalid provider URL {base_url:?}: {error}"))?;
    let Some(host) = parsed.host_str() else {
        return Err("provider URL must include a host".to_owned());
    };
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !is_loopback {
        return Err(format!(
            "Refusing to auto-probe non-loopback provider URL {base_url}. Use an explicit custom URL path for remote providers instead."
        ));
    }
    Ok(())
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

pub fn write_provider_routes(
    home: &Path,
    id: ProviderId,
    base_url_v1: &str,
    model: &str,
) -> io::Result<PathBuf> {
    let dir = home.join(crate::ROUTER_STATE_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("provider-routes.json");
    let body = serde_json::to_vec_pretty(&provider_routes_json(id, base_url_v1, model))
        .map_err(io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

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
}

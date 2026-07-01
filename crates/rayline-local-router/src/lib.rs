//! Local-only static router for Claude Code-compatible Anthropic traffic.
//!
//! This crate deliberately mirrors the small HTTP surface the current
//! transparent proxy already expects from the hosted router. It keeps the first
//! OSS-shaped milestone local/client-side only: static rules, configured
//! provider endpoints, and local-model redirects.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_metrics::{MetricsUpdate, REQUEST_ID_HEADER, SharedMetricsSink, new_request_id};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub const DEFAULT_PORT: u16 = 20811;
pub const DEFAULT_LOCAL_ADAPTER_PORT: u16 = 20808;
pub const DEFAULT_VIRTUAL_MODEL: &str = "rayline-router";
pub const DEFAULT_SUBAGENT_MODEL: &str = "rayline-subagent";
pub const CONFIG_ENV: &str = "RAYLINE_ROUTER_CONFIG";
pub const MAIN_ENDPOINT_ENV: &str = "RAYLINE_MAIN_ENDPOINT";
pub const MAIN_MODEL_ENV: &str = "RAYLINE_MAIN_MODEL";
pub const SUBAGENT_ENDPOINT_ENV: &str = "RAYLINE_SUBAGENT_ENDPOINT";
pub const SUBAGENT_MODEL_ENV: &str = "RAYLINE_SUBAGENT_MODEL";
const CLAUDE_CODE_AGENT_ID_HEADER: &str = "x-claude-code-agent-id";
const RAYLINE_AGENT_TYPE_HEADER: &str = "x-rayline-claude-code-agent-type";
const OPENAI_SUBAGENT_HEADER: &str = "x-openai-subagent";
const OPENAI_CLIENT_REQUEST_ID_HEADER: &str = "x-client-request-id";

#[derive(Clone)]
pub struct LocalRouterOptions {
    pub port: u16,
    pub local_adapter_port: u16,
    pub local_model_id: String,
    pub config_path: Option<PathBuf>,
    pub metrics: Option<SharedMetricsSink>,
}

impl Default for LocalRouterOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            local_adapter_port: DEFAULT_LOCAL_ADAPTER_PORT,
            local_model_id: "qwen3.6-35b-a3b-q4-k-m".to_owned(),
            config_path: None,
            metrics: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RouterConfig {
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
    #[serde(default)]
    pub routes: RoutesConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EndpointConfig {
    pub id: String,
    #[serde(default = "default_endpoint_kind")]
    pub kind: String,
    pub protocol: EndpointProtocol,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Overrides the protocol-default auth scheme. `None` keeps the historical
    /// per-protocol behavior (`anthropic_messages` -> `x-api-key`,
    /// `openai_chat` -> bearer).
    #[serde(default)]
    pub auth: Option<AuthMode>,
}

fn default_endpoint_kind() -> String {
    "provider".to_owned()
}

/// Optional per-endpoint auth override.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    Bearer,
    ApiKey,
    /// Forward the inbound client's bearer identity to the upstream. This is for
    /// Codex subscription passthrough and is intentionally restricted to the
    /// ChatGPT Codex backend or loopback development endpoints.
    ClientBearer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointProtocol {
    AnthropicMessages,
    #[serde(rename = "openai_chat", alias = "open_ai_chat")]
    OpenAIChat,
    #[serde(rename = "openai_responses", alias = "responses")]
    OpenAIResponses,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RouteTarget {
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    /// v2 (`rayline`-only): which rayline decider runs — `"rayline-cloud"` (the
    /// hosted RCR) or `"rayline-local"` (the on-device LSR). `None` defaults to
    /// `rayline-cloud` behavior. Ignored for non-`rayline` endpoints. The LSR's
    /// own routing does not read this; the CLI inspects it to wire the run.
    #[serde(default)]
    pub router: Option<String>,
    /// v2 (`router: rayline-cloud` only): local model ids the hosted RCR may
    /// redirect this class to ("may-local"). A non-empty list turns may-local ON
    /// and advertises `local_models[0]`; today only the first entry is used.
    /// `N/A` (ignored) for `rayline-local` and for `anthropic`/`local` endpoints.
    #[serde(default)]
    pub local_models: Vec<String>,
}

impl RouteTarget {
    fn local(model: impl Into<String>) -> Self {
        Self {
            endpoint: "local".to_owned(),
            model: model.into(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RoutesConfig {
    #[serde(default)]
    pub main: Option<RouteTarget>,
    #[serde(default)]
    pub subagent: Option<RouteTarget>,
    #[serde(default)]
    pub default: Option<RouteTarget>,
    #[serde(default)]
    pub model_routes: HashMap<String, RouteTarget>,
    #[serde(default)]
    pub subagents: HashMap<String, RouteTarget>,
}

#[derive(Clone)]
struct AppState {
    opts: Arc<LocalRouterOptions>,
    config: Arc<RouterConfig>,
    http: reqwest::Client,
    http_ipv4: reqwest::Client,
    route_counter: Arc<AtomicU64>,
    started_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RouteDecision {
    target: RouteSelection,
    requested_model: String,
    selected_model: String,
    policy: String,
    task_class: String,
    route_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RouteSelection {
    Local,
    Endpoint(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyntheticOutputKind {
    Message,
    Compaction,
}

struct OpenAIPassthroughRequest<'a> {
    inbound_headers: &'a HeaderMap,
    method: Method,
    inbound_path_and_query: &'a str,
    body: Bytes,
    parsed: &'a Value,
    request_id: &'a str,
    rewrite_model: bool,
}

pub async fn serve(opts: LocalRouterOptions) -> Result<()> {
    let config = load_config(&opts)?;
    let mut subagent_keys = config.routes.subagents.keys().cloned().collect::<Vec<_>>();
    subagent_keys.sort();
    let endpoint_ids = config
        .endpoints
        .iter()
        .map(|endpoint| endpoint.id.clone())
        .collect::<Vec<_>>();
    let config_source = opts
        .config_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| std::env::var(CONFIG_ENV).ok())
        .unwrap_or_else(|| "<default>".to_owned());
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), opts.port);
    let listener = TcpListener::bind(addr).await?;
    info!(
        "local router listening on 127.0.0.1:{} (adapter :{}, local_model={}, config={}, endpoints=[{}], subagents=[{}], main={}, subagent_default={})",
        opts.port,
        opts.local_adapter_port,
        opts.local_model_id,
        config_source,
        endpoint_ids.join(","),
        subagent_keys.join(","),
        route_summary(config.routes.main.as_ref()),
        route_summary(config.routes.subagent.as_ref())
    );
    let state = AppState {
        opts: Arc::new(opts),
        config: Arc::new(config),
        http: outbound_http_client(false)?,
        http_ipv4: outbound_http_client(true)?,
        route_counter: Arc::new(AtomicU64::new(1)),
        started_at: chrono_like_now(),
    };
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(state, req).await) }
            });
            if let Err(error) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                warn!("local router connection error: {error}");
            }
        });
    }
}

fn route_summary(route: Option<&RouteTarget>) -> String {
    route
        .map(|route| format!("{}:{}", route.endpoint, route.model))
        .unwrap_or_else(|| "<default>".to_owned())
}

fn load_config(opts: &LocalRouterOptions) -> Result<RouterConfig> {
    let path = opts
        .config_path
        .clone()
        .map(|path| (path, "--router-config-path"))
        .or_else(|| std::env::var_os(CONFIG_ENV).map(|path| (PathBuf::from(path), CONFIG_ENV)));
    let mut config = default_config(&opts.local_model_id);
    if let Some((path, source)) = path {
        if !path.is_file() {
            return Err(anyhow!(
                "{source} points to missing router config {}",
                path.display()
            ));
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let overrides = serde_json::from_str::<RouterConfig>(&raw)
            .with_context(|| format!("parse router config {}", path.display()))?;
        merge_config(&mut config, overrides);
    }
    apply_env_overrides(&mut config);
    normalize_config(&mut config, &opts.local_model_id)?;
    Ok(config)
}

fn merge_config(config: &mut RouterConfig, overrides: RouterConfig) {
    for endpoint in overrides.endpoints {
        if let Some(existing) = config
            .endpoints
            .iter_mut()
            .find(|existing| existing.id == endpoint.id)
        {
            *existing = endpoint;
        } else {
            config.endpoints.push(endpoint);
        }
    }

    if overrides.routes.main.is_some() {
        config.routes.main = overrides.routes.main;
    }
    if overrides.routes.subagent.is_some() {
        config.routes.subagent = overrides.routes.subagent;
    }
    if overrides.routes.default.is_some() {
        config.routes.default = overrides.routes.default;
    }
    config
        .routes
        .model_routes
        .extend(overrides.routes.model_routes);
    config.routes.subagents.extend(overrides.routes.subagents);
}

fn normalize_config(config: &mut RouterConfig, local_model_id: &str) -> Result<()> {
    for endpoint in &config.endpoints {
        validate_endpoint(endpoint)?;
    }
    if let Some(route) = config.routes.main.as_mut() {
        normalize_route_target(route, local_model_id)?;
    }
    if let Some(route) = config.routes.subagent.as_mut() {
        normalize_route_target(route, local_model_id)?;
    }
    if let Some(route) = config.routes.default.as_mut() {
        normalize_route_target(route, local_model_id)?;
    }
    for route in config.routes.model_routes.values_mut() {
        normalize_route_target(route, local_model_id)?;
    }
    for route in config.routes.subagents.values_mut() {
        normalize_route_target(route, local_model_id)?;
    }
    Ok(())
}

fn validate_endpoint(endpoint: &EndpointConfig) -> Result<()> {
    if endpoint.auth != Some(AuthMode::ClientBearer) {
        return Ok(());
    }
    if endpoint.api_key_env.is_some() {
        return Err(anyhow!(
            "endpoint {:?} uses auth=client_bearer and must not set api_key_env",
            endpoint.id
        ));
    }
    if !client_bearer_base_url_allowed(&endpoint.base_url) {
        return Err(anyhow!(
            "endpoint {:?} uses auth=client_bearer, which is only allowed for {} or loopback development endpoints",
            endpoint.id,
            "https://chatgpt.com/backend-api/codex"
        ));
    }
    Ok(())
}

fn client_bearer_base_url_allowed(base_url: &str) -> bool {
    let normalized = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    normalized == "https://chatgpt.com/backend-api/codex"
        || normalized.starts_with("http://127.0.0.1:")
        || normalized.starts_with("http://localhost:")
        || normalized.starts_with("http://[::1]:")
}

fn normalize_route_target(route: &mut RouteTarget, local_model_id: &str) -> Result<()> {
    route.endpoint = route.endpoint.trim().to_owned();
    route.model = route.model.trim().to_owned();
    if route.endpoint.is_empty() {
        return Err(anyhow!("route endpoint must not be empty"));
    }
    if route.model.is_empty() {
        if route.endpoint == "local" {
            route.model = local_model_id.to_owned();
        } else {
            return Err(anyhow!(
                "route to endpoint {:?} must include a model",
                route.endpoint
            ));
        }
    }
    Ok(())
}

fn default_config(local_model_id: &str) -> RouterConfig {
    RouterConfig {
        endpoints: vec![
            EndpointConfig {
                id: "anthropic".to_owned(),
                kind: "provider".to_owned(),
                protocol: EndpointProtocol::AnthropicMessages,
                base_url: "https://api.anthropic.com".to_owned(),
                api_key_env: Some("ANTHROPIC_API_KEY".to_owned()),
                models: vec!["claude-sonnet-4-6".to_owned(), "claude-opus-4-7".to_owned()],
                headers: HashMap::new(),
                auth: None,
            },
            EndpointConfig {
                id: "openai".to_owned(),
                kind: "provider".to_owned(),
                protocol: EndpointProtocol::OpenAIChat,
                base_url: "https://api.openai.com/v1".to_owned(),
                api_key_env: Some("OPENAI_API_KEY".to_owned()),
                models: vec!["gpt-5.2".to_owned(), "gpt-5.2-codex".to_owned()],
                headers: HashMap::new(),
                auth: None,
            },
            EndpointConfig {
                // OpenRouter exposes a native Anthropic Messages endpoint at
                // /v1/messages that accepts bearer auth and streams native
                // Anthropic SSE, so prefer it over the openai_chat shim.
                id: "openrouter".to_owned(),
                kind: "provider".to_owned(),
                protocol: EndpointProtocol::AnthropicMessages,
                base_url: "https://openrouter.ai/api".to_owned(),
                api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
                models: vec!["anthropic/claude-sonnet-4.6".to_owned()],
                headers: HashMap::new(),
                auth: Some(AuthMode::Bearer),
            },
        ],
        routes: RoutesConfig {
            main: Some(RouteTarget {
                endpoint: "anthropic".to_owned(),
                model: "claude-sonnet-4-6".to_owned(),
                ..Default::default()
            }),
            subagent: Some(RouteTarget::local(local_model_id)),
            default: Some(RouteTarget {
                endpoint: "anthropic".to_owned(),
                model: "claude-sonnet-4-6".to_owned(),
                ..Default::default()
            }),
            model_routes: HashMap::from([
                (
                    DEFAULT_SUBAGENT_MODEL.to_owned(),
                    RouteTarget::local(local_model_id),
                ),
                (
                    "rayline-local".to_owned(),
                    RouteTarget::local(local_model_id),
                ),
            ]),
            subagents: HashMap::new(),
        },
    }
}

fn apply_env_overrides(config: &mut RouterConfig) {
    if let Ok(model) = std::env::var(MAIN_MODEL_ENV) {
        let endpoint = std::env::var(MAIN_ENDPOINT_ENV).unwrap_or_else(|_| {
            config
                .routes
                .main
                .as_ref()
                .map(|route| route.endpoint.clone())
                .unwrap_or_else(|| "anthropic".to_owned())
        });
        config.routes.main = Some(RouteTarget {
            endpoint,
            model,
            ..Default::default()
        });
    } else if let Ok(endpoint) = std::env::var(MAIN_ENDPOINT_ENV) {
        if let Some(route) = config.routes.main.as_mut() {
            route.endpoint = endpoint;
        }
    }

    if let Ok(model) = std::env::var(SUBAGENT_MODEL_ENV) {
        let endpoint = std::env::var(SUBAGENT_ENDPOINT_ENV).unwrap_or_else(|_| {
            config
                .routes
                .subagent
                .as_ref()
                .map(|route| route.endpoint.clone())
                .unwrap_or_else(|| "local".to_owned())
        });
        config.routes.subagent = Some(RouteTarget {
            endpoint,
            model,
            ..Default::default()
        });
    } else if let Ok(endpoint) = std::env::var(SUBAGENT_ENDPOINT_ENV) {
        if let Some(route) = config.routes.subagent.as_mut() {
            route.endpoint = endpoint;
        }
    }
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn json_response(status: StatusCode, value: Value) -> Response<BoxBody> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(body))
        .unwrap()
}

async fn handle(state: AppState, req: Request<Incoming>) -> Response<BoxBody> {
    let path = req.uri().path().to_owned();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| path.clone());
    let method = req.method().clone();
    match (method, path.as_str()) {
        (Method::GET, "/healthz") => healthz_response(&state),
        (Method::GET, "/v1/models" | "/v1/models/") => {
            models_response(&state, req.headers(), &path_and_query).await
        }
        (Method::GET, path) if path.starts_with("/v1/models/") => model_response(&state, path),
        (Method::POST, "/v1/messages/count_tokens") => count_tokens_response(req).await,
        (Method::POST, "/v1/responses/count_tokens") => count_responses_tokens_response(req).await,
        (Method::POST, "/v1/usage/update") => json_response(StatusCode::OK, json!({"ok": true})),
        (Method::GET, "/v1/settings") => json_response(
            StatusCode::OK,
            json!({"settings": {"enable_local_router": true, "local_gateway_port": state.opts.local_adapter_port}}),
        ),
        (Method::PATCH, "/v1/settings") => json_response(
            StatusCode::OK,
            json!({"settings": {"enable_local_router": true, "local_gateway_port": state.opts.local_adapter_port}}),
        ),
        (Method::POST, "/v1/messages") => match handle_messages(state, req).await {
            Ok(response) => response,
            Err(error) => {
                warn!("local router /v1/messages error: {error}");
                json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({"type":"error","error":{"type":"api_error","message":error.to_string()}}),
                )
            }
        },
        (Method::POST, "/v1/responses") => match handle_responses(state, req).await {
            Ok(response) => response,
            Err(error) => {
                warn!("local router /v1/responses error: {error:#}");
                json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({"error":{"type":"api_error","message":error.to_string()}}),
                )
            }
        },
        (Method::POST, "/v1/responses/compact")
        | (Method::POST, "/v1/memories/trace_summarize")
        | (Method::POST, "/v1/images/generations")
        | (Method::POST, "/v1/images/edits")
        | (Method::POST, "/v1/alpha/search") => match handle_openai_auxiliary(state, req).await {
            Ok(response) => response,
            Err(error) => {
                warn!("local router OpenAI auxiliary endpoint error: {error:#}");
                json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({"error":{"type":"api_error","message":error.to_string()}}),
                )
            }
        },
        (method, path)
            if path.starts_with("/v1/responses/")
                && matches!(method, Method::GET | Method::DELETE | Method::POST) =>
        {
            match handle_openai_passthrough_family(state, req).await {
                Ok(response) => response,
                Err(error) => {
                    warn!("local router Responses passthrough endpoint error: {error:#}");
                    json_response(
                        StatusCode::BAD_GATEWAY,
                        json!({"error":{"type":"api_error","message":error.to_string()}}),
                    )
                }
            }
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(full_body("not found"))
            .unwrap(),
    }
}

fn outbound_http_client(ipv4_only: bool) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    let mut builder = if ipv4_only {
        builder
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .http1_only()
            .connect_timeout(Duration::from_secs(10))
    } else {
        builder
    };
    if ipv4_only {
        let addrs = ("chatgpt.com", 443)
            .to_socket_addrs()
            .map(|addrs| {
                addrs
                    .filter(|addr| addr.is_ipv4())
                    .collect::<Vec<SocketAddr>>()
            })
            .unwrap_or_default();
        if !addrs.is_empty() {
            builder = builder.resolve_to_addrs("chatgpt.com", &addrs);
        }
    }
    Ok(builder.build()?)
}

fn healthz_response(state: &AppState) -> Response<BoxBody> {
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "runtime": "rayline-local-router",
            "router_url": format!("http://127.0.0.1:{}", state.opts.port),
            "local_adapter_port": state.opts.local_adapter_port,
            "local_model_id": state.opts.local_model_id,
            "startedAt": state.started_at,
        }),
    )
}

async fn models_response(
    state: &AppState,
    inbound_headers: &HeaderMap,
    path_and_query: &str,
) -> Response<BoxBody> {
    if let Some(endpoint) = client_bearer_models_endpoint(&state.config) {
        match proxy_client_bearer_models(state, endpoint, inbound_headers, path_and_query).await {
            Ok(response) => return response,
            Err(error) => warn!("local router client-bearer /models proxy failed: {error:#}"),
        }
    }
    static_models_response(state)
}

fn static_models_response(state: &AppState) -> Response<BoxBody> {
    json_response(StatusCode::OK, static_models_value(state))
}

fn static_model_values(state: &AppState) -> Vec<Value> {
    let mut models = vec![
        model_json(DEFAULT_VIRTUAL_MODEL),
        model_json(DEFAULT_SUBAGENT_MODEL),
        model_json("rayline-codex"),
        model_json("rayline-local"),
    ];
    for endpoint in &state.config.endpoints {
        for model in &endpoint.models {
            models.push(model_json(model));
        }
    }
    models
}

fn static_models_value(state: &AppState) -> Value {
    let models = static_model_values(state);
    let codex_models = models
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .map(codex_model_json)
        .collect::<Vec<_>>();
    json!({
        "object": "list",
        "data": models,
        "models": codex_models,
        "has_more": false,
    })
}

fn client_bearer_models_endpoint(config: &RouterConfig) -> Option<&EndpointConfig> {
    let route_endpoint = [
        config.routes.main.as_ref(),
        config.routes.default.as_ref(),
        config.routes.subagent.as_ref(),
    ]
    .into_iter()
    .flatten()
    .find_map(|route| endpoint_by_id(config, &route.endpoint))
    .filter(|endpoint| {
        endpoint.protocol == EndpointProtocol::OpenAIResponses
            && endpoint.auth == Some(AuthMode::ClientBearer)
    });
    route_endpoint.or_else(|| {
        config.endpoints.iter().find(|endpoint| {
            endpoint.protocol == EndpointProtocol::OpenAIResponses
                && endpoint.auth == Some(AuthMode::ClientBearer)
        })
    })
}

fn endpoint_by_id<'a>(config: &'a RouterConfig, id: &str) -> Option<&'a EndpointConfig> {
    config.endpoints.iter().find(|endpoint| endpoint.id == id)
}

async fn proxy_client_bearer_models(
    state: &AppState,
    endpoint: &EndpointConfig,
    inbound_headers: &HeaderMap,
    path_and_query: &str,
) -> Result<Response<BoxBody>> {
    let upstream_path = path_and_query
        .strip_prefix("/v1/")
        .or_else(|| path_and_query.strip_prefix('/'))
        .unwrap_or(path_and_query);
    let url = format!(
        "{}/{}",
        endpoint.base_url.trim_end_matches('/'),
        upstream_path
    );
    let mut outbound = state.http_ipv4.get(url).timeout(Duration::from_secs(2));
    copy_openai_passthrough_headers(&mut outbound, inbound_headers, false, true);
    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Bearer)?;
    let resp = outbound.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return response_from_reqwest(resp, status, None, None, None, None).await;
    }
    let headers = resp.headers().clone();
    let body = resp.bytes().await?;
    let mut value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => return Ok(raw_response(status, &headers, body)),
    };
    merge_static_models(&mut value, state);
    Ok(json_response(StatusCode::OK, value))
}

fn raw_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Bytes,
) -> Response<BoxBody> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if is_hop_by_hop_str(name.as_str()) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    builder.body(full_body(body)).unwrap()
}

fn merge_static_models(value: &mut Value, state: &AppState) {
    let Some(object) = value.as_object_mut() else {
        *value = static_models_value(state);
        return;
    };

    let static_models = static_model_values(state);
    let data = object
        .entry("data")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(data) = data.as_array_mut() {
        for model in &static_models {
            let Some(id) = model.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !data
                .iter()
                .any(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
            {
                data.push(model.clone());
            }
        }
    }

    let models = object
        .entry("models")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(models) = models.as_array_mut() {
        for model in static_models
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .map(codex_model_json)
        {
            let Some(slug) = model.get("slug").and_then(Value::as_str) else {
                continue;
            };
            if !models
                .iter()
                .any(|existing| existing.get("slug").and_then(Value::as_str) == Some(slug))
            {
                models.push(model);
            }
        }
    }

    object
        .entry("object")
        .or_insert_with(|| Value::String("list".to_owned()));
    object
        .entry("has_more")
        .or_insert_with(|| Value::Bool(false));
}

fn model_response(state: &AppState, path: &str) -> Response<BoxBody> {
    let Some(model) = path.strip_prefix("/v1/models/").filter(|id| !id.is_empty()) else {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({"type":"error","error":{"type":"not_found_error","message":"model not found"}}),
        );
    };
    let model = percent_decode_minimal(model);
    if model == DEFAULT_VIRTUAL_MODEL
        || model == DEFAULT_SUBAGENT_MODEL
        || model == "rayline-codex"
        || model == "rayline-local"
        || state
            .config
            .endpoints
            .iter()
            .any(|endpoint| endpoint.models.iter().any(|m| m == &model))
    {
        return json_response(StatusCode::OK, model_json(&model));
    }
    json_response(
        StatusCode::NOT_FOUND,
        json!({"type":"error","error":{"type":"not_found_error","message":"model not found"}}),
    )
}

fn model_json(id: &str) -> Value {
    json!({
        "id": id,
        "object": "model",
        "type": "model",
        "display_name": id,
    })
}

fn codex_model_json(id: &str) -> Value {
    json!({
        "slug": id,
        "display_name": id,
        "description": "Rayline-routed model",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Low"},
            {"effort": "medium", "description": "Medium"},
            {"effort": "high", "description": "High"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "additional_speed_tiers": [],
        "availability_nux": Value::Null,
        "upgrade": Value::Null,
        "base_instructions": "",
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "default_verbosity": Value::Null,
        "apply_patch_tool_type": Value::Null,
        "truncation_policy": {"mode": "tokens", "limit": 128000},
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": false,
        "context_window": 128000,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false
    })
}

async fn count_tokens_response(req: Request<Incoming>) -> Response<BoxBody> {
    let bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"type":"error","error":{"type":"invalid_request_error","message":format!("read body: {error}")}}),
            );
        }
    };
    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    json_response(
        StatusCode::OK,
        json!({"input_tokens": approximate_input_tokens(&value)}),
    )
}

async fn count_responses_tokens_response(req: Request<Incoming>) -> Response<BoxBody> {
    let bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error":{"type":"invalid_request_error","message":error.to_string()}}),
            );
        }
    };
    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    json_response(
        StatusCode::OK,
        json!({"input_tokens": approximate_responses_input_tokens(&value)}),
    )
}

async fn handle_messages(state: AppState, req: Request<Incoming>) -> Result<Response<BoxBody>> {
    let t_start = Instant::now();
    let headers = req.headers().clone();
    let body = req.into_body().collect().await?.to_bytes();
    let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let decision = select_route_with_warn(&state, &headers, &parsed);
    let request_id = headers
        .get(REQUEST_ID_HEADER)
        .and_then(header_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(new_request_id);
    let agent_id = headers
        .get(CLAUDE_CODE_AGENT_ID_HEADER)
        .and_then(header_str)
        .unwrap_or("<none>");
    let agent_type = headers
        .get(RAYLINE_AGENT_TYPE_HEADER)
        .and_then(header_str)
        .unwrap_or("<none>");
    info!(
        "local route {} requested={} selected={} policy={} task={} agent_id={} agent_type={} elapsed_ms={}",
        route_target_label(&decision.target),
        decision.requested_model,
        decision.selected_model,
        decision.policy,
        decision.task_class,
        agent_id,
        agent_type,
        t_start.elapsed().as_millis()
    );
    if let Some(metrics) = state.opts.metrics.as_ref() {
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: request_id.clone(),
            route_id: Some(decision.route_id.clone()),
            target: match &decision.target {
                RouteSelection::Local => "local".to_owned(),
                RouteSelection::Endpoint(_) => "remote".to_owned(),
            },
            endpoint_id: match &decision.target {
                RouteSelection::Local => Some("local".to_owned()),
                RouteSelection::Endpoint(endpoint_id) => Some(endpoint_id.clone()),
            },
            selected_model: Some(decision.selected_model.clone()),
            requested_model: Some(decision.requested_model.clone()),
            policy: Some(decision.policy.clone()),
            task_class: Some(decision.task_class.clone()),
            agent_id: (agent_id != "<none>").then(|| agent_id.to_owned()),
            agent_type: (agent_type != "<none>").then(|| agent_type.to_owned()),
        });
    }
    match &decision.target {
        RouteSelection::Local => Ok(local_redirect_response(&state, &decision, &request_id)),
        RouteSelection::Endpoint(endpoint_id) => {
            let endpoint = match state
                .config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == *endpoint_id)
            {
                Some(endpoint) => endpoint,
                None => {
                    record_request_error(
                        state.opts.metrics.as_ref(),
                        &request_id,
                        None,
                        format!("endpoint {endpoint_id:?} not found"),
                    );
                    return Err(anyhow!("endpoint {endpoint_id:?} not found"));
                }
            };
            let response = match endpoint.protocol {
                EndpointProtocol::AnthropicMessages => {
                    forward_anthropic_endpoint(
                        &state,
                        endpoint,
                        &decision,
                        &headers,
                        body,
                        &request_id,
                        approximate_input_tokens(&parsed),
                    )
                    .await
                }
                EndpointProtocol::OpenAIChat => {
                    forward_openai_chat_endpoint(&state, endpoint, &decision, parsed, &request_id)
                        .await
                }
                EndpointProtocol::OpenAIResponses => Err(anyhow!(
                    "endpoint {:?} uses openai_responses, which is only supported for /v1/responses requests",
                    endpoint.id
                )),
            };
            if let Err(error) = response.as_ref() {
                record_request_error(state.opts.metrics.as_ref(), &request_id, None, error);
            }
            response
        }
    }
}

async fn handle_responses(state: AppState, req: Request<Incoming>) -> Result<Response<BoxBody>> {
    let t_start = Instant::now();
    let headers = req.headers().clone();
    let body = req.into_body().collect().await?.to_bytes();
    let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let decision = select_route_with_warn(&state, &headers, &parsed);
    let request_id = request_id_from_headers(&headers);
    let output_kind = responses_output_kind(&parsed);
    let subagent = headers
        .get(OPENAI_SUBAGENT_HEADER)
        .and_then(header_str)
        .unwrap_or("<none>");
    info!(
        "codex route {} requested={} selected={} policy={} task={} subagent={} elapsed_ms={}",
        route_target_label(&decision.target),
        decision.requested_model,
        decision.selected_model,
        decision.policy,
        decision.task_class,
        subagent,
        t_start.elapsed().as_millis()
    );
    if let Some(metrics) = state.opts.metrics.as_ref() {
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: request_id.clone(),
            route_id: Some(decision.route_id.clone()),
            target: match &decision.target {
                RouteSelection::Local => "local".to_owned(),
                RouteSelection::Endpoint(_) => "remote".to_owned(),
            },
            endpoint_id: match &decision.target {
                RouteSelection::Local => Some("local".to_owned()),
                RouteSelection::Endpoint(endpoint_id) => Some(endpoint_id.clone()),
            },
            selected_model: Some(decision.selected_model.clone()),
            requested_model: Some(decision.requested_model.clone()),
            policy: Some(decision.policy.clone()),
            task_class: Some(decision.task_class.clone()),
            agent_id: (subagent != "<none>").then(|| subagent.to_owned()),
            agent_type: (subagent != "<none>").then(|| subagent.to_owned()),
        });
    }

    let response = match &decision.target {
        RouteSelection::Local => {
            forward_responses_to_local_adapter(&state, &decision, &parsed, &request_id, output_kind)
                .await
        }
        RouteSelection::Endpoint(endpoint_id) => {
            let endpoint = state
                .config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == *endpoint_id)
                .ok_or_else(|| anyhow!("endpoint {endpoint_id:?} not found"))?;
            match endpoint.protocol {
                EndpointProtocol::OpenAIResponses => {
                    forward_openai_responses_endpoint(
                        &state,
                        endpoint,
                        &decision,
                        &headers,
                        body,
                        &parsed,
                        &request_id,
                    )
                    .await
                }
                EndpointProtocol::AnthropicMessages => {
                    forward_responses_to_anthropic_endpoint(
                        &state,
                        endpoint,
                        &decision,
                        &headers,
                        &parsed,
                        &request_id,
                        output_kind,
                    )
                    .await
                }
                EndpointProtocol::OpenAIChat => {
                    forward_responses_to_openai_chat_endpoint(
                        &state,
                        endpoint,
                        &decision,
                        &parsed,
                        &request_id,
                        output_kind,
                    )
                    .await
                }
            }
        }
    };
    if let Err(error) = response.as_ref() {
        record_request_error(state.opts.metrics.as_ref(), &request_id, None, error);
    }
    response
}

async fn handle_openai_passthrough_family(
    state: AppState,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>> {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let headers = req.headers().clone();
    let body = req.into_body().collect().await?.to_bytes();
    let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let decision = select_route_with_warn(&state, &headers, &parsed);
    let request_id = request_id_from_headers(&headers);
    forward_openai_family_or_unsupported(
        &state,
        &decision,
        OpenAIPassthroughRequest {
            inbound_headers: &headers,
            method,
            inbound_path_and_query: &path_and_query,
            body,
            parsed: &parsed,
            request_id: &request_id,
            rewrite_model: true,
        },
    )
    .await
}

async fn handle_openai_auxiliary(
    state: AppState,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>> {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let headers = req.headers().clone();
    let body = req.into_body().collect().await?.to_bytes();
    let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let decision = select_route_with_warn(&state, &headers, &parsed);
    let request_id = request_id_from_headers(&headers);

    if let Some(response) = maybe_handle_synthetic_auxiliary(
        &state,
        &decision,
        &headers,
        &path_and_query,
        &parsed,
        &request_id,
    )
    .await?
    {
        return Ok(response);
    }

    forward_openai_family_or_unsupported(
        &state,
        &decision,
        OpenAIPassthroughRequest {
            inbound_headers: &headers,
            method,
            inbound_path_and_query: &path_and_query,
            body,
            parsed: &parsed,
            request_id: &request_id,
            rewrite_model: true,
        },
    )
    .await
}

async fn forward_openai_family_or_unsupported(
    state: &AppState,
    decision: &RouteDecision,
    passthrough: OpenAIPassthroughRequest<'_>,
) -> Result<Response<BoxBody>> {
    let RouteSelection::Endpoint(endpoint_id) = &decision.target else {
        return Ok(unsupported_openai_endpoint_response(
            decision,
            passthrough.inbound_path_and_query,
        ));
    };
    let endpoint = state
        .config
        .endpoints
        .iter()
        .find(|endpoint| endpoint.id == *endpoint_id)
        .ok_or_else(|| anyhow!("endpoint {endpoint_id:?} not found"))?;
    if endpoint.protocol != EndpointProtocol::OpenAIResponses {
        return Ok(unsupported_openai_endpoint_response(
            decision,
            passthrough.inbound_path_and_query,
        ));
    }
    forward_openai_passthrough_endpoint(state, endpoint, decision, passthrough).await
}

fn unsupported_openai_endpoint_response(
    decision: &RouteDecision,
    inbound_path_and_query: &str,
) -> Response<BoxBody> {
    let mut response = json_response(
        StatusCode::NOT_IMPLEMENTED,
        json!({
            "error": {
                "type": "unsupported_endpoint",
                "message": format!(
                    "{inbound_path_and_query} requires an endpoint using protocol=openai_responses for this route"
                )
            }
        }),
    );
    add_decision_headers(response.headers_mut(), decision);
    response
}

async fn maybe_handle_synthetic_auxiliary(
    state: &AppState,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    path_and_query: &str,
    body: &Value,
    request_id: &str,
) -> Result<Option<Response<BoxBody>>> {
    if let RouteSelection::Endpoint(endpoint_id) = &decision.target
        && state
            .config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == *endpoint_id)
            .is_some_and(|endpoint| endpoint.protocol == EndpointProtocol::OpenAIResponses)
    {
        return Ok(None);
    }
    let path = path_and_query.split('?').next().unwrap_or(path_and_query);
    match path {
        "/v1/responses/compact" => synthetic_compact_response(
            state,
            decision,
            inbound_headers,
            body,
            request_id,
            SyntheticOutputKind::Compaction,
        )
        .await
        .map(Some),
        "/v1/memories/trace_summarize" => {
            synthetic_memory_summarize_response(state, decision, inbound_headers, body, request_id)
                .await
                .map(Some)
        }
        _ => Ok(None),
    }
}

async fn synthetic_compact_response(
    state: &AppState,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    body: &Value,
    request_id: &str,
    output_kind: SyntheticOutputKind,
) -> Result<Response<BoxBody>> {
    let mut request_body = body.clone();
    if let Some(obj) = request_body.as_object_mut() {
        obj.insert("stream".to_owned(), Value::Bool(false));
    }
    let (anthropic_message, estimated_input_tokens) =
        run_synthetic_unary_response(state, decision, inbound_headers, &request_body, request_id)
            .await?;
    let text = anthropic_message_text(&anthropic_message);
    let item = match output_kind {
        SyntheticOutputKind::Compaction => json!({
            "type": "compaction",
            "encrypted_content": text,
        }),
        SyntheticOutputKind::Message => json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
            "phase": "final_answer",
        }),
    };
    let input_tokens = anthropic_message
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(estimated_input_tokens);
    let output_tokens = anthropic_message
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| (content_to_text(&item).len() as u64 / 4).max(1));
    record_remote_completion(
        state.opts.metrics.as_ref(),
        request_id,
        StatusCode::OK.as_u16(),
        Some(input_tokens),
        Some(output_tokens),
        Some(decision.selected_model.clone()),
    );
    let mut response = json_response(StatusCode::OK, json!({"output": [item]}));
    add_decision_headers(response.headers_mut(), decision);
    Ok(response)
}

async fn synthetic_memory_summarize_response(
    state: &AppState,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    body: &Value,
    request_id: &str,
) -> Result<Response<BoxBody>> {
    let request_body = memory_summarize_to_responses_request(body, &decision.requested_model);
    let (anthropic_message, estimated_input_tokens) =
        run_synthetic_unary_response(state, decision, inbound_headers, &request_body, request_id)
            .await?;
    let text = anthropic_message_text(&anthropic_message);
    let output = parse_memory_summarize_output(body, &text);
    let output_tokens = anthropic_message
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| (text.len() as u64 / 4).max(1));
    record_remote_completion(
        state.opts.metrics.as_ref(),
        request_id,
        StatusCode::OK.as_u16(),
        anthropic_message
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .or(Some(estimated_input_tokens)),
        Some(output_tokens),
        Some(decision.selected_model.clone()),
    );
    let mut response = json_response(StatusCode::OK, json!({"output": output}));
    add_decision_headers(response.headers_mut(), decision);
    Ok(response)
}

async fn run_synthetic_unary_response(
    state: &AppState,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    body: &Value,
    request_id: &str,
) -> Result<(Value, u64)> {
    let anthropic = responses_to_anthropic_request(body, &decision.selected_model);
    let estimated_input_tokens = approximate_input_tokens(&anthropic);
    match &decision.target {
        RouteSelection::Local => {
            let url = format!(
                "http://127.0.0.1:{}/api/v1/messages?usage_doc_id={}&rayline_request_id={}",
                state.opts.local_adapter_port,
                query_escape(&decision.route_id),
                query_escape(request_id)
            );
            let resp = state
                .http
                .post(url)
                .header("content-type", "application/json")
                .json(&anthropic)
                .send()
                .await?;
            parse_anthropic_unary_response(resp, estimated_input_tokens).await
        }
        RouteSelection::Endpoint(endpoint_id) => {
            let endpoint = state
                .config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == *endpoint_id)
                .ok_or_else(|| anyhow!("endpoint {endpoint_id:?} not found"))?;
            match endpoint.protocol {
                EndpointProtocol::AnthropicMessages => {
                    let url = format!("{}/v1/messages", endpoint.base_url.trim_end_matches('/'));
                    let mut outbound = state
                        .http
                        .post(url)
                        .header("content-type", "application/json")
                        .header(
                            "anthropic-version",
                            inbound_headers
                                .get("anthropic-version")
                                .and_then(header_str)
                                .unwrap_or("2023-06-01"),
                        )
                        .json(&anthropic);
                    if let Some(beta) = inbound_headers.get("anthropic-beta").and_then(header_str) {
                        outbound = outbound.header("anthropic-beta", beta);
                    }
                    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Anthropic)?;
                    let resp = outbound.send().await?;
                    parse_anthropic_unary_response(resp, estimated_input_tokens).await
                }
                EndpointProtocol::OpenAIChat => {
                    let request_body =
                        build_openai_chat_request(&anthropic, &decision.selected_model, false);
                    let url = format!(
                        "{}/chat/completions",
                        endpoint.base_url.trim_end_matches('/')
                    );
                    let mut outbound = state
                        .http
                        .post(url)
                        .header("content-type", "application/json")
                        .json(&request_body);
                    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Bearer)?;
                    let resp = outbound.send().await?;
                    if !resp.status().is_success() {
                        return Err(anyhow!("upstream returned HTTP {}", resp.status().as_u16()));
                    }
                    let value = resp.json::<Value>().await?;
                    Ok((
                        openai_chat_response_to_anthropic(&value, &decision.selected_model),
                        estimated_input_tokens,
                    ))
                }
                EndpointProtocol::OpenAIResponses => Err(anyhow!(
                    "native OpenAI Responses endpoint should have used passthrough"
                )),
            }
        }
    }
}

async fn parse_anthropic_unary_response(
    resp: reqwest::Response,
    estimated_input_tokens: u64,
) -> Result<(Value, u64)> {
    if !resp.status().is_success() {
        return Err(anyhow!("upstream returned HTTP {}", resp.status().as_u16()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        let text = resp.text().await?;
        Ok((
            anthropic_sse_text_to_message(&text, estimated_input_tokens),
            estimated_input_tokens,
        ))
    } else {
        Ok((resp.json::<Value>().await?, estimated_input_tokens))
    }
}

fn memory_summarize_to_responses_request(body: &Value, model: &str) -> Value {
    let traces = body.get("traces").cloned().unwrap_or_else(|| json!([]));
    json!({
        "model": model,
        "instructions": "Summarize the supplied memory traces. Return concise durable memory summaries. If possible, return JSON with an output array of objects containing trace_summary and memory_summary.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!("Summarize these memory traces:\n{}", traces)
            }]
        }],
        "stream": false,
        "max_output_tokens": 2048,
    })
}

fn parse_memory_summarize_output(request_body: &Value, text: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(output) = value.get("output").and_then(Value::as_array) {
            return Value::Array(output.clone());
        }
        if let Some(items) = value.as_array() {
            return Value::Array(items.clone());
        }
    }
    let trace_count = request_body
        .get("traces")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(1)
        .max(1);
    Value::Array(
        (0..trace_count)
            .map(|_| json!({"trace_summary": text, "memory_summary": text}))
            .collect(),
    )
}

fn anthropic_message_text(message: &Value) -> String {
    message
        .get("content")
        .map(content_to_text)
        .unwrap_or_else(|| content_to_text(message))
}

fn request_id_from_headers(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| headers.get(OPENAI_CLIENT_REQUEST_ID_HEADER))
        .and_then(header_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(new_request_id)
}

fn select_route(state: &AppState, headers: &HeaderMap, body: &Value) -> RouteDecision {
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(normalize_model_name)
        .unwrap_or_else(|| DEFAULT_VIRTUAL_MODEL.to_owned());
    let agent_id = headers
        .get(CLAUDE_CODE_AGENT_ID_HEADER)
        .or_else(|| headers.get(OPENAI_SUBAGENT_HEADER))
        .and_then(header_str);
    let agent_type = headers
        .get(RAYLINE_AGENT_TYPE_HEADER)
        .or_else(|| headers.get(OPENAI_SUBAGENT_HEADER))
        .and_then(header_str);
    // Guard: a bare `agent_id` header on a main-virtual-model request is
    // treated as stray and does NOT trigger subagent classification. Only a
    // confirmed `agent_type` (set by the proxy after successful meta-file
    // resolution) or the explicit `DEFAULT_SUBAGENT_MODEL` model name is a
    // reliable subagent signal when the main virtual model is requested.
    // This prevents CC from wrongly downgrading main-thread traffic to a
    // local/subagent endpoint when a stray agent-id header leaks through.
    let is_subagent = agent_type.is_some()
        || requested_model == DEFAULT_SUBAGENT_MODEL
        || (agent_id.is_some() && requested_model != DEFAULT_VIRTUAL_MODEL);
    let mut policy = if is_subagent { "subagent" } else { "main" }.to_owned();
    let mut route = if let Some(route) = state.config.routes.model_routes.get(&requested_model) {
        policy = format!("model:{requested_model}");
        route.clone()
    } else if is_subagent {
        if let Some((configured_key, route)) =
            subagent_route(&state.config.routes.subagents, agent_type, agent_id)
        {
            policy = format!("subagent:{configured_key}");
            route.clone()
        } else {
            state
                .config
                .routes
                .subagent
                .clone()
                .or_else(|| state.config.routes.default.clone())
                .unwrap_or_else(|| RouteTarget::local(&state.opts.local_model_id))
        }
    } else if requested_model == DEFAULT_VIRTUAL_MODEL {
        state
            .config
            .routes
            .main
            .clone()
            .or_else(|| state.config.routes.default.clone())
            .unwrap_or_else(default_main_route)
    } else if let Some((endpoint, model)) = route_direct_model(&state.config, &requested_model) {
        policy = "direct-model".to_owned();
        RouteTarget {
            endpoint,
            model,
            ..Default::default()
        }
    } else {
        state
            .config
            .routes
            .main
            .clone()
            .or_else(|| state.config.routes.default.clone())
            .unwrap_or_else(default_main_route)
    };

    if route.endpoint == "local" && !local_available(headers) {
        policy.push_str(":local-unavailable-fallback");
        route = state
            .config
            .routes
            .main
            .clone()
            .or_else(|| state.config.routes.default.clone())
            .unwrap_or_else(default_main_route);
    }

    let target = if route.endpoint == "local" {
        RouteSelection::Local
    } else {
        RouteSelection::Endpoint(route.endpoint.clone())
    };
    let route_id = format!(
        "local-{}",
        state.route_counter.fetch_add(1, Ordering::Relaxed)
    );
    RouteDecision {
        target,
        requested_model,
        selected_model: route.model,
        policy,
        task_class: if is_subagent {
            "subagent".to_owned()
        } else {
            "main".to_owned()
        },
        route_id,
    }
}

/// Thin wrapper around `select_route` that detects the inconsistent state where
/// `is_subagent` resolved to `true` but no `agent_id` header was present.
/// When that happens the function emits a `warn!` and increments the
/// `routing_uncertain` counter so operators notice rather than silently
/// degrading.  This state should be impossible in normal operation — its
/// occurrence means the `agent_type` header was set by some external caller
/// without a corresponding `agent_id`, or the subagent model name was used
/// without an `agent_id` header.
fn select_route_with_warn(state: &AppState, headers: &HeaderMap, body: &Value) -> RouteDecision {
    let decision = select_route(state, headers, body);
    let agent_id = headers
        .get(CLAUDE_CODE_AGENT_ID_HEADER)
        .or_else(|| headers.get(OPENAI_SUBAGENT_HEADER))
        .and_then(header_str);
    if decision.task_class == "subagent" && agent_id.is_none() {
        warn!(
            "local router: subagent flagged with no agent id — \
             agent_type header set without agent_id or DEFAULT_SUBAGENT_MODEL \
             used without agent_id header; routing uncertain"
        );
        if let Some(metrics) = state.opts.metrics.as_ref() {
            metrics.record(MetricsUpdate::RoutingUncertain {
                agent_id: "<none>".to_owned(),
            });
        }
    }
    decision
}

fn header_str(value: &HeaderValue) -> Option<&str> {
    value
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn subagent_route<'a>(
    routes: &'a HashMap<String, RouteTarget>,
    agent_type: Option<&str>,
    agent_id: Option<&str>,
) -> Option<(String, &'a RouteTarget)> {
    for key in [agent_type, agent_id].into_iter().flatten() {
        if let Some(route) = routes.get(key) {
            return Some((key.to_owned(), route));
        }
        if let Some((configured_key, route)) = routes
            .iter()
            .find(|(configured_key, _)| configured_key.eq_ignore_ascii_case(key))
        {
            return Some((configured_key.clone(), route));
        }
    }
    None
}

fn default_main_route() -> RouteTarget {
    RouteTarget {
        endpoint: "anthropic".to_owned(),
        model: "claude-sonnet-4-6".to_owned(),
        ..Default::default()
    }
}

fn route_direct_model(config: &RouterConfig, requested_model: &str) -> Option<(String, String)> {
    for endpoint in &config.endpoints {
        if endpoint.models.iter().any(|model| model == requested_model) {
            return Some((endpoint.id.clone(), requested_model.to_owned()));
        }
    }
    None
}

fn local_available(headers: &HeaderMap) -> bool {
    for name in ["x-rayline-local-available", "x-rayline-local-hint"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            return !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "no"
            );
        }
    }
    true
}

fn local_redirect_response(
    state: &AppState,
    decision: &RouteDecision,
    request_id: &str,
) -> Response<BoxBody> {
    let location = format!(
        "http://127.0.0.1:{}/api/v1/messages?usage_doc_id={}&rayline_request_id={}",
        state.opts.local_adapter_port,
        query_escape(&decision.route_id),
        query_escape(request_id)
    );
    let mut builder = Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header("location", location)
        .header(REQUEST_ID_HEADER, request_id);
    add_decision_headers(builder.headers_mut().unwrap(), decision);
    builder.body(full_body(Bytes::new())).unwrap()
}

fn query_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn forward_openai_responses_endpoint(
    state: &AppState,
    endpoint: &EndpointConfig,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    body: Bytes,
    parsed: &Value,
    request_id: &str,
) -> Result<Response<BoxBody>> {
    forward_openai_passthrough_endpoint(
        state,
        endpoint,
        decision,
        OpenAIPassthroughRequest {
            inbound_headers,
            method: Method::POST,
            inbound_path_and_query: "/v1/responses",
            body,
            parsed,
            request_id,
            rewrite_model: true,
        },
    )
    .await
}

async fn forward_openai_passthrough_endpoint(
    state: &AppState,
    endpoint: &EndpointConfig,
    decision: &RouteDecision,
    passthrough: OpenAIPassthroughRequest<'_>,
) -> Result<Response<BoxBody>> {
    let estimated_input_tokens = approximate_responses_input_tokens(passthrough.parsed);
    let mut outbound_value = passthrough.parsed.clone();
    let outbound_body = if passthrough.rewrite_model && outbound_value.is_object() {
        rewrite_body_model(&mut outbound_value, &decision.selected_model);
        serde_json::to_vec(&outbound_value).unwrap_or_else(|_| passthrough.body.to_vec())
    } else {
        passthrough.body.to_vec()
    };
    let upstream_path = passthrough
        .inbound_path_and_query
        .strip_prefix("/v1/")
        .or_else(|| passthrough.inbound_path_and_query.strip_prefix('/'))
        .unwrap_or(passthrough.inbound_path_and_query);
    let url = format!(
        "{}/{}",
        endpoint.base_url.trim_end_matches('/'),
        upstream_path
    );
    let http = if endpoint.auth == Some(AuthMode::ClientBearer) {
        &state.http_ipv4
    } else {
        &state.http
    };
    let mut outbound = http.request(passthrough.method, url).body(outbound_body);
    copy_openai_passthrough_headers(
        &mut outbound,
        passthrough.inbound_headers,
        !passthrough.body.is_empty(),
        endpoint.auth == Some(AuthMode::ClientBearer),
    );
    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Bearer)?;
    let resp = outbound.send().await?;
    let status = resp.status();
    response_from_reqwest(
        resp,
        status,
        Some(decision),
        state.opts.metrics.clone(),
        Some(passthrough.request_id.to_owned()),
        Some(estimated_input_tokens),
    )
    .await
}

fn copy_openai_passthrough_headers(
    outbound: &mut reqwest::RequestBuilder,
    inbound_headers: &HeaderMap,
    has_body: bool,
    forward_client_auth: bool,
) {
    if has_body {
        if let Some(content_type) = inbound_headers
            .get(hyper::header::CONTENT_TYPE)
            .and_then(header_str)
        {
            let next = std::mem::replace(outbound, reqwest::Client::new().get("http://127.0.0.1"));
            *outbound = next.header("content-type", content_type);
        } else {
            let next = std::mem::replace(outbound, reqwest::Client::new().get("http://127.0.0.1"));
            *outbound = next.header("content-type", "application/json");
        }
    }
    if let Some(accept) = inbound_headers
        .get(hyper::header::ACCEPT)
        .and_then(header_str)
    {
        let next = std::mem::replace(outbound, reqwest::Client::new().get("http://127.0.0.1"));
        *outbound = next.header("accept", accept);
    } else {
        let next = std::mem::replace(outbound, reqwest::Client::new().get("http://127.0.0.1"));
        *outbound = next.header("accept", "text/event-stream, application/json");
    }
    for (name, value) in inbound_headers {
        let name_str = name.as_str();
        if !is_openai_passthrough_request_header(name_str, forward_client_auth) {
            continue;
        }
        if let Some(value) = header_str(value) {
            let next = std::mem::replace(outbound, reqwest::Client::new().get("http://127.0.0.1"));
            *outbound = next.header(name_str, value);
        }
    }
}

fn is_openai_passthrough_request_header(name: &str, forward_client_auth: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if forward_client_auth
        && (lower == "authorization"
            || lower == "chatgpt-account-id"
            || lower == "x-openai-fedramp"
            || lower == "version")
    {
        return true;
    }
    lower == OPENAI_CLIENT_REQUEST_ID_HEADER
        || lower == OPENAI_SUBAGENT_HEADER
        || lower == "x-codex-parent-thread-id"
        || lower == "x-codex-window-id"
        || lower == "x-codex-beta-features"
        || lower == "x-codex-turn-state"
        || lower == "originator"
        || lower == "traceparent"
        || lower == "tracestate"
        || lower.starts_with("openai-")
        || lower.starts_with("x-openai-")
        || lower.starts_with("x-stainless-")
        || lower.starts_with("anthropic-dangerous-direct-browser-access")
}

async fn forward_responses_to_anthropic_endpoint(
    state: &AppState,
    endpoint: &EndpointConfig,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    body: &Value,
    request_id: &str,
    output_kind: SyntheticOutputKind,
) -> Result<Response<BoxBody>> {
    let anthropic = responses_to_anthropic_request(body, &decision.selected_model);
    let estimated_input_tokens = approximate_input_tokens(&anthropic);
    let url = format!("{}/v1/messages", endpoint.base_url.trim_end_matches('/'));
    let mut outbound = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .header(
            "anthropic-version",
            inbound_headers
                .get("anthropic-version")
                .and_then(header_str)
                .unwrap_or("2023-06-01"),
        )
        .json(&anthropic);
    if let Some(beta) = inbound_headers.get("anthropic-beta").and_then(header_str) {
        outbound = outbound.header("anthropic-beta", beta);
    }
    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Anthropic)?;
    let resp = outbound.send().await?;
    anthropic_response_to_responses(
        resp,
        decision,
        state.opts.metrics.clone(),
        request_id.to_owned(),
        estimated_input_tokens,
        output_kind,
        responses_wants_stream(body),
    )
    .await
}

async fn forward_responses_to_local_adapter(
    state: &AppState,
    decision: &RouteDecision,
    body: &Value,
    request_id: &str,
    output_kind: SyntheticOutputKind,
) -> Result<Response<BoxBody>> {
    let anthropic = responses_to_anthropic_request(body, &decision.selected_model);
    let estimated_input_tokens = approximate_input_tokens(&anthropic);
    let url = format!(
        "http://127.0.0.1:{}/api/v1/messages?usage_doc_id={}&rayline_request_id={}",
        state.opts.local_adapter_port,
        query_escape(&decision.route_id),
        query_escape(request_id)
    );
    let resp = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .json(&anthropic)
        .send()
        .await?;
    anthropic_response_to_responses(
        resp,
        decision,
        state.opts.metrics.clone(),
        request_id.to_owned(),
        estimated_input_tokens,
        output_kind,
        responses_wants_stream(body),
    )
    .await
}

async fn forward_responses_to_openai_chat_endpoint(
    state: &AppState,
    endpoint: &EndpointConfig,
    decision: &RouteDecision,
    body: &Value,
    request_id: &str,
    output_kind: SyntheticOutputKind,
) -> Result<Response<BoxBody>> {
    let anthropic = responses_to_anthropic_request(body, &decision.selected_model);
    let estimated_input_tokens = approximate_input_tokens(&anthropic);
    let request_body = build_openai_chat_request(&anthropic, &decision.selected_model, false);
    let url = format!(
        "{}/chat/completions",
        endpoint.base_url.trim_end_matches('/')
    );
    let mut outbound = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .json(&request_body);
    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Bearer)?;
    let resp = outbound.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return response_from_reqwest(
            resp,
            status,
            Some(decision),
            state.opts.metrics.clone(),
            Some(request_id.to_owned()),
            Some(estimated_input_tokens),
        )
        .await;
    }
    let value = resp.json::<Value>().await?;
    let anthropic = openai_chat_response_to_anthropic(&value, &decision.selected_model);
    record_remote_completion(
        state.opts.metrics.as_ref(),
        request_id,
        StatusCode::OK.as_u16(),
        usage_u64(&anthropic, "input_tokens").or(Some(estimated_input_tokens)),
        usage_u64(&anthropic, "output_tokens"),
        Some(decision.selected_model.clone()),
    );
    if responses_wants_stream(body) {
        Ok(synthetic_responses_sse(
            decision,
            &anthropic,
            request_id,
            estimated_input_tokens,
            output_kind,
        ))
    } else {
        Ok(synthetic_responses_json(
            decision,
            &anthropic,
            request_id,
            estimated_input_tokens,
            output_kind,
        ))
    }
}

async fn forward_anthropic_endpoint(
    state: &AppState,
    endpoint: &EndpointConfig,
    decision: &RouteDecision,
    inbound_headers: &HeaderMap,
    body: Bytes,
    request_id: &str,
    estimated_input_tokens: u64,
) -> Result<Response<BoxBody>> {
    let mut parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    rewrite_body_model(&mut parsed, &decision.selected_model);
    let outbound_body = serde_json::to_vec(&parsed).unwrap_or_else(|_| body.to_vec());
    let url = format!("{}/v1/messages", endpoint.base_url.trim_end_matches('/'));
    let mut outbound = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .header(
            "anthropic-version",
            inbound_headers
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("2023-06-01"),
        )
        .body(outbound_body);
    if let Some(beta) = inbound_headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
    {
        outbound = outbound.header("anthropic-beta", beta);
    }
    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Anthropic)?;
    let resp = outbound.send().await?;
    let status = resp.status();
    response_from_reqwest(
        resp,
        status,
        Some(decision),
        state.opts.metrics.clone(),
        Some(request_id.to_owned()),
        Some(estimated_input_tokens),
    )
    .await
}

async fn forward_openai_chat_endpoint(
    state: &AppState,
    endpoint: &EndpointConfig,
    decision: &RouteDecision,
    body: Value,
    request_id: &str,
) -> Result<Response<BoxBody>> {
    let want_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let estimated_input_tokens = approximate_input_tokens(&body);
    let request_body = build_openai_chat_request(&body, &decision.selected_model, want_stream);
    let url = format!(
        "{}/chat/completions",
        endpoint.base_url.trim_end_matches('/')
    );
    let mut outbound = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .json(&request_body);
    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Bearer)?;
    let resp = outbound.send().await?;
    let status = resp.status();
    if !status.is_success() {
        return response_from_reqwest(
            resp,
            status,
            Some(decision),
            state.opts.metrics.clone(),
            Some(request_id.to_owned()),
            Some(estimated_input_tokens),
        )
        .await;
    }
    if want_stream {
        // True streaming: translate OpenAI Chat SSE -> Anthropic SSE chunk by chunk.
        return Ok(openai_chat_stream_to_anthropic(
            resp,
            decision,
            state.opts.metrics.clone(),
            request_id.to_owned(),
            estimated_input_tokens,
        ));
    }
    let value = resp.json::<Value>().await?;
    let anthropic = openai_chat_response_to_anthropic(&value, &decision.selected_model);
    record_remote_completion(
        state.opts.metrics.as_ref(),
        request_id,
        StatusCode::OK.as_u16(),
        usage_u64(&anthropic, "input_tokens").or(Some(estimated_input_tokens)),
        usage_u64(&anthropic, "output_tokens"),
        Some(decision.selected_model.clone()),
    );
    if want_stream {
        Ok(synthetic_anthropic_sse(decision, &anthropic))
    } else {
        let mut response = json_response(StatusCode::OK, anthropic);
        add_decision_headers(response.headers_mut(), decision);
        Ok(response)
    }
}

#[derive(Clone, Copy)]
enum AuthStyle {
    Anthropic,
    Bearer,
    ClientBearer,
}

/// Resolve the auth scheme actually used for an endpoint: an explicit `auth`
/// override wins, otherwise the protocol's historical default is kept.
fn resolve_auth_style(endpoint: &EndpointConfig, protocol_default: AuthStyle) -> AuthStyle {
    match endpoint.auth {
        Some(AuthMode::Bearer) => AuthStyle::Bearer,
        Some(AuthMode::ApiKey) => AuthStyle::Anthropic,
        Some(AuthMode::ClientBearer) => AuthStyle::ClientBearer,
        None => protocol_default,
    }
}

fn apply_endpoint_headers(
    mut request: reqwest::RequestBuilder,
    endpoint: &EndpointConfig,
    protocol_default: AuthStyle,
) -> Result<reqwest::RequestBuilder> {
    for (name, value) in &endpoint.headers {
        request = request.header(name, value);
    }
    if let Some(env_name) = endpoint.api_key_env.as_deref() {
        let key = std::env::var(env_name).with_context(|| {
            format!(
                "endpoint {} requires ${env_name}; set it or change router config",
                endpoint.id
            )
        })?;
        request = match resolve_auth_style(endpoint, protocol_default) {
            AuthStyle::Anthropic => request.header("x-api-key", key),
            AuthStyle::Bearer => request.bearer_auth(key),
            AuthStyle::ClientBearer => request,
        };
    }
    Ok(request)
}

async fn response_from_reqwest(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    decision: Option<&RouteDecision>,
    metrics: Option<SharedMetricsSink>,
    request_id: Option<String>,
    estimated_input_tokens: Option<u64>,
) -> Result<Response<BoxBody>> {
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_owned();
    let mut headers_out = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        if is_hop_by_hop_str(k.as_str()) {
            continue;
        }
        let name = match HeaderName::from_bytes(k.as_str().as_bytes()) {
            Ok(name) => name,
            Err(_) => continue,
        };
        let value = match HeaderValue::from_bytes(v.as_bytes()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        headers_out.append(name, value);
    }
    if let Some(decision) = decision {
        add_decision_headers(&mut headers_out, decision);
        if let Ok(value) = HeaderValue::from_str(&decision.requested_model) {
            headers_out.insert("openai-model", value);
        }
        headers_out.remove("x-openai-model");
    }

    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();
    let selected_model = decision.map(|decision| decision.selected_model.clone());
    // Body accumulation is only needed for end-of-stream usage extraction when metrics are active.
    // Mirror the proxy's observe_response gate so large responses are not buffered unnecessarily.
    let has_metrics = metrics.is_some() && request_id.is_some();
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut body = Vec::new();
        let mut sse_buffer = String::new();
        let mut input_tokens = estimated_input_tokens;
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        let mut saw_first_token = false;
        let mut downstream_open = true;
        let mut stream_error = None;
        record_remote_token_usage(
            metrics.as_ref(),
            request_id.as_deref(),
            input_tokens,
            output_tokens,
            selected_model.clone(),
        );
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if !saw_first_token {
                        saw_first_token = true;
                        if let (Some(metrics), Some(request_id)) =
                            (metrics.as_ref(), request_id.as_ref())
                        {
                            metrics.record(MetricsUpdate::FirstToken {
                                request_id: request_id.clone(),
                            });
                        }
                    }
                    let previous_input_tokens = input_tokens;
                    let previous_output_tokens = output_tokens;
                    observe_anthropic_sse_chunk(
                        &bytes,
                        &mut sse_buffer,
                        &mut input_tokens,
                        &mut output_tokens,
                        &mut prompt_cache_tokens,
                    );
                    if input_tokens != previous_input_tokens
                        || output_tokens != previous_output_tokens
                    {
                        record_remote_token_usage(
                            metrics.as_ref(),
                            request_id.as_deref(),
                            input_tokens,
                            output_tokens,
                            selected_model.clone(),
                        );
                    }
                    retain_for_metrics(has_metrics, &mut body, &bytes);
                    if downstream_open && tx.send(Ok(Frame::data(bytes))).await.is_err() {
                        downstream_open = false;
                    }
                }
                Err(error) => {
                    stream_error = Some(error.to_string());
                    if downstream_open {
                        let _ = tx.send(Err(std::io::Error::other(error.to_string()))).await;
                    }
                    break;
                }
            }
        }
        let previous_input_tokens = input_tokens;
        let previous_output_tokens = output_tokens;
        observe_anthropic_sse_chunk(
            b"\n\n",
            &mut sse_buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );
        if input_tokens != previous_input_tokens || output_tokens != previous_output_tokens {
            record_remote_token_usage(
                metrics.as_ref(),
                request_id.as_deref(),
                input_tokens,
                output_tokens,
                selected_model.clone(),
            );
        }
        let previous_input_tokens = input_tokens;
        let previous_output_tokens = output_tokens;
        usage_from_anthropic_body(&body).merge_into(
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );
        if input_tokens != previous_input_tokens || output_tokens != previous_output_tokens {
            record_remote_token_usage(
                metrics.as_ref(),
                request_id.as_deref(),
                input_tokens,
                output_tokens,
                selected_model.clone(),
            );
        }
        if let (Some(metrics), Some(request_id)) = (metrics.as_ref(), request_id.as_ref()) {
            if let Some(error) = stream_error {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: Some(status.as_u16()),
                    error,
                });
            } else if !status.is_success() {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: Some(status.as_u16()),
                    error: format!("upstream returned HTTP {}", status.as_u16()),
                });
            } else {
                if output_tokens.is_none() && estimated_input_tokens.unwrap_or(0) > 1 {
                    info!(
                        "local-router metrics completed without output usage request_id={} status={} input_tokens={} body_bytes={} content_type={} trailing_sse_bytes={}",
                        request_id,
                        status.as_u16(),
                        display_optional_u64(input_tokens),
                        body.len(),
                        content_type,
                        sse_buffer.len()
                    );
                }
                emit_completed_metrics(
                    metrics,
                    request_id,
                    status.as_u16(),
                    input_tokens,
                    output_tokens,
                    prompt_cache_tokens,
                    selected_model,
                );
            }
        }
    });

    let mut builder = Response::builder().status(status.as_u16());
    if let Some(headers) = builder.headers_mut() {
        *headers = headers_out;
    }
    Ok(builder.body(body_out).unwrap())
}

/// Accumulates `chunk` into `buf` only when `has_metrics` is true.
/// Extracted so tests can verify the gate without driving a full async streaming pipeline.
fn retain_for_metrics(has_metrics: bool, buf: &mut Vec<u8>, chunk: &[u8]) {
    if has_metrics {
        buf.extend_from_slice(chunk);
    }
}

fn observe_anthropic_sse_chunk(
    bytes: &[u8],
    buffer: &mut String,
    input_tokens: &mut Option<u64>,
    output_tokens: &mut Option<u64>,
    prompt_cache_tokens: &mut Option<u64>,
) {
    buffer.push_str(&String::from_utf8_lossy(bytes));
    while let Some(event) = drain_sse_event(buffer) {
        for line in event.lines() {
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            usage_from_value(&value).merge_into(input_tokens, output_tokens, prompt_cache_tokens);
        }
    }
}

fn drain_sse_event(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    let (idx, delimiter_len) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return None,
    };
    let event = buffer[..idx].to_owned();
    buffer.drain(..idx + delimiter_len);
    Some(event)
}

fn usage_u64(value: &Value, key: &str) -> Option<u64> {
    value
        .get("usage")
        .and_then(|usage| usage.get(key))
        .and_then(Value::as_u64)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ObservedUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    prompt_cache_tokens: Option<u64>,
}

impl ObservedUsage {
    fn merge_into(
        self,
        input_tokens: &mut Option<u64>,
        output_tokens: &mut Option<u64>,
        prompt_cache_tokens: &mut Option<u64>,
    ) {
        if self.input_tokens.is_some() {
            *input_tokens = self.input_tokens;
        }
        if self.output_tokens.is_some() {
            *output_tokens = self.output_tokens;
        }
        if self.prompt_cache_tokens.is_some() {
            *prompt_cache_tokens = self.prompt_cache_tokens;
        }
    }
}

fn usage_from_anthropic_body(body: &[u8]) -> ObservedUsage {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return usage_from_value(&value);
    }
    let mut usage = ObservedUsage::default();
    for line in String::from_utf8_lossy(body).lines() {
        let Some(payload) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            usage_from_value(&value).merge_into(
                &mut usage.input_tokens,
                &mut usage.output_tokens,
                &mut usage.prompt_cache_tokens,
            );
        }
    }
    usage
}

fn usage_from_value(value: &Value) -> ObservedUsage {
    let mut usage = ObservedUsage::default();
    collect_usage_from_value(value, &mut usage);
    usage
}

fn collect_usage_from_value(value: &Value, usage: &mut ObservedUsage) {
    match value {
        Value::Object(map) => {
            if let Some(input) = total_input_tokens_from_object(map) {
                usage.input_tokens = Some(input);
            }
            // Read cache tokens regardless of whether total input tokens are present.
            if let Some(cache) = cache_read_tokens_from_object(map) {
                usage.prompt_cache_tokens = Some(cache);
            }
            if let Some(output) = map
                .get("output_tokens")
                .or_else(|| map.get("completion_tokens"))
                .and_then(Value::as_u64)
            {
                usage.output_tokens = Some(output);
            }
            for child in map.values() {
                collect_usage_from_value(child, usage);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_usage_from_value(item, usage);
            }
        }
        _ => {}
    }
}

fn cache_read_tokens_from_object(map: &serde_json::Map<String, Value>) -> Option<u64> {
    if let Some(tokens) = map.get("cache_read_input_tokens").and_then(Value::as_u64) {
        return Some(tokens);
    }
    map.get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
}

fn total_input_tokens_from_object(map: &serde_json::Map<String, Value>) -> Option<u64> {
    let anthropic_total = token_field(map, "input_tokens")
        .saturating_add(token_field(map, "cache_creation_input_tokens"))
        .saturating_add(token_field(map, "cache_read_input_tokens"));
    if anthropic_total > 0 {
        return Some(anthropic_total);
    }
    map.get("prompt_tokens").and_then(Value::as_u64)
}

fn token_field(map: &serde_json::Map<String, Value>, key: &str) -> u64 {
    map.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn display_optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn record_remote_completion(
    metrics: Option<&SharedMetricsSink>,
    request_id: &str,
    status_code: u16,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    selected_model: Option<String>,
) {
    let Some(metrics) = metrics else {
        return;
    };
    metrics.record(MetricsUpdate::FirstToken {
        request_id: request_id.to_owned(),
    });
    metrics.record(MetricsUpdate::RequestCompleted {
        request_id: request_id.to_owned(),
        status_code: Some(status_code),
        input_tokens,
        output_tokens,
        selected_model,
    });
}

fn record_request_error(
    metrics: Option<&SharedMetricsSink>,
    request_id: &str,
    status_code: Option<u16>,
    error: impl std::fmt::Display,
) {
    let Some(metrics) = metrics else {
        return;
    };
    metrics.record(MetricsUpdate::RequestErrored {
        request_id: request_id.to_owned(),
        status_code,
        error: error.to_string(),
    });
}

/// Emit `RequestCompleted` and, when cache tokens are present, `PromptCache`.
/// Mirrors the proxy's end-of-stream emission pattern.
fn emit_completed_metrics(
    metrics: &SharedMetricsSink,
    request_id: &str,
    status_code: u16,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    prompt_cache_tokens: Option<u64>,
    selected_model: Option<String>,
) {
    metrics.record(MetricsUpdate::RequestCompleted {
        request_id: request_id.to_owned(),
        status_code: Some(status_code),
        input_tokens,
        output_tokens,
        selected_model,
    });
    // Mirror the proxy's PromptCache emission so cache-token accounting works.
    if input_tokens.is_some() || prompt_cache_tokens.is_some() {
        metrics.record(MetricsUpdate::PromptCache {
            request_id: request_id.to_owned(),
            prompt_tokens: input_tokens,
            cache_tokens: prompt_cache_tokens,
            processed_tokens: None,
            prompt_ms: None,
            prompt_tps: None,
        });
    }
}

fn record_remote_token_usage(
    metrics: Option<&SharedMetricsSink>,
    request_id: Option<&str>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    selected_model: Option<String>,
) {
    let (Some(metrics), Some(request_id)) = (metrics, request_id) else {
        return;
    };
    metrics.record(MetricsUpdate::TokenUsage {
        request_id: request_id.to_owned(),
        input_tokens,
        output_tokens,
        selected_model,
    });
}

fn add_decision_headers(headers: &mut HeaderMap, decision: &RouteDecision) {
    let selected = HeaderValue::from_str(&decision.selected_model)
        .unwrap_or_else(|_| HeaderValue::from_static("unknown"));
    let virtual_model = HeaderValue::from_str(&decision.requested_model)
        .unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_VIRTUAL_MODEL));
    let policy = HeaderValue::from_str(&decision.policy)
        .unwrap_or_else(|_| HeaderValue::from_static("local-static"));
    let task = HeaderValue::from_str(&decision.task_class)
        .unwrap_or_else(|_| HeaderValue::from_static("unknown"));
    let route_id = HeaderValue::from_str(&decision.route_id)
        .unwrap_or_else(|_| HeaderValue::from_static("local"));
    headers.insert("x-rayline-selected-model", selected);
    headers.insert("x-rayline-virtual-model", virtual_model);
    headers.insert("x-rayline-policy", policy);
    headers.insert("x-rayline-task-class", task);
    headers.insert("x-rayline-route-id", route_id);
}

fn responses_to_anthropic_request(body: &Value, model: &str) -> Value {
    let mut messages = Vec::new();
    let mut system_parts = Vec::new();
    let mut converted_tools = Vec::new();
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for item in input {
            append_responses_input_as_anthropic(
                &mut messages,
                &mut system_parts,
                &mut converted_tools,
                item,
            );
        }
    } else if let Some(input) = body.get("input") {
        let text = content_to_text(input);
        if !text.is_empty() {
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": text}]}));
        }
    }
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }

    let mut out = Map::new();
    out.insert("model".to_owned(), Value::String(model.to_owned()));
    out.insert("messages".to_owned(), Value::Array(messages));
    out.insert(
        "stream".to_owned(),
        Value::Bool(responses_wants_stream(body)),
    );
    out.insert(
        "max_tokens".to_owned(),
        body.get("max_output_tokens")
            .cloned()
            .unwrap_or_else(|| json!(4096)),
    );
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        system_parts.push(instructions.to_owned());
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools.iter().filter_map(responses_tool_to_anthropic) {
            converted_tools.push(tool);
        }
    }
    if responses_has_compaction_trigger(body) {
        system_parts.push(
            "This is a context compaction request. Return only a concise, durable summary of the conversation state, decisions, pending work, commands, files, and constraints needed to continue later."
                .to_owned(),
        );
    }
    if !system_parts.is_empty() {
        out.insert(
            "system".to_owned(),
            Value::String(system_parts.join("\n\n")),
        );
    }
    if !converted_tools.is_empty() {
        out.insert("tools".to_owned(), Value::Array(converted_tools));
    }
    Value::Object(out)
}

fn responses_wants_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

fn responses_has_compaction_trigger(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
}

fn responses_output_kind(body: &Value) -> SyntheticOutputKind {
    if responses_has_compaction_trigger(body) {
        SyntheticOutputKind::Compaction
    } else {
        SyntheticOutputKind::Message
    }
}

fn append_responses_input_as_anthropic(
    messages: &mut Vec<Value>,
    system_parts: &mut Vec<String>,
    converted_tools: &mut Vec<Value>,
    item: &Value,
) {
    match item.get("type").and_then(Value::as_str) {
        Some("additional_tools") => {
            if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                for tool in tools.iter().filter_map(responses_tool_to_anthropic) {
                    converted_tools.push(tool);
                }
            }
        }
        Some("message") => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            if matches!(role, "system" | "developer") {
                let text = item
                    .get("content")
                    .map(content_to_text)
                    .unwrap_or_else(|| content_to_text(item));
                if !text.trim().is_empty() {
                    system_parts.push(text);
                }
                return;
            }
            let role = if role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            let content = item
                .get("content")
                .and_then(Value::as_array)
                .map(|items| responses_content_to_anthropic(items))
                .unwrap_or_else(|| vec![json!({"type": "text", "text": content_to_text(item)})]);
            messages.push(json!({"role": role, "content": content}));
        }
        Some("agent_message") => {
            let author = item
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("agent");
            let recipient = item
                .get("recipient")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let text = item
                .get("content")
                .map(content_to_text)
                .unwrap_or_else(|| content_to_text(item));
            if !text.trim().is_empty() {
                messages.push(json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": format!("[{author} -> {recipient}]\n{text}")}]
                }));
            }
        }
        Some("function_call") | Some("custom_tool_call") => {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_rayline");
            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
            let arguments = item
                .get("arguments")
                .or_else(|| item.get("input"))
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .unwrap_or_else(|| json!({}));
            messages.push(json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": id, "name": name, "input": arguments}]
            }));
        }
        Some("local_shell_call") => {
            let text = content_to_text(item);
            if !text.trim().is_empty() {
                messages.push(
                    json!({"role": "assistant", "content": [{"type": "text", "text": text}]}),
                );
            }
        }
        Some("function_call_output")
        | Some("custom_tool_call_output")
        | Some("mcp_tool_call_output") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_rayline");
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": function_output_to_text(item.get("output").unwrap_or(&Value::Null)),
                }]
            }));
        }
        Some("tool_search_output") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_rayline");
            let content = format!(
                "status: {}\nexecution: {}\ntools: {}",
                item.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed"),
                item.get("execution").and_then(Value::as_str).unwrap_or(""),
                item.get("tools").cloned().unwrap_or_else(|| json!([]))
            );
            messages.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]
            }));
        }
        Some("compaction") | Some("compaction_summary") | Some("context_compaction") => {
            let text = item
                .get("encrypted_content")
                .or_else(|| item.get("content"))
                .map(content_to_text)
                .unwrap_or_else(|| content_to_text(item));
            if !text.trim().is_empty() {
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": format!("Context compaction summary:\n{text}")}]
                }));
            }
        }
        Some("reasoning") | Some("compaction_trigger") => {}
        _ => {
            let text = content_to_text(item);
            if !text.is_empty() {
                messages.push(json!({"role": "user", "content": [{"type": "text", "text": text}]}));
            }
        }
    }
}

fn responses_content_to_anthropic(items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(json!({"type": "text", "text": text}));
                }
            }
            Some("input_image") => {
                if let Some(url) = item.get("image_url").and_then(Value::as_str) {
                    out.push(openai_image_url_to_anthropic(url));
                }
            }
            Some("input_file") => {
                let label = item
                    .get("filename")
                    .or_else(|| item.get("file_id"))
                    .or_else(|| item.get("file_data"))
                    .and_then(Value::as_str)
                    .unwrap_or("attached file");
                out.push(json!({"type": "text", "text": format!("[file: {label}]")}));
            }
            Some("computer_screenshot") => {
                let text = item
                    .get("image_url")
                    .and_then(Value::as_str)
                    .map(|url| format!("[computer screenshot: {url}]"))
                    .unwrap_or_else(|| "[computer screenshot]".to_owned());
                out.push(json!({"type": "text", "text": text}));
            }
            _ => {
                let text = content_to_text(item);
                if !text.is_empty() {
                    out.push(json!({"type": "text", "text": text}));
                }
            }
        }
    }
    if out.is_empty() {
        out.push(json!({"type": "text", "text": ""}));
    }
    out
}

fn openai_image_url_to_anthropic(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((media_type, data)) = rest.split_once(";base64,")
    {
        return json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data}
        });
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

fn responses_tool_to_anthropic(tool: &Value) -> Option<Value> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    let name = tool
        .get("name")
        .or_else(|| tool.pointer("/function/name"))?
        .as_str()?;
    let description = tool
        .get("description")
        .or_else(|| tool.pointer("/function/description"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let input_schema = tool
        .get("parameters")
        .or_else(|| tool.pointer("/function/parameters"))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Some(json!({
        "name": name,
        "description": description,
        "input_schema": input_schema
    }))
}

fn function_output_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    if let Some(items) = value.get("content").and_then(Value::as_array) {
        return items
            .iter()
            .map(content_to_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(items) = value.get("content_items").and_then(Value::as_array) {
        return items
            .iter()
            .map(content_to_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    content_to_text(value)
}

async fn anthropic_response_to_responses(
    resp: reqwest::Response,
    decision: &RouteDecision,
    metrics: Option<SharedMetricsSink>,
    request_id: String,
    estimated_input_tokens: u64,
    output_kind: SyntheticOutputKind,
    want_stream: bool,
) -> Result<Response<BoxBody>> {
    let status = resp.status();
    if !status.is_success() {
        return response_from_reqwest(
            resp,
            status,
            Some(decision),
            metrics,
            Some(request_id),
            Some(estimated_input_tokens),
        )
        .await;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        if want_stream {
            return Ok(anthropic_stream_to_responses(
                resp,
                decision,
                metrics,
                request_id,
                estimated_input_tokens,
                output_kind,
            ));
        }
        let text = resp.text().await?;
        let value = anthropic_sse_text_to_message(&text, estimated_input_tokens);
        Ok(synthetic_responses_json(
            decision,
            &value,
            &request_id,
            estimated_input_tokens,
            output_kind,
        ))
    } else {
        let value = resp.json::<Value>().await?;
        if !want_stream {
            Ok(synthetic_responses_json(
                decision,
                &value,
                &request_id,
                estimated_input_tokens,
                output_kind,
            ))
        } else {
            Ok(synthetic_responses_sse(
                decision,
                &value,
                &request_id,
                estimated_input_tokens,
                output_kind,
            ))
        }
    }
}

fn anthropic_stream_to_responses(
    resp: reqwest::Response,
    decision: &RouteDecision,
    metrics: Option<SharedMetricsSink>,
    request_id: String,
    estimated_input_tokens: u64,
    output_kind: SyntheticOutputKind,
) -> Response<BoxBody> {
    let selected_model = decision.selected_model.clone();
    let response_id = format!("resp_{}", decision.route_id.replace('-', "_"));
    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();

    tokio::spawn(async move {
        use futures::StreamExt;
        let mut translator = AnthropicToResponsesTranslator::new(
            response_id,
            selected_model.clone(),
            estimated_input_tokens,
            output_kind,
        );
        let mut stream = resp.bytes_stream();
        let mut downstream_open = true;
        let mut saw_first_token = false;
        let mut stream_error = None;
        let initial = translator.start();
        if tx
            .send(Ok(Frame::data(Bytes::from(initial))))
            .await
            .is_err()
        {
            downstream_open = false;
        }
        record_remote_token_usage(
            metrics.as_ref(),
            Some(&request_id),
            Some(estimated_input_tokens),
            None,
            Some(selected_model.clone()),
        );
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let emitted = translator.push_bytes(&bytes);
                    if !emitted.is_empty() {
                        if !saw_first_token && translator.saw_content {
                            saw_first_token = true;
                            if let Some(metrics) = metrics.as_ref() {
                                metrics.record(MetricsUpdate::FirstToken {
                                    request_id: request_id.clone(),
                                });
                            }
                        }
                        if downstream_open
                            && tx
                                .send(Ok(Frame::data(Bytes::from(emitted))))
                                .await
                                .is_err()
                        {
                            downstream_open = false;
                        }
                        record_remote_token_usage(
                            metrics.as_ref(),
                            Some(&request_id),
                            Some(translator.input_tokens),
                            Some(translator.output_tokens_estimate()),
                            Some(selected_model.clone()),
                        );
                    }
                }
                Err(error) => {
                    stream_error = Some(error.to_string());
                    if downstream_open {
                        let _ = tx.send(Err(std::io::Error::other(error.to_string()))).await;
                    }
                    break;
                }
            }
        }
        let tail = translator.finish();
        if downstream_open && !tail.is_empty() && stream_error.is_none() {
            let _ = tx.send(Ok(Frame::data(Bytes::from(tail)))).await;
        }
        if let Some(metrics) = metrics.as_ref() {
            if let Some(error) = stream_error {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::BAD_GATEWAY.as_u16()),
                    error,
                });
            } else {
                if !saw_first_token {
                    metrics.record(MetricsUpdate::FirstToken {
                        request_id: request_id.clone(),
                    });
                }
                metrics.record(MetricsUpdate::RequestCompleted {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    input_tokens: Some(translator.input_tokens),
                    output_tokens: Some(translator.output_tokens_estimate()),
                    selected_model: Some(selected_model),
                });
            }
        }
    });

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("openai-model", &decision.requested_model)
        .body(body_out)
        .unwrap();
    add_decision_headers(response.headers_mut(), decision);
    response
}

#[derive(Default)]
struct AnthropicToolBlock {
    id: String,
    name: String,
    input_json: String,
}

struct AnthropicToResponsesTranslator {
    response_id: String,
    selected_model: String,
    output_kind: SyntheticOutputKind,
    buffer: String,
    text: String,
    tools: HashMap<usize, AnthropicToolBlock>,
    input_tokens: u64,
    output_tokens: Option<u64>,
    running_output_chars: usize,
    completed: bool,
    saw_content: bool,
    text_item_started: bool,
}

impl AnthropicToResponsesTranslator {
    fn new(
        response_id: String,
        selected_model: String,
        estimated_input_tokens: u64,
        output_kind: SyntheticOutputKind,
    ) -> Self {
        Self {
            response_id,
            selected_model,
            output_kind,
            buffer: String::new(),
            text: String::new(),
            tools: HashMap::new(),
            input_tokens: estimated_input_tokens,
            output_tokens: None,
            running_output_chars: 0,
            completed: false,
            saw_content: false,
            text_item_started: false,
        }
    }

    fn start(&self) -> String {
        let mut out = String::new();
        push_sse(
            &mut out,
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": self.selected_model,
                }
            }),
        );
        out
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> String {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = String::new();
        while let Some(idx) = self.buffer.find("\n\n") {
            let raw = self.buffer[..idx].to_owned();
            self.buffer.replace_range(..idx + 2, "");
            if let Some((event, data)) = parse_sse_event(&raw) {
                self.process_event(&event, &data, &mut out);
            }
        }
        out
    }

    fn process_event(&mut self, event: &str, data: &str, out: &mut String) {
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match event {
            "message_start" => {
                if let Some(input) = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                {
                    self.input_tokens = input;
                }
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
                {
                    let id = value
                        .pointer("/content_block/id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_rayline")
                        .to_owned();
                    let name = value
                        .pointer("/content_block/name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let input_json = value
                        .pointer("/content_block/input")
                        .filter(|input| !input.is_null())
                        .map(Value::to_string)
                        .unwrap_or_default();
                    self.tools.insert(
                        index,
                        AnthropicToolBlock {
                            id,
                            name,
                            input_json,
                        },
                    );
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                match value.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            self.text.push_str(text);
                            self.running_output_chars += text.len();
                            self.saw_content = true;
                            if self.output_kind == SyntheticOutputKind::Message {
                                self.start_text_item(out);
                                push_sse(
                                    out,
                                    "response.output_text.delta",
                                    json!({"type": "response.output_text.delta", "delta": text}),
                                );
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) =
                            value.pointer("/delta/partial_json").and_then(Value::as_str)
                            && let Some(tool) = self.tools.get_mut(&index)
                        {
                            tool.input_json.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(tool) = self.tools.remove(&index) {
                    self.saw_content = true;
                    let arguments = if tool.input_json.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        tool.input_json
                    };
                    let item = json!({
                        "type": "function_call",
                        "name": tool.name,
                        "arguments": arguments,
                        "call_id": tool.id,
                    });
                    push_sse(
                        out,
                        "response.output_item.added",
                        json!({"type": "response.output_item.added", "item": item}),
                    );
                    push_sse(
                        out,
                        "response.output_item.done",
                        json!({"type": "response.output_item.done", "item": item}),
                    );
                }
            }
            "message_delta" => {
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.output_tokens = Some(output);
                }
            }
            "message_stop" => {
                out.push_str(&self.finish());
            }
            _ => {}
        }
    }

    fn finish(&mut self) -> String {
        if self.completed {
            return String::new();
        }
        self.completed = true;
        let mut out = String::new();
        if !self.text.is_empty() {
            let item = match self.output_kind {
                SyntheticOutputKind::Message => json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": self.text}],
                    "phase": "final_answer",
                }),
                SyntheticOutputKind::Compaction => json!({
                    "type": "compaction",
                    "encrypted_content": self.text,
                }),
            };
            push_sse(
                &mut out,
                "response.output_item.done",
                json!({"type": "response.output_item.done", "item": item}),
            );
        }
        let output_tokens = self.output_tokens_estimate();
        push_sse(
            &mut out,
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": self.response_id,
                    "status": "completed",
                    "model": self.selected_model,
                    "usage": {
                        "input_tokens": self.input_tokens,
                        "input_tokens_details": {"cached_tokens": 0},
                        "output_tokens": output_tokens,
                        "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": self.input_tokens + output_tokens,
                    }
                }
            }),
        );
        out
    }

    fn start_text_item(&mut self, out: &mut String) {
        if self.text_item_started {
            return;
        }
        self.text_item_started = true;
        let item = json!({
            "type": "message",
            "role": "assistant",
            "content": [],
            "phase": "final_answer",
        });
        push_sse(
            out,
            "response.output_item.added",
            json!({"type": "response.output_item.added", "item": item}),
        );
    }

    fn output_tokens_estimate(&self) -> u64 {
        self.output_tokens
            .unwrap_or_else(|| (self.running_output_chars as u64 / 4).max(1))
    }
}

fn parse_sse_event(raw: &str) -> Option<(String, String)> {
    let mut event = None;
    let mut data = String::new();
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim());
        }
    }
    Some((event?, data))
}

fn synthetic_responses_sse(
    decision: &RouteDecision,
    anthropic_message: &Value,
    request_id: &str,
    estimated_input_tokens: u64,
    output_kind: SyntheticOutputKind,
) -> Response<BoxBody> {
    let response_id = format!("resp_{}", request_id.replace('-', "_"));
    let mut translator = AnthropicToResponsesTranslator::new(
        response_id,
        decision.selected_model.clone(),
        estimated_input_tokens,
        output_kind,
    );
    let mut events = translator.start();
    if let Some(usage) = anthropic_message.get("usage") {
        if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
            translator.input_tokens = input;
        }
        translator.output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
    }
    for (index, block) in anthropic_message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let tool = AnthropicToolBlock {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_rayline")
                        .to_owned(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned(),
                    input_json: block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string(),
                };
                translator.tools.insert(index, tool);
                translator.process_event(
                    "content_block_stop",
                    &json!({"index": index}).to_string(),
                    &mut events,
                );
            }
            _ => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    translator.process_event(
                        "content_block_delta",
                        &json!({"index": index, "delta": {"type": "text_delta", "text": text}})
                            .to_string(),
                        &mut events,
                    );
                }
            }
        }
    }
    events.push_str(&translator.finish());
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("openai-model", &decision.requested_model)
        .body(full_body(events))
        .unwrap();
    add_decision_headers(response.headers_mut(), decision);
    response
}

fn synthetic_responses_json(
    decision: &RouteDecision,
    anthropic_message: &Value,
    request_id: &str,
    estimated_input_tokens: u64,
    output_kind: SyntheticOutputKind,
) -> Response<BoxBody> {
    let response_id = format!("resp_{}", request_id.replace('-', "_"));
    let output = synthetic_response_items(anthropic_message, output_kind);
    let output_text = output
        .iter()
        .filter_map(|item| {
            item.get("content")
                .map(content_to_text)
                .or_else(|| item.get("encrypted_content").map(content_to_text))
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let input_tokens = anthropic_message
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(estimated_input_tokens);
    let output_tokens = anthropic_message
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| (output_text.len() as u64 / 4).max(1));
    let mut response = json_response(
        StatusCode::OK,
        json!({
            "id": response_id,
            "object": "response",
            "created_at": unix_now_secs(),
            "status": "completed",
            "model": decision.requested_model,
            "output": output,
            "output_text": output_text,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": output_tokens,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": input_tokens + output_tokens,
            },
        }),
    );
    response.headers_mut().insert(
        "openai-model",
        HeaderValue::from_str(&decision.requested_model)
            .unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_VIRTUAL_MODEL)),
    );
    add_decision_headers(response.headers_mut(), decision);
    response
}

fn synthetic_response_items(
    anthropic_message: &Value,
    output_kind: SyntheticOutputKind,
) -> Vec<Value> {
    let mut text = String::new();
    let mut output = Vec::new();
    for block in anthropic_message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") if output_kind == SyntheticOutputKind::Message => {
                output.push(json!({
                    "type": "function_call",
                    "name": block.get("name").and_then(Value::as_str).unwrap_or("tool"),
                    "arguments": block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string(),
                    "call_id": block.get("id").and_then(Value::as_str).unwrap_or("call_rayline"),
                }));
            }
            _ => {
                if let Some(part) = block.get("text").and_then(Value::as_str)
                    && !part.is_empty()
                {
                    text.push_str(part);
                }
            }
        }
    }
    if !text.is_empty() {
        match output_kind {
            SyntheticOutputKind::Message => output.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
                "phase": "final_answer",
            })),
            SyntheticOutputKind::Compaction => output.push(json!({
                "type": "compaction",
                "encrypted_content": text,
            })),
        }
    }
    output
}

fn anthropic_sse_text_to_message(sse_text: &str, estimated_input_tokens: u64) -> Value {
    let mut text = String::new();
    let mut input_tokens = estimated_input_tokens;
    let mut output_tokens = None;
    let mut buffer = sse_text.to_owned();
    while let Some(event) = drain_sse_event(&mut buffer) {
        let Some((_event, data)) = parse_sse_event(&event) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(input) = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                {
                    input_tokens = input;
                }
            }
            Some("content_block_delta") => {
                if value.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                    && let Some(delta) = value.pointer("/delta/text").and_then(Value::as_str)
                {
                    text.push_str(delta);
                }
            }
            Some("message_delta") => {
                output_tokens = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64);
            }
            _ => {}
        }
    }
    let output_tokens = output_tokens.unwrap_or_else(|| (text.len() as u64 / 4).max(1));
    json!({
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    })
}

fn build_openai_chat_request(body: &Value, model: &str, want_stream: bool) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        let content = content_to_text(system);
        if !content.is_empty() {
            messages.push(json!({"role": "system", "content": content}));
        }
    }
    if let Some(items) = body.get("messages").and_then(Value::as_array) {
        for message in items {
            append_openai_messages(&mut messages, message);
        }
    }
    let mut out = Map::new();
    out.insert("model".to_owned(), Value::String(model.to_owned()));
    out.insert("messages".to_owned(), Value::Array(messages));
    out.insert("stream".to_owned(), Value::Bool(want_stream));
    if want_stream {
        // Ask the upstream for a trailing usage chunk so we can report real token counts.
        out.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
    // Anthropic `max_tokens` maps to OpenAI `max_completion_tokens`. Newer OpenAI
    // models (gpt-5.x, o-series) reject the deprecated `max_tokens` outright, and
    // older models (gpt-4o*) accept `max_completion_tokens` too, so always emit
    // the modern field.
    if let Some(value) = body.get("max_tokens") {
        out.insert("max_completion_tokens".to_owned(), value.clone());
    }
    for key in ["temperature", "top_p", "stop"] {
        if let Some(value) = body.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let converted = tools.iter().filter_map(tool_to_openai).collect::<Vec<_>>();
        if !converted.is_empty() {
            out.insert("tools".to_owned(), Value::Array(converted));
        }
    }
    Value::Object(out)
}

fn append_openai_messages(out: &mut Vec<Value>, message: &Value) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let Some(content) = message.get("content") else {
        out.push(json!({"role": role, "content": ""}));
        return;
    };
    if let Some(text) = content.as_str() {
        out.push(json!({"role": role, "content": text}));
        return;
    }
    let Some(blocks) = content.as_array() else {
        out.push(json!({"role": role, "content": content_to_text(content)}));
        return;
    };
    if role == "assistant" {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(value) = block.get("text").and_then(Value::as_str) {
                        text.push_str(value);
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("toolu_local");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let arguments = block
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string();
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    }));
                }
                _ => {}
            }
        }
        let mut msg = Map::new();
        msg.insert("role".to_owned(), Value::String("assistant".to_owned()));
        msg.insert(
            "content".to_owned(),
            if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            },
        );
        if !tool_calls.is_empty() {
            msg.insert("tool_calls".to_owned(), Value::Array(tool_calls));
        }
        out.push(Value::Object(msg));
        return;
    }

    // When the user message carries image blocks we must emit OpenAI multimodal
    // content PARTS instead of a flat string; otherwise we keep the plain-string
    // shape to avoid churning behavior for the common text-only path.
    let has_image = blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("image"));
    if has_image {
        let mut parts = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(value) = block.get("text").and_then(Value::as_str) {
                        if !value.is_empty() {
                            parts.push(json!({"type": "text", "text": value}));
                        }
                    }
                }
                Some("image") => {
                    if let Some(url) = anthropic_image_to_openai_url(block) {
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                }
                Some("tool_result") => {
                    if !parts.is_empty() {
                        out.push(json!({"role": "user", "content": Value::Array(parts)}));
                        parts = Vec::new();
                    }
                    let tool_call_id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("toolu_local");
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": content_to_text(block.get("content").unwrap_or(&Value::Null)),
                    }));
                }
                _ => {}
            }
        }
        out.push(json!({"role": role, "content": Value::Array(parts)}));
        return;
    }

    let mut user_text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    user_text.push_str(value);
                }
            }
            Some("tool_result") => {
                if !user_text.is_empty() {
                    out.push(json!({"role": "user", "content": user_text}));
                    user_text = String::new();
                }
                let tool_call_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu_local");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content_to_text(block.get("content").unwrap_or(&Value::Null)),
                }));
            }
            _ => {}
        }
    }
    out.push(json!({"role": role, "content": user_text}));
}

/// Convert an Anthropic image block into an OpenAI `image_url` value string.
/// `base64` sources become `data:<media_type>;base64,<data>` URLs; `url`
/// sources pass through. Returns `None` if the block is malformed.
fn anthropic_image_to_openai_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn tool_to_openai(tool: &Value) -> Option<Value> {
    let name = tool.get("name")?.as_str()?;
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let parameters = tool
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    }))
}

fn openai_chat_response_to_anthropic(value: &Value, model: &str) -> Value {
    let message = value
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish_reason = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("toolu_local");
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }
    let stop_reason = if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        "tool_use"
    } else if finish_reason == "length" {
        "max_tokens"
    } else {
        "end_turn"
    };
    json!({
        "id": value.get("id").and_then(Value::as_str).unwrap_or("msg_rayline_local"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": value.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": value.pointer("/usage/completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        }
    })
}

/// Streaming translator state: OpenAI Chat `chat.completion.chunk` SSE in,
/// Anthropic Messages SSE out.
///
/// State machine (one Anthropic block index per text run and per OpenAI tool
/// index):
///   - lazily emit `message_start` on the first chunk (real `id` if present,
///     else generated; input_tokens = estimate, corrected later);
///   - on a text fragment: open a text block if none is open
///     (`content_block_start`), emit one `content_block_delta`/`text_delta` per
///     fragment, and `content_block_stop` when the run ends;
///   - on a tool_call delta: close any open text block, on the first delta for
///     an OpenAI index open a `tool_use` block and remember name/id, then emit
///     one `input_json_delta` per `function.arguments` fragment;
///   - terminal chunk (`finish_reason` set) flushes open blocks, then on stream
///     end emit `message_delta` (mapped stop_reason + output_tokens) and
///     `message_stop`.
///
/// One OpenAI streamed tool call, accumulated across delta fragments.
#[derive(Default)]
struct ToolCallAcc {
    index: u64,
    id: String,
    name: String,
    args: String,
}

/// `output_tokens` prefers the trailing usage chunk's `completion_tokens`,
/// otherwise falls back to a rough running estimate.
#[derive(Default)]
struct OpenAiSseTranslator {
    selected_model: String,
    estimated_input_tokens: u64,
    /// Raw, undecoded upstream bytes awaiting a complete `\n`-terminated line.
    /// Kept as bytes (not a String) so a multi-byte UTF-8 codepoint split across
    /// two upstream chunks is never lossily mangled before the line is complete.
    line_buffer: Vec<u8>,
    message_started: bool,
    message_id: Option<String>,
    next_block_index: usize,
    open_text_block: Option<usize>,
    /// Tool calls accumulated by OpenAI `tool_calls[].index`, in first-seen order.
    /// OpenAI may interleave argument fragments across indices, which a strictly
    /// sequential Anthropic block stream cannot express incrementally, so we
    /// buffer here and emit complete, non-overlapping `tool_use` blocks in
    /// `finish`.
    tool_calls: Vec<ToolCallAcc>,
    saw_tool: bool,
    finish_reason: Option<String>,
    /// Real prompt token count from the trailing usage chunk, when present.
    prompt_tokens: Option<u64>,
    output_tokens: Option<u64>,
    running_output_chars: usize,
    saw_content: bool,
    finished: bool,
}

impl OpenAiSseTranslator {
    fn new(selected_model: &str, estimated_input_tokens: u64) -> Self {
        Self {
            selected_model: selected_model.to_owned(),
            estimated_input_tokens,
            ..Default::default()
        }
    }

    /// Feed a raw byte chunk; returns Anthropic SSE text to forward downstream.
    /// Partial trailing lines are buffered until completed by a later chunk.
    fn push_bytes(&mut self, bytes: &[u8]) -> String {
        self.line_buffer.extend_from_slice(bytes);
        let mut out = String::new();
        while let Some(line) = self.next_line() {
            self.handle_line(&line, &mut out);
        }
        out
    }

    /// Pop one complete line (terminated by `\n`), trimming a trailing `\r`.
    /// Decoding is deferred until the line is whole; `\n`/`\r` are ASCII and can
    /// never fall inside a multi-byte codepoint, so a complete line always
    /// decodes cleanly even when the upstream split a codepoint across chunks.
    fn next_line(&mut self) -> Option<String> {
        let idx = self.line_buffer.iter().position(|&b| b == b'\n')?;
        let mut line = self.line_buffer[..idx].to_vec();
        self.line_buffer.drain(..idx + 1);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(String::from_utf8_lossy(&line).into_owned())
    }

    fn handle_line(&mut self, line: &str, out: &mut String) {
        let Some(payload) = line.strip_prefix("data:") else {
            // Blank lines, comments (`:`), and event: lines carry no JSON.
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        self.handle_chunk(&value, out);
    }

    fn handle_chunk(&mut self, chunk: &Value, out: &mut String) {
        if !self.message_started {
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                self.message_id = Some(id.to_owned());
            }
            self.emit_message_start(out);
        }

        // Trailing usage chunk: `choices` empty, populated `usage`. With
        // `stream_options.include_usage` this carries the REAL prompt + completion
        // counts, which we prefer over the pre-request estimate for metrics.
        if let Some(usage) = chunk.get("usage") {
            if let Some(prompt) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.prompt_tokens = Some(prompt);
            }
            if let Some(completion) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = Some(completion);
            }
        }

        let Some(choice) = chunk.pointer("/choices/0") else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.emit_text_fragment(text, out);
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                if !tool_calls.is_empty() {
                    // Text (if any) is complete once tool calls begin.
                    self.close_text_block(out);
                }
                for call in tool_calls {
                    self.accumulate_tool_fragment(call);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }
    }

    fn emit_message_start(&mut self, out: &mut String) {
        self.message_started = true;
        let id = self
            .message_id
            .clone()
            .unwrap_or_else(|| "msg_rayline_local".to_owned());
        push_sse(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.selected_model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": self.estimated_input_tokens, "output_tokens": 0},
                }
            }),
        );
    }

    fn emit_text_fragment(&mut self, text: &str, out: &mut String) {
        self.saw_content = true;
        self.running_output_chars += text.chars().count();
        let index = match self.open_text_block {
            Some(index) => index,
            None => {
                let index = self.next_block_index;
                self.next_block_index += 1;
                self.open_text_block = Some(index);
                push_sse(
                    out,
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                );
                index
            }
        };
        push_sse(
            out,
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
        );
    }

    fn close_text_block(&mut self, out: &mut String) {
        if let Some(index) = self.open_text_block.take() {
            push_sse(
                out,
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            );
        }
    }

    /// Accumulate one streamed `tool_calls[]` fragment. Nothing is emitted here;
    /// complete, non-overlapping tool blocks are written in `finish`, because
    /// OpenAI may interleave argument fragments across tool indices.
    fn accumulate_tool_fragment(&mut self, call: &Value) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        self.saw_tool = true;
        self.saw_content = true;
        let entry = match self.tool_calls.iter().position(|tool| tool.index == index) {
            Some(pos) => &mut self.tool_calls[pos],
            None => {
                self.tool_calls.push(ToolCallAcc {
                    index,
                    ..Default::default()
                });
                self.tool_calls.last_mut().unwrap()
            }
        };
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                entry.id = id.to_owned();
            }
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            if !name.is_empty() {
                entry.name = name.to_owned();
            }
        }
        if let Some(fragment) = call.pointer("/function/arguments").and_then(Value::as_str) {
            entry.args.push_str(fragment);
        }
    }

    /// Close all open blocks and emit `message_delta` + `message_stop`.
    fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.finished {
            return out;
        }
        self.finished = true;
        if !self.message_started {
            self.emit_message_start(&mut out);
        }
        self.close_text_block(&mut out);
        // Emit each buffered tool call as a complete, non-overlapping `tool_use`
        // block, in OpenAI index order. Streaming the fragments incrementally is
        // impossible because OpenAI may interleave them across indices, which
        // Anthropic's one-open-block-at-a-time SSE cannot represent.
        let mut tools = std::mem::take(&mut self.tool_calls);
        tools.sort_by_key(|tool| tool.index);
        for tool in &tools {
            let block_index = self.next_block_index;
            self.next_block_index += 1;
            let id = if tool.id.is_empty() {
                "toolu_local"
            } else {
                tool.id.as_str()
            };
            let name = if tool.name.is_empty() {
                "tool"
            } else {
                tool.name.as_str()
            };
            push_sse(
                &mut out,
                "content_block_start",
                json!({"type":"content_block_start","index":block_index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            );
            if !tool.args.is_empty() {
                push_sse(
                    &mut out,
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":block_index,"delta":{"type":"input_json_delta","partial_json":tool.args}}),
                );
            }
            push_sse(
                &mut out,
                "content_block_stop",
                json!({"type":"content_block_stop","index":block_index}),
            );
        }
        let stop_reason = self.mapped_stop_reason();
        let output_tokens = self
            .output_tokens
            .unwrap_or_else(|| (self.running_output_chars as u64 / 4).max(1));
        push_sse(
            &mut out,
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":Value::Null},"usage":{"output_tokens":output_tokens}}),
        );
        push_sse(&mut out, "message_stop", json!({"type":"message_stop"}));
        out
    }

    fn mapped_stop_reason(&self) -> &'static str {
        if self.saw_tool {
            return "tool_use";
        }
        match self.finish_reason.as_deref() {
            Some("tool_calls") => "tool_use",
            Some("length") => "max_tokens",
            _ => "end_turn",
        }
    }

    fn output_tokens_estimate(&self) -> u64 {
        self.output_tokens
            .unwrap_or_else(|| (self.running_output_chars as u64 / 4).max(1))
    }

    /// Real prompt token count from the streamed usage chunk when present
    /// (`stream_options.include_usage`), else the pre-request estimate.
    fn input_tokens(&self, estimate: u64) -> u64 {
        self.prompt_tokens.unwrap_or(estimate)
    }
}

/// Stream OpenAI Chat SSE upstream, translating to Anthropic SSE as bytes
/// arrive. Mirrors the side-effects of `response_from_reqwest`:
/// `add_decision_headers`, `FirstToken` on the first content/tool delta,
/// incremental + terminal token usage, `RequestCompleted`/`RequestErrored`.
fn openai_chat_stream_to_anthropic(
    resp: reqwest::Response,
    decision: &RouteDecision,
    metrics: Option<SharedMetricsSink>,
    request_id: String,
    estimated_input_tokens: u64,
) -> Response<BoxBody> {
    let selected_model = decision.selected_model.clone();
    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();

    tokio::spawn(async move {
        use futures::StreamExt;
        let mut translator = OpenAiSseTranslator::new(&selected_model, estimated_input_tokens);
        let mut stream = resp.bytes_stream();
        let mut downstream_open = true;
        let mut saw_first_token = false;
        let mut stream_error = None;
        record_remote_token_usage(
            metrics.as_ref(),
            Some(&request_id),
            Some(estimated_input_tokens),
            None,
            Some(selected_model.clone()),
        );
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let emitted = translator.push_bytes(&bytes);
                    if !emitted.is_empty() {
                        if !saw_first_token && translator.saw_content {
                            saw_first_token = true;
                            if let Some(metrics) = metrics.as_ref() {
                                metrics.record(MetricsUpdate::FirstToken {
                                    request_id: request_id.clone(),
                                });
                            }
                        }
                        if downstream_open
                            && tx
                                .send(Ok(Frame::data(Bytes::from(emitted))))
                                .await
                                .is_err()
                        {
                            downstream_open = false;
                        }
                        record_remote_token_usage(
                            metrics.as_ref(),
                            Some(&request_id),
                            Some(translator.input_tokens(estimated_input_tokens)),
                            Some(translator.output_tokens_estimate()),
                            Some(selected_model.clone()),
                        );
                    }
                }
                Err(error) => {
                    stream_error = Some(error.to_string());
                    if downstream_open {
                        let _ = tx.send(Err(std::io::Error::other(error.to_string()))).await;
                    }
                    break;
                }
            }
        }
        let tail = translator.finish();
        if downstream_open && !tail.is_empty() && stream_error.is_none() {
            let _ = tx.send(Ok(Frame::data(Bytes::from(tail)))).await;
        }
        let output_tokens = translator.output_tokens;
        if let Some(metrics) = metrics.as_ref() {
            if let Some(error) = stream_error {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::BAD_GATEWAY.as_u16()),
                    error,
                });
            } else {
                if !saw_first_token {
                    metrics.record(MetricsUpdate::FirstToken {
                        request_id: request_id.clone(),
                    });
                }
                metrics.record(MetricsUpdate::RequestCompleted {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    input_tokens: Some(translator.input_tokens(estimated_input_tokens)),
                    output_tokens: output_tokens.or(Some(translator.output_tokens_estimate())),
                    selected_model: Some(selected_model.clone()),
                });
            }
        }
    });

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body_out)
        .unwrap();
    add_decision_headers(response.headers_mut(), decision);
    response
}

fn synthetic_anthropic_sse(decision: &RouteDecision, message: &Value) -> Response<BoxBody> {
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let usage = message.get("usage").cloned().unwrap_or_else(|| json!({}));
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stop_reason = message
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    let mut events = String::new();
    push_sse(
        &mut events,
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message.get("id").and_then(Value::as_str).unwrap_or("msg_rayline_local"),
                "type": "message",
                "role": "assistant",
                "model": decision.selected_model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": input_tokens, "output_tokens": 0},
            }
        }),
    );
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu_local");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                push_sse(
                    &mut events,
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
                );
                push_sse(
                    &mut events,
                    "content_block_delta",
                    json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
                );
                push_sse(
                    &mut events,
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":index}),
                );
            }
            _ => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                push_sse(
                    &mut events,
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                );
                if !text.is_empty() {
                    push_sse(
                        &mut events,
                        "content_block_delta",
                        json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
                    );
                }
                push_sse(
                    &mut events,
                    "content_block_stop",
                    json!({"type":"content_block_stop","index":index}),
                );
            }
        }
    }
    push_sse(
        &mut events,
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":Value::Null},"usage":{"output_tokens":output_tokens}}),
    );
    push_sse(&mut events, "message_stop", json!({"type":"message_stop"}));
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(full_body(events))
        .unwrap();
    add_decision_headers(response.headers_mut(), decision);
    response
}

fn push_sse(out: &mut String, event: &str, data: Value) {
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    out.push_str("data: ");
    out.push_str(&data.to_string());
    out.push_str("\n\n");
}

fn rewrite_body_model(body: &mut Value, model: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_owned(), Value::String(model.to_owned()));
    }
}

fn approximate_input_tokens(value: &Value) -> u64 {
    let system = value.get("system").map(content_to_text).unwrap_or_default();
    let messages = value
        .get("messages")
        .map(content_to_text)
        .unwrap_or_default();
    ((system.len() + messages.len()) as u64 / 4).max(1)
}

fn approximate_responses_input_tokens(value: &Value) -> u64 {
    let instructions = value
        .get("instructions")
        .map(content_to_text)
        .unwrap_or_default();
    let input = value.get("input").map(content_to_text).unwrap_or_default();
    ((instructions.len() + input.len()) as u64 / 4).max(1)
}

fn content_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(obj) = value.as_object() {
        if let Some(text) = obj.get("text").and_then(Value::as_str) {
            return text.to_owned();
        }
        if let Some(text) = obj.get("output_text").and_then(Value::as_str) {
            return text.to_owned();
        }
        if let Some(text) = obj.get("encrypted_content").and_then(Value::as_str) {
            return text.to_owned();
        }
        if let Some(text) = obj.get("input").and_then(Value::as_str) {
            return text.to_owned();
        }
        if let Some(text) = obj.get("arguments").and_then(Value::as_str) {
            return text.to_owned();
        }
        if let Some(content) = obj.get("content") {
            return content_to_text(content);
        }
        if let Some(output) = obj.get("output") {
            return content_to_text(output);
        }
    }
    if value.is_null() {
        String::new()
    } else {
        value.to_string()
    }
}

fn normalize_model_name(model: &str) -> String {
    let trimmed = if model.ends_with(']') {
        model.rfind('[').map_or(model, |idx| &model[..idx])
    } else {
        model
    };
    match trimmed.strip_prefix("claude-rayline-router-") {
        Some("balanced") => DEFAULT_VIRTUAL_MODEL.to_owned(),
        Some(suffix) => format!("rayline-router-{suffix}"),
        None if trimmed == "rayline-router-balanced" => DEFAULT_VIRTUAL_MODEL.to_owned(),
        None => trimmed.to_owned(),
    }
}

fn route_target_label(target: &RouteSelection) -> String {
    match target {
        RouteSelection::Local => "local".to_owned(),
        RouteSelection::Endpoint(endpoint) => format!("endpoint:{endpoint}"),
    }
}

fn is_hop_by_hop_str(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn percent_decode_minimal(value: &str) -> String {
    value
        .replace("%2F", "/")
        .replace("%2f", "/")
        .replace("%3A", ":")
        .replace("%3a", ":")
}

fn chrono_like_now() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => format!("{}", duration.as_secs()),
        Err(_) => "0".to_owned(),
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayline_metrics::MetricsSink as _;

    fn state(config: RouterConfig) -> AppState {
        AppState {
            opts: Arc::new(LocalRouterOptions {
                local_model_id: "local-model".to_owned(),
                ..LocalRouterOptions::default()
            }),
            config: Arc::new(config),
            http: reqwest::Client::new(),
            http_ipv4: reqwest::Client::new(),
            route_counter: Arc::new(AtomicU64::new(1)),
            started_at: "0".to_owned(),
        }
    }

    #[test]
    fn routes_confirmed_subagent_to_local_by_default() {
        // A confirmed subagent (agent_type resolved by the proxy from meta file)
        // is routed to the local model even when using the main virtual model name.
        // Bare agent_id alone is NOT sufficient — see the stray-agent-id test below.
        let state = state(default_config("local-model"));
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("explore"),
        );
        headers.insert(
            RAYLINE_AGENT_TYPE_HEADER,
            HeaderValue::from_static("Explore"),
        );
        let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});

        let decision = select_route(&state, &headers, &body);

        assert_eq!(decision.target, RouteSelection::Local);
        assert_eq!(decision.selected_model, "local-model");
        assert_eq!(decision.task_class, "subagent");
    }

    #[test]
    fn local_redirect_carries_request_id_in_query() {
        let state = state(default_config("local-model"));
        let decision = RouteDecision {
            target: RouteSelection::Local,
            requested_model: DEFAULT_VIRTUAL_MODEL.to_owned(),
            selected_model: "local-model".to_owned(),
            policy: "subagent".to_owned(),
            task_class: "subagent".to_owned(),
            route_id: "local-1".to_owned(),
        };

        let response = local_redirect_response(&state, &decision, "req_abc123");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("redirect location");

        assert!(location.contains("usage_doc_id=local-1"));
        assert!(location.contains("rayline_request_id=req_abc123"));
        assert_eq!(
            response.headers().get(REQUEST_ID_HEADER).unwrap(),
            "req_abc123"
        );
    }

    #[test]
    fn sparse_config_layers_over_defaults_and_defaults_local_model() {
        let path = std::env::temp_dir().join(format!(
            "rayline-local-router-config-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            r#"{"routes":{"subagents":{"Explore":{"endpoint":"local"}}}}"#,
        )
        .unwrap();
        let opts = LocalRouterOptions {
            local_model_id: "local-model".to_owned(),
            config_path: Some(path.clone()),
            ..LocalRouterOptions::default()
        };

        let config = load_config(&opts).unwrap();

        assert!(
            config
                .endpoints
                .iter()
                .any(|endpoint| endpoint.id == "anthropic")
        );
        let main = config.routes.main.as_ref().unwrap();
        assert_eq!(main.endpoint, "anthropic");
        assert_eq!(main.model, "claude-sonnet-4-6");
        let explore = config.routes.subagents.get("Explore").unwrap();
        assert_eq!(explore.endpoint, "local");
        assert_eq!(explore.model, "local-model");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn example_configs_parse() {
        for (name, raw) in [
            (
                "local-router",
                include_str!("../../../examples/local-router.json"),
            ),
            (
                "openai-compatible",
                include_str!("../../../examples/openai-compatible.json"),
            ),
            (
                "openrouter",
                include_str!("../../../examples/openrouter.json"),
            ),
        ] {
            serde_json::from_str::<RouterConfig>(raw)
                .unwrap_or_else(|error| panic!("{name} example did not parse: {error}"));
        }
    }

    /// End-to-end: each `examples/routing-modes/*.json` config (the `--config`
    /// fixtures) must route the MAIN agent and SUBAGENTS to the endpoints/models it
    /// declares. A `subscription` main is stripped first (the proxy handles it),
    /// mirroring the CLI's `materialize_for_local_router`.
    #[test]
    fn config_mode_examples_route_main_and_subagents() {
        fn load_state(raw: &str) -> AppState {
            let mut cfg: serde_json::Value = serde_json::from_str(raw).unwrap();
            let main_passthrough = cfg["routes"]
                .get("main")
                .and_then(|main| main.get("endpoint"))
                .and_then(serde_json::Value::as_str)
                == Some("subscription");
            if main_passthrough {
                cfg["routes"].as_object_mut().unwrap().remove("main");
            }
            let path = std::env::temp_dir().join(format!(
                "rl-mode-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
            let opts = LocalRouterOptions {
                local_model_id: "local-model".to_owned(),
                config_path: Some(path.clone()),
                ..LocalRouterOptions::default()
            };
            let config = load_config(&opts).unwrap();
            let _ = fs::remove_file(path);
            state(config)
        }
        fn main_route(st: &AppState) -> (RouteSelection, String) {
            let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});
            let decision = select_route(st, &HeaderMap::new(), &body);
            (decision.target, decision.selected_model)
        }
        fn sub_route(st: &AppState, agent_type: &str) -> (RouteSelection, String) {
            let mut headers = HeaderMap::new();
            headers.insert(
                CLAUDE_CODE_AGENT_ID_HEADER,
                HeaderValue::from_static("abc123"),
            );
            headers.insert(
                RAYLINE_AGENT_TYPE_HEADER,
                HeaderValue::from_str(agent_type).unwrap(),
            );
            let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});
            let decision = select_route(st, &headers, &body);
            (decision.target, decision.selected_model)
        }
        fn ep(id: &str) -> RouteSelection {
            RouteSelection::Endpoint(id.to_owned())
        }
        let cloud = || (ep("rayline-cloud"), "rayline-router".to_owned());
        let ollama_def = || (ep("ollama"), "qwen3.5:9b".to_owned());
        let anthropic = || (ep("anthropic"), "claude-sonnet-4-6".to_owned());

        // main routed + subagent routed (the routes the local router executes):
        let st = load_state(include_str!("../../../examples/routing-modes/RRC.json"));
        assert_eq!(main_route(&st), cloud());
        assert_eq!(sub_route(&st, "reviewer"), cloud());

        // RRL: `router: rayline-local` makes the LSR the router; it forwards to the
        // `rayline-cloud` endpoint but pins each class's `model` on-device instead of
        // sending the `rayline-router` virtual model for the RCR to pick. Covers all
        // three routing slots — main, default subagent, and a per-type override —
        // each a distinct model, proving the LSR (not the RCR) is choosing.
        let st = load_state(include_str!("../../../examples/routing-modes/RRL.json"));
        assert_eq!(main_route(&st), (ep("rayline-cloud"), "GLM-5.2".to_owned()));
        assert_eq!(
            sub_route(&st, "reviewer"),
            (ep("rayline-cloud"), "deepseek/deepseek-v4-pro".to_owned())
        );
        assert_eq!(
            sub_route(&st, "Explore"),
            (ep("rayline-cloud"), "deepseek/deepseek-v4-flash".to_owned())
        );

        // RRCL: `router`/`local_models` are may-local advertisement metadata; they do
        // not change the LSR's routing — main + subagents still resolve to cloud.
        let st = load_state(include_str!("../../../examples/routing-modes/RRCL.json"));
        assert_eq!(main_route(&st), cloud());
        assert_eq!(sub_route(&st, "reviewer"), cloud());

        let st = load_state(include_str!("../../../examples/routing-modes/RLC.json"));
        assert_eq!(main_route(&st), cloud());
        assert_eq!(sub_route(&st, "reviewer"), ollama_def());

        let st = load_state(include_str!("../../../examples/routing-modes/LRC.json"));
        assert_eq!(main_route(&st), ollama_def());
        assert_eq!(sub_route(&st, "reviewer"), cloud());

        let st = load_state(include_str!("../../../examples/routing-modes/LL.json"));
        assert_eq!(main_route(&st), ollama_def());
        assert_eq!(sub_route(&st, "reviewer"), ollama_def());

        let st = load_state(include_str!("../../../examples/routing-modes/RAC.json"));
        assert_eq!(main_route(&st), cloud());
        assert_eq!(sub_route(&st, "reviewer"), anthropic());

        let st = load_state(include_str!("../../../examples/routing-modes/LA.json"));
        assert_eq!(main_route(&st), ollama_def());
        assert_eq!(sub_route(&st, "reviewer"), anthropic());

        // subscription main (stripped) → assert subagents only:
        let st = load_state(include_str!("../../../examples/routing-modes/ARC.json"));
        assert_eq!(sub_route(&st, "reviewer"), cloud());

        let st = load_state(include_str!("../../../examples/routing-modes/AL.json"));
        assert_eq!(sub_route(&st, "reviewer"), ollama_def());

        // per-type: Explore/Plan → distinct local models, anything else → cloud catch-all:
        let st = load_state(include_str!(
            "../../../examples/routing-modes/RLC-per-type.json"
        ));
        assert_eq!(main_route(&st), cloud());
        assert_eq!(
            sub_route(&st, "Explore"),
            (ep("ollama"), "qwen2.5-coder:7b".to_owned())
        );
        assert_eq!(sub_route(&st, "Plan"), ollama_def());
        assert_eq!(sub_route(&st, "reviewer"), cloud());

        // router: rayline-local on the rayline class (RAL/RLL/ARL/LRL) — the LSR
        // routes that class to rayline-cloud and pins its model; the other class is
        // anthropic (API key) / ollama / subscription, per the JSON.
        let glm = || (ep("rayline-cloud"), "GLM-5.2".to_owned());
        let ds_pro = || (ep("rayline-cloud"), "deepseek/deepseek-v4-pro".to_owned());
        let st = load_state(include_str!("../../../examples/routing-modes/RAL.json"));
        assert_eq!(main_route(&st), glm());
        assert_eq!(sub_route(&st, "reviewer"), anthropic());

        let st = load_state(include_str!("../../../examples/routing-modes/RLL.json"));
        assert_eq!(main_route(&st), glm());
        assert_eq!(
            sub_route(&st, "reviewer"),
            (ep("ollama"), "qwen2.5-coder:7b".to_owned())
        );

        let st = load_state(include_str!("../../../examples/routing-modes/LRL.json"));
        assert_eq!(main_route(&st), ollama_def());
        assert_eq!(sub_route(&st, "reviewer"), ds_pro());

        // ARL: subscription main (stripped) → assert the rayline-local subagent only.
        let st = load_state(include_str!("../../../examples/routing-modes/ARL.json"));
        assert_eq!(sub_route(&st, "reviewer"), ds_pro());
    }

    #[test]
    fn explicit_missing_config_path_errors() {
        let path = std::env::temp_dir().join(format!(
            "rayline-missing-router-config-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let opts = LocalRouterOptions {
            config_path: Some(path.clone()),
            ..LocalRouterOptions::default()
        };

        let error = load_config(&opts).expect_err("missing explicit config should fail");

        assert!(error.to_string().contains("--router-config-path"));
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn openai_chat_protocol_spelling_is_accepted() {
        let config = serde_json::from_str::<RouterConfig>(
            r#"{"endpoints":[{"id":"local-openai","protocol":"openai_chat","base_url":"http://127.0.0.1:1234/v1"}]}"#,
        )
        .unwrap();

        assert_eq!(
            config.endpoints.first().unwrap().protocol,
            EndpointProtocol::OpenAIChat
        );
    }

    #[test]
    fn openai_responses_protocol_spelling_is_accepted() {
        let config = serde_json::from_str::<RouterConfig>(
            r#"{"endpoints":[{"id":"openai","protocol":"openai_responses","base_url":"http://127.0.0.1:1234/v1"}]}"#,
        )
        .unwrap();

        assert_eq!(
            config.endpoints.first().unwrap().protocol,
            EndpointProtocol::OpenAIResponses
        );
    }

    #[test]
    fn responses_request_converts_to_anthropic_messages_and_tools() {
        let request = json!({
            "model": "rayline-codex",
            "instructions": "You are concise.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Run a command"}
                ]},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "Run shell",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }]
        });

        let converted = responses_to_anthropic_request(&request, "claude-test");

        assert_eq!(converted["model"], "claude-test");
        assert_eq!(converted["system"], "You are concise.");
        assert_eq!(converted["messages"][0]["role"], "user");
        assert_eq!(
            converted["messages"][0]["content"][0]["text"],
            "Run a command"
        );
        assert_eq!(
            converted["messages"][1]["content"][0]["type"],
            "tool_result"
        );
        assert_eq!(
            converted["messages"][1]["content"][0]["tool_use_id"],
            "call_1"
        );
        assert_eq!(converted["tools"][0]["name"], "shell");
        assert_eq!(
            converted["tools"][0]["input_schema"]["properties"]["cmd"]["type"],
            "string"
        );
    }

    #[test]
    fn responses_lite_input_converts_developer_messages_and_additional_tools() {
        let request = json!({
            "model": "rayline-codex",
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": [{
                    "type": "function",
                    "name": "exec_command",
                    "description": "Run a command",
                    "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
                }]},
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "Follow repo rules."}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hello"}
                ]}
            ],
            "stream": false
        });

        let converted = responses_to_anthropic_request(&request, "claude-test");

        assert_eq!(converted["stream"], false);
        assert_eq!(converted["system"], "Follow repo rules.");
        assert_eq!(converted["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(converted["tools"][0]["name"], "exec_command");
    }

    #[test]
    fn anthropic_stream_translator_emits_responses_events() {
        let mut translator = AnthropicToResponsesTranslator::new(
            "resp_test".to_owned(),
            "rayline-codex".to_owned(),
            3,
            SyntheticOutputKind::Message,
        );
        let mut upstream = String::new();
        push_sse(
            &mut upstream,
            "message_start",
            json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
        );
        push_sse(
            &mut upstream,
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
        push_sse(
            &mut upstream,
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
        );
        push_sse(
            &mut upstream,
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"shell","input":{}}}),
        );
        push_sse(
            &mut upstream,
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"date\"}"}}),
        );
        push_sse(
            &mut upstream,
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        );
        push_sse(
            &mut upstream,
            "message_delta",
            json!({"type":"message_delta","usage":{"output_tokens":7}}),
        );
        push_sse(
            &mut upstream,
            "message_stop",
            json!({"type":"message_stop"}),
        );

        let emitted = format!(
            "{}{}",
            translator.start(),
            translator.push_bytes(upstream.as_bytes())
        );

        assert!(emitted.contains("\"type\":\"response.created\""));
        assert!(emitted.contains("\"type\":\"response.output_text.delta\""));
        assert!(emitted.contains("\"type\":\"function_call\""));
        assert!(emitted.contains("\"name\":\"shell\""));
        assert!(emitted.contains("\"type\":\"response.completed\""));
        assert!(emitted.contains("\"input_tokens\":5"));
        assert!(emitted.contains("\"output_tokens\":7"));
    }

    #[test]
    fn anthropic_stream_translator_can_emit_compaction_output() {
        let mut translator = AnthropicToResponsesTranslator::new(
            "resp_compact".to_owned(),
            "rayline-codex".to_owned(),
            3,
            SyntheticOutputKind::Compaction,
        );
        let mut upstream = String::new();
        push_sse(
            &mut upstream,
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"summary"}}),
        );
        push_sse(
            &mut upstream,
            "message_stop",
            json!({"type":"message_stop"}),
        );

        let emitted = format!(
            "{}{}",
            translator.start(),
            translator.push_bytes(upstream.as_bytes())
        );

        assert!(emitted.contains("\"type\":\"compaction\""));
        assert!(emitted.contains("\"encrypted_content\":\"summary\""));
        assert!(!emitted.contains("response.output_text.delta"));
    }

    #[test]
    fn synthetic_responses_json_returns_completed_response() {
        let decision = RouteDecision {
            target: RouteSelection::Local,
            requested_model: "rayline-codex".to_owned(),
            selected_model: "local-model".to_owned(),
            policy: "main".to_owned(),
            task_class: "main".to_owned(),
            route_id: "local-1".to_owned(),
        };
        let response = synthetic_responses_json(
            &decision,
            &json!({
                "content": [{"type": "text", "text": "done"}],
                "usage": {"input_tokens": 2, "output_tokens": 1}
            }),
            "req-test",
            2,
            SyntheticOutputKind::Message,
        );

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn local_availability_uses_rayline_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-rayline-local-available",
            HeaderValue::from_static("false"),
        );
        assert!(!local_available(&headers));

        let mut headers = HeaderMap::new();
        headers.insert("x-rayline-local-hint", HeaderValue::from_static("1"));
        assert!(local_available(&headers));
    }

    #[test]
    fn normalizes_rayline_router_aliases() {
        assert_eq!(
            normalize_model_name("claude-rayline-router-balanced"),
            DEFAULT_VIRTUAL_MODEL
        );
        assert_eq!(
            normalize_model_name("claude-rayline-router-fast"),
            "rayline-router-fast"
        );
        assert_eq!(
            normalize_model_name("rayline-router-balanced"),
            DEFAULT_VIRTUAL_MODEL
        );
    }

    #[test]
    fn named_subagent_overrides_default_subagent_route() {
        // A confirmed subagent (agent_type resolved) whose type matches a named
        // route overrides the default subagent route.
        let mut config = default_config("local-model");
        config.routes.subagents.insert(
            "reviewer".to_owned(),
            RouteTarget {
                endpoint: "openrouter".to_owned(),
                model: "openai/gpt-5.2".to_owned(),
                ..Default::default()
            },
        );
        let state = state(config);
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("abc123"),
        );
        headers.insert(
            RAYLINE_AGENT_TYPE_HEADER,
            HeaderValue::from_static("reviewer"),
        );
        let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});

        let decision = select_route(&state, &headers, &body);

        assert_eq!(
            decision.target,
            RouteSelection::Endpoint("openrouter".to_owned())
        );
        assert_eq!(decision.selected_model, "openai/gpt-5.2");
        assert_eq!(decision.policy, "subagent:reviewer");
    }

    #[test]
    fn named_subagent_uses_resolved_agent_type_header() {
        let mut config = default_config("local-model");
        config.routes.subagents.insert(
            "reviewer".to_owned(),
            RouteTarget {
                endpoint: "openrouter".to_owned(),
                model: "openai/gpt-5.2".to_owned(),
                ..Default::default()
            },
        );
        let state = state(config);
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("a332089fa2c10afe6"),
        );
        headers.insert(
            RAYLINE_AGENT_TYPE_HEADER,
            HeaderValue::from_static("Reviewer"),
        );
        let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});

        let decision = select_route(&state, &headers, &body);

        assert_eq!(
            decision.target,
            RouteSelection::Endpoint("openrouter".to_owned())
        );
        assert_eq!(decision.selected_model, "openai/gpt-5.2");
        assert_eq!(decision.policy, "subagent:reviewer");
    }

    #[test]
    fn named_subagent_falls_back_to_agent_id_when_type_route_missing() {
        let agent_id = "a332089fa2c10afe6";
        let mut config = default_config("local-model");
        config.routes.subagents.insert(
            agent_id.to_owned(),
            RouteTarget {
                endpoint: "openrouter".to_owned(),
                model: "openai/gpt-5.2".to_owned(),
                ..Default::default()
            },
        );
        let state = state(config);
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static(agent_id),
        );
        headers.insert(
            RAYLINE_AGENT_TYPE_HEADER,
            HeaderValue::from_static("Reviewer"),
        );
        let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});

        let decision = select_route(&state, &headers, &body);

        assert_eq!(
            decision.target,
            RouteSelection::Endpoint("openrouter".to_owned())
        );
        assert_eq!(decision.selected_model, "openai/gpt-5.2");
        assert_eq!(decision.policy, format!("subagent:{agent_id}"));
    }

    #[test]
    fn direct_model_routes_to_declaring_endpoint() {
        let state = state(default_config("local-model"));
        let headers = HeaderMap::new();
        let body = json!({"model": "gpt-5.2", "messages": []});

        let decision = select_route(&state, &headers, &body);

        assert_eq!(
            decision.target,
            RouteSelection::Endpoint("openai".to_owned())
        );
        assert_eq!(decision.selected_model, "gpt-5.2");
    }

    #[test]
    fn openai_tool_response_maps_to_anthropic_tool_use() {
        let response = json!({
            "id": "chatcmpl_1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "Read", "arguments": "{\"file_path\":\"a.rs\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });

        let mapped = openai_chat_response_to_anthropic(&response, "gpt-5.2");

        assert_eq!(mapped["stop_reason"], "tool_use");
        assert_eq!(mapped["content"][0]["type"], "tool_use");
        assert_eq!(mapped["content"][0]["name"], "Read");
        assert_eq!(mapped["content"][0]["input"]["file_path"], "a.rs");
    }

    #[test]
    fn observes_anthropic_sse_usage_for_remote_metrics() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        observe_anthropic_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":0}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );

        assert_eq!(input_tokens, Some(42));
        assert_eq!(output_tokens, Some(17));
    }

    #[test]
    fn observes_crlf_anthropic_sse_usage_for_remote_metrics() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        observe_anthropic_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":0}}}\r\n\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\r\n\r\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );

        assert_eq!(input_tokens, Some(42));
        assert_eq!(output_tokens, Some(17));
        assert!(buffer.is_empty());
    }

    #[test]
    fn observes_unterminated_anthropic_sse_usage_on_flush() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        observe_anthropic_sse_chunk(
            b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );
        assert_eq!(output_tokens, None);

        observe_anthropic_sse_chunk(
            b"\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );
        assert_eq!(output_tokens, Some(17));
        assert!(buffer.is_empty());
    }

    #[test]
    fn observes_cached_anthropic_sse_usage_for_remote_metrics() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        observe_anthropic_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );

        assert_eq!(input_tokens, Some(42));
        assert_eq!(output_tokens, Some(17));
        assert_eq!(prompt_cache_tokens, Some(30));
    }

    #[test]
    fn extracts_usage_from_completed_sse_body() {
        let usage = usage_from_anthropic_body(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
        );

        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(17));
    }

    /// Verifies that `retain_for_metrics` (the helper called inside the streaming loop of
    /// `response_from_reqwest`) accumulates bytes iff `has_metrics` is true.
    ///
    /// This test WILL FAIL if the guard is removed (i.e. if `retain_for_metrics` is made
    /// unconditional), because `buf` would then be non-empty even when `has_metrics=false`.
    #[test]
    fn local_router_body_retention_gated_on_has_metrics() {
        // When has_metrics is false, the buffer must stay empty regardless of how many
        // chunks are fed through.
        let mut buf = Vec::new();
        retain_for_metrics(false, &mut buf, b"hello");
        retain_for_metrics(false, &mut buf, b" world");
        assert!(
            buf.is_empty(),
            "body must not be retained when has_metrics=false (guard is missing or broken)"
        );

        // When has_metrics is true, all chunks must be accumulated.
        let mut buf = Vec::new();
        retain_for_metrics(true, &mut buf, b"hello");
        retain_for_metrics(true, &mut buf, b" world");
        assert_eq!(
            buf, b"hello world",
            "body must be retained in full when has_metrics=true"
        );
    }

    /// A stray `agent_id` header on a request that explicitly asks for the
    /// main virtual model must NOT reclassify the request as a subagent.
    /// This guards against the dangerous downgrade: CC sometimes forwards
    /// agent-id headers even on main-model requests.
    #[test]
    fn main_virtual_model_with_stray_agent_id_is_not_downgraded_to_subagent() {
        let state = state(default_config("local-model"));
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("stray-agent-xyz"),
        );
        // Explicitly requesting the main virtual model
        let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});

        let decision = select_route(&state, &headers, &body);

        assert_eq!(
            decision.task_class, "main",
            "main virtual model request must be classified as main, not subagent, \
             even when a stray agent_id header is present"
        );
    }

    /// When `is_subagent` is true but `agent_id` is None (e.g. because
    /// `agent_type` header is present without an `agent_id` header, or the
    /// request uses `DEFAULT_SUBAGENT_MODEL` with no agent_id header),
    /// `select_route_with_warn` must increment the `routing_uncertain` counter
    /// and emit a warning.  This is an inconsistent/impossible state in normal
    /// operation, so surfacing it makes schema changes observable.
    #[test]
    fn subagent_without_agent_id_increments_routing_uncertain_counter() {
        let metrics = rayline_metrics::RouterMetrics::new("test");
        let opts = LocalRouterOptions {
            metrics: Some(metrics.clone()),
            ..Default::default()
        };
        let app_state = AppState {
            opts: Arc::new(opts),
            config: Arc::new(default_config("local-model")),
            http: reqwest::Client::new(),
            http_ipv4: reqwest::Client::new(),
            route_counter: Arc::new(AtomicU64::new(1)),
            started_at: "0".to_owned(),
        };

        // agent_type header is set but NO agent_id header — this triggers
        // is_subagent=true while agent_id=None.
        let mut headers = HeaderMap::new();
        headers.insert(
            RAYLINE_AGENT_TYPE_HEADER,
            HeaderValue::from_static("Explore"),
        );
        let body = json!({"model": DEFAULT_VIRTUAL_MODEL, "messages": []});

        assert_eq!(metrics.snapshot().totals.routing_uncertain, 0);
        select_route_with_warn(&app_state, &headers, &body);
        assert_eq!(
            metrics.snapshot().totals.routing_uncertain,
            1,
            "routing_uncertain must increment when is_subagent=true but agent_id is None"
        );
    }

    /// Regression test: local-router must track cache-read tokens in ObservedUsage.
    /// Before the fix, `prompt_cache_tokens` was missing from ObservedUsage and
    /// cache-read tokens were silently dropped.
    #[test]
    fn local_router_tracks_prompt_cache_tokens() {
        let mut buffer = String::new();
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        // SSE with cache_read_input_tokens=1234 — the bug was that this field was lost.
        observe_anthropic_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":1234,\"output_tokens\":0}}}\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
        );
        assert_eq!(prompt_cache_tokens, Some(1234)); // was lost before the fix
    }

    /// Regression test: `emit_completed_metrics` must emit a `PromptCache` update
    /// to the metrics sink so cache-token accounting is not silently dropped.
    /// This test FAILS if the `PromptCache` emission is removed from
    /// `emit_completed_metrics`.
    #[test]
    fn emit_completed_metrics_records_prompt_cache_in_sink() {
        let metrics = rayline_metrics::RouterMetrics::new("test");
        // Register the request so RequestCompleted can move it to recent.
        metrics.record(rayline_metrics::MetricsUpdate::RequestStarted {
            request_id: "req-cache-1".to_owned(),
            source: "local-router".to_owned(),
            requested_model: Some("claude-opus".to_owned()),
            agent_id: None,
            agent_type: None,
        });

        emit_completed_metrics(
            &(metrics.clone() as SharedMetricsSink),
            "req-cache-1",
            200,
            Some(10),
            Some(5),
            Some(1234), // cache tokens — must reach the sink
            Some("claude-opus-4-5".to_owned()),
        );

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.recent.len(), 1, "request must appear in recent");
        assert_eq!(
            snapshot.recent[0].prompt_cache_tokens,
            Some(1234),
            "PromptCache emission missing: prompt_cache_tokens not recorded in sink"
        );
    }

    /// Collect the `data:` JSON payloads of emitted Anthropic SSE, in order.
    fn sse_events(emitted: &str) -> Vec<Value> {
        emitted
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|payload| serde_json::from_str::<Value>(payload).expect("valid sse json"))
            .collect()
    }

    #[test]
    fn openai_stream_emits_many_text_deltas() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 7);
        let mut emitted = String::new();
        emitted.push_str(&t.push_bytes(
            b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
        ));
        emitted.push_str(&t.push_bytes(b"data: [DONE]\n\n"));
        emitted.push_str(&t.finish());

        let events = sse_events(&emitted);
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(types.first(), Some(&"message_start"));
        // One content_block_start, then one delta PER fragment (two), then stop.
        let text_deltas = events
            .iter()
            .filter(|event| {
                event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
            })
            .count();
        assert_eq!(text_deltas, 2, "expected one text_delta per fragment");
        let concatenated: String = events
            .iter()
            .filter(|event| event["delta"]["type"] == "text_delta")
            .filter_map(|event| event["delta"]["text"].as_str())
            .collect();
        assert_eq!(concatenated, "Hello world");
        // message_start input estimate, message_delta carries real output tokens.
        let start = events
            .iter()
            .find(|e| e["type"] == "message_start")
            .unwrap();
        assert_eq!(start["message"]["usage"]["input_tokens"], 7);
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["output_tokens"], 2);
        assert_eq!(types.last(), Some(&"message_stop"));
    }

    #[test]
    fn openai_stream_reconstructs_single_tool_call_args() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 5);
        let mut emitted = String::new();
        emitted.push_str(&t.push_bytes(
            b"data: {\"id\":\"chatcmpl-t\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"file\\\"\"}}]}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ));
        emitted.push_str(&t.finish());

        let events = sse_events(&emitted);
        let start = events
            .iter()
            .find(|e| e["type"] == "content_block_start")
            .expect("tool block start");
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["name"], "Read");
        assert_eq!(start["content_block"]["id"], "call_1");
        let args: String = events
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .filter_map(|e| e["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(args, "{\"file\":\"a.rs\"}");
        let parsed: Value = serde_json::from_str(&args).expect("reconstructed json args");
        assert_eq!(parsed["file"], "a.rs");
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn openai_stream_handles_event_split_across_chunks() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 3);
        // First chunk ends mid-JSON-object (no trailing newline): nothing parses yet.
        let first = t.push_bytes(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"con");
        assert!(
            sse_events(&first)
                .iter()
                .all(|e| e["type"] != "content_block_delta"),
            "partial line must not emit a text delta yet"
        );
        // Second chunk completes the object and terminates the line.
        let second = t.push_bytes(b"tent\":\"split\"}}]}\n\n");
        let events = sse_events(&second);
        let concatenated: String = events
            .iter()
            .filter(|e| e["delta"]["type"] == "text_delta")
            .filter_map(|e| e["delta"]["text"].as_str())
            .collect();
        assert_eq!(concatenated, "split");
    }

    #[test]
    fn openai_stream_preserves_multibyte_codepoint_split_across_chunks() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 3);
        // "café" — é is 0xC3 0xA9; split the upstream chunk *inside* that codepoint.
        let line =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"caf\xc3\xa9\"}}]}\n\n";
        let split = line.iter().position(|&b| b == 0xc3).unwrap() + 1;
        let mut emitted = String::new();
        emitted.push_str(&t.push_bytes(&line[..split]));
        emitted.push_str(&t.push_bytes(&line[split..]));
        emitted.push_str(&t.finish());

        let text: String = sse_events(&emitted)
            .iter()
            .filter(|e| e["delta"]["type"] == "text_delta")
            .filter_map(|e| e["delta"]["text"].as_str())
            .collect();
        assert_eq!(
            text, "café",
            "codepoint split across chunks must survive intact"
        );
        assert!(!text.contains('\u{FFFD}'), "no replacement character");
    }

    #[test]
    fn openai_stream_sequential_tool_calls_emit_non_overlapping_blocks() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 5);
        let mut emitted = String::new();
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"f\\\":1}\"}}]}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"Grep\",\"arguments\":\"{\\\"q\\\":2}\"}}]}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ));
        emitted.push_str(&t.finish());

        let events = sse_events(&emitted);
        // Block lifecycle must be strictly sequential: 0 fully closes before 1 opens.
        let block_seq: Vec<(&str, i64)> = events
            .iter()
            .filter(|e| {
                matches!(
                    e["type"].as_str(),
                    Some("content_block_start") | Some("content_block_stop")
                )
            })
            .map(|e| {
                (
                    e["type"].as_str().unwrap(),
                    e["index"].as_i64().unwrap_or(-1),
                )
            })
            .collect();
        assert_eq!(
            block_seq,
            vec![
                ("content_block_start", 0),
                ("content_block_stop", 0),
                ("content_block_start", 1),
                ("content_block_stop", 1),
            ],
            "two streamed tool calls must not produce overlapping Anthropic blocks"
        );
        let names: Vec<&str> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .filter_map(|e| e["content_block"]["name"].as_str())
            .collect();
        assert_eq!(names, vec!["Read", "Grep"]);
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    /// OpenAI may continue arguments for tool index 0 *after* index 1 has already
    /// appeared. Buffering must reconstruct each index's args correctly and still
    /// emit strictly non-overlapping blocks (no delta after a block's stop).
    #[test]
    fn openai_stream_interleaved_tool_calls_reconstruct_and_do_not_overlap() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 5);
        let mut emitted = String::new();
        // index 0 opens with a partial argument fragment.
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]}}]}\n\n",
        ));
        // index 1 appears (fully) BEFORE index 0 is finished.
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"Grep\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]}}]}\n\n",
        ));
        // index 0 RESUMES with the rest of its arguments.
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"a.rs\\\"}\"}}]}}]}\n\n",
        ));
        emitted.push_str(&t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ));
        emitted.push_str(&t.finish());

        let events = sse_events(&emitted);
        // Strictly sequential blocks: 0 fully closed before 1 opens — no delta may
        // appear after its block's content_block_stop.
        let block_seq: Vec<(&str, i64)> = events
            .iter()
            .filter(|e| {
                matches!(
                    e["type"].as_str(),
                    Some("content_block_start")
                        | Some("content_block_delta")
                        | Some("content_block_stop")
                )
            })
            .map(|e| {
                (
                    e["type"].as_str().unwrap(),
                    e["index"].as_i64().unwrap_or(-1),
                )
            })
            .collect();
        assert_eq!(
            block_seq,
            vec![
                ("content_block_start", 0),
                ("content_block_delta", 0),
                ("content_block_stop", 0),
                ("content_block_start", 1),
                ("content_block_delta", 1),
                ("content_block_stop", 1),
            ],
            "interleaved tool args must not produce overlapping/out-of-order blocks"
        );
        // Each index's interleaved fragments reconstruct to valid JSON.
        let args: Vec<&str> = events
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .filter_map(|e| e["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(args, vec!["{\"path\":\"a.rs\"}", "{\"q\":\"x\"}"]);
        for raw in &args {
            serde_json::from_str::<Value>(raw).expect("reconstructed tool args are valid JSON");
        }
    }

    /// The trailing usage chunk's `prompt_tokens` must be captured and preferred
    /// over the pre-request estimate for input-token reporting.
    #[test]
    fn openai_stream_captures_prompt_tokens_from_usage_chunk() {
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 999);
        let _ = t.push_bytes(
            b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        );
        // Before the usage chunk, fall back to the estimate.
        assert_eq!(t.input_tokens(999), 999);
        let _ = t.push_bytes(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        let _ = t.push_bytes(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7}}\n\n",
        );
        assert_eq!(t.prompt_tokens, Some(42));
        assert_eq!(
            t.input_tokens(999),
            42,
            "real prompt_tokens must win over estimate"
        );
        assert_eq!(t.output_tokens, Some(7));
    }

    #[test]
    fn image_block_becomes_openai_image_url_part() {
        let body = json!({
            "model": "gpt-4o-mini",
            "stream": false,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what color is this?"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
                ]
            }]
        });

        let request = build_openai_chat_request(&body, "gpt-4o-mini", false);
        let content = &request["messages"][0]["content"];
        assert!(content.is_array(), "image message must use content parts");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what color is this?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn url_image_source_passes_through() {
        let block = json!({"type":"image","source":{"type":"url","url":"https://x/y.png"}});
        assert_eq!(
            anthropic_image_to_openai_url(&block).as_deref(),
            Some("https://x/y.png")
        );
    }

    #[test]
    fn text_only_user_message_keeps_string_content() {
        let body = json!({
            "model": "gpt-4o-mini",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "hi"}]
            }]
        });
        let request = build_openai_chat_request(&body, "gpt-4o-mini", true);
        assert_eq!(request["messages"][0]["content"], "hi");
        // Streaming requests must opt in to the trailing usage chunk.
        assert_eq!(request["stream"], true);
        assert_eq!(request["stream_options"]["include_usage"], true);
    }

    #[test]
    fn max_tokens_maps_to_openai_max_completion_tokens() {
        let body = json!({
            "model": "gpt-5.4-mini",
            "max_tokens": 32000,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let request = build_openai_chat_request(&body, "gpt-5.4-mini", false);
        // Newer OpenAI models (gpt-5.x, o-series) reject the deprecated `max_tokens`,
        // so the router must translate it to `max_completion_tokens`.
        assert_eq!(request["max_completion_tokens"], 32000);
        assert!(
            request.get("max_tokens").is_none(),
            "deprecated max_tokens must not be forwarded"
        );
    }

    #[test]
    fn auth_override_resolves_over_protocol_default() {
        let mut endpoint = EndpointConfig {
            id: "x".to_owned(),
            kind: "provider".to_owned(),
            protocol: EndpointProtocol::AnthropicMessages,
            base_url: "https://example".to_owned(),
            api_key_env: None,
            models: vec![],
            headers: HashMap::new(),
            auth: None,
        };
        // No override: protocol default is kept.
        assert!(matches!(
            resolve_auth_style(&endpoint, AuthStyle::Anthropic),
            AuthStyle::Anthropic
        ));
        endpoint.auth = Some(AuthMode::Bearer);
        assert!(matches!(
            resolve_auth_style(&endpoint, AuthStyle::Anthropic),
            AuthStyle::Bearer
        ));
        endpoint.auth = Some(AuthMode::ApiKey);
        assert!(matches!(
            resolve_auth_style(&endpoint, AuthStyle::Bearer),
            AuthStyle::Anthropic
        ));
        endpoint.auth = Some(AuthMode::ClientBearer);
        assert!(matches!(
            resolve_auth_style(&endpoint, AuthStyle::Bearer),
            AuthStyle::ClientBearer
        ));
    }

    #[test]
    fn client_bearer_endpoint_is_restricted_to_chatgpt_or_loopback() {
        let mut endpoint = EndpointConfig {
            id: "codex-subscription".to_owned(),
            kind: "provider".to_owned(),
            protocol: EndpointProtocol::OpenAIResponses,
            base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
            api_key_env: None,
            models: vec![],
            headers: HashMap::new(),
            auth: Some(AuthMode::ClientBearer),
        };
        validate_endpoint(&endpoint).unwrap();

        endpoint.base_url = "http://127.0.0.1:1234/v1".to_owned();
        validate_endpoint(&endpoint).unwrap();

        endpoint.base_url = "https://example.com/v1".to_owned();
        assert!(validate_endpoint(&endpoint).is_err());

        endpoint.base_url = "https://chatgpt.com/backend-api/codex".to_owned();
        endpoint.api_key_env = Some("OPENAI_API_KEY".to_owned());
        assert!(validate_endpoint(&endpoint).is_err());
    }

    /// Feed bytes through the translator one byte at a time to stress the
    /// cross-chunk line buffering, then flush. Returns the concatenated
    /// Anthropic SSE output.
    fn translate_byte_by_byte(translator: &mut OpenAiSseTranslator, raw: &[u8]) -> String {
        let mut out = String::new();
        for byte in raw {
            out.push_str(&translator.push_bytes(&[*byte]));
        }
        out.push_str(&translator.finish());
        out
    }

    // These fixtures are REAL `gpt-4o-mini` streaming responses captured from the
    // live OpenAI API (see crates/rayline-local-router/tests/fixtures), so the
    // translator is exercised against actual provider wire bytes — including the
    // trailing `usage` chunk and `data: [DONE]` sentinel — fed one byte at a time.
    #[test]
    fn translates_real_openai_text_stream_fixture() {
        let raw = include_bytes!("../tests/fixtures/openai_text_stream.sse");
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 18);
        let emitted = translate_byte_by_byte(&mut t, raw);
        let events = sse_events(&emitted);
        let types: Vec<&str> = events
            .iter()
            .map(|e| e["type"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(types.first(), Some(&"message_start"));
        assert_eq!(types.last(), Some(&"message_stop"));
        let text_deltas = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
            .count();
        assert!(
            text_deltas > 1,
            "real stream must yield many text deltas, got {text_deltas}"
        );
        let text: String = events
            .iter()
            .filter(|e| e["delta"]["type"] == "text_delta")
            .filter_map(|e| e["delta"]["text"].as_str())
            .collect();
        // The model was asked to count 1..8 space separated.
        assert!(
            text.contains('1') && text.contains('8'),
            "unexpected reply text: {text:?}"
        );
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        // Output tokens come from the real trailing usage chunk (completion_tokens=15).
        assert_eq!(delta["usage"]["output_tokens"], 15);
    }

    #[test]
    fn translates_real_openai_tool_stream_fixture() {
        let raw = include_bytes!("../tests/fixtures/openai_tool_stream.sse");
        let mut t = OpenAiSseTranslator::new("gpt-4o-mini", 52);
        let emitted = translate_byte_by_byte(&mut t, raw);
        let events = sse_events(&emitted);
        let tool_start = events
            .iter()
            .find(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use"
            })
            .expect("a tool_use content_block_start");
        assert_eq!(tool_start["content_block"]["name"], "get_weather");
        let args: String = events
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .filter_map(|e| e["delta"]["partial_json"].as_str())
            .collect();
        let parsed: Value =
            serde_json::from_str(&args).unwrap_or_else(|_| panic!("reconstructed args: {args:?}"));
        assert!(
            parsed["city"].as_str().is_some(),
            "expected a city argument, got {parsed:?}"
        );
        // Provider sent finish_reason="stop", but a tool block was emitted, so we
        // must still map to tool_use.
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }
}

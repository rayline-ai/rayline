use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::StreamBody;
use hyper::body::Frame;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Response, StatusCode};
use rayline_metrics::{MetricsUpdate, REQUEST_ID_HEADER, SharedMetricsSink, new_request_id};
use rayline_mtrouter::{C82Router, EpisodeState, HistoryTurn, Route, WorkerManifest};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, mpsc};

use super::{BoxBody, C82_ARTIFACT_COMMIT, C82Options, full_body, header_str, is_hop_by_hop_str};

const EPISODE_HEADER: &str = "x-rayline-episode-id";
const PUBLIC_MODEL_ALIAS: &str = "rayline/router";
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) struct C82Runtime {
    options: C82Options,
    router: C82Router,
    http: reqwest::Client,
    openrouter_key: String,
    episodes: Mutex<HashMap<String, Arc<Mutex<EpisodeState>>>>,
    decision_log: Mutex<()>,
}

impl C82Runtime {
    pub(crate) async fn load(options: C82Options) -> Result<Self> {
        if options.artifact_commit != C82_ARTIFACT_COMMIT {
            return Err(anyhow!(
                "C82 artifact commit is not the Rayline-pinned immutable commit"
            ));
        }
        let openrouter_key = std::env::var("OPENROUTER_API_KEY")
            .context("C82 requires inherited OPENROUTER_API_KEY")?;
        if openrouter_key.is_empty() {
            return Err(anyhow!("OPENROUTER_API_KEY cannot be empty for C82"));
        }
        let router =
            C82Router::load_native(&options.runtime_dir, options.native_encoder.clone()).await?;
        router.verify_head_golden()?;
        let deadline = Instant::now() + READY_TIMEOUT;
        let health = loop {
            match router.health().await {
                Ok(health) => break health,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => {
                    return Err(anyhow!(
                        "C82 encoder failed readiness before timeout: {error:#}"
                    ));
                }
            }
        };
        if health.device == "cpu" && options.native_encoder.device == "auto" {
            tracing::warn!(
                "C82 found no supported GPU backend; using the explicit slow CPU fallback"
            );
        } else {
            tracing::info!(
                backend = %health.backend,
                revision = %health.backend_revision,
                device = %health.device,
                "C82 native encoder ready"
            );
        }
        if let Some(parent) = options.decision_log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(Self {
            options,
            router,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .build()?,
            openrouter_key,
            episodes: Mutex::new(HashMap::new()),
            decision_log: Mutex::new(()),
        })
    }

    pub(crate) async fn handle_messages(
        &self,
        headers: &HeaderMap,
        request: &Value,
        metrics: Option<&SharedMetricsSink>,
    ) -> Result<Response<BoxBody>> {
        let started = Instant::now();
        let episode_id = headers
            .get(EPISODE_HEADER)
            .and_then(header_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("C82 requests require x-rayline-episode-id"))?
            .to_owned();
        let turns = anthropic_messages_to_turns(request)?;
        let request_id = headers
            .get(REQUEST_ID_HEADER)
            .and_then(header_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(new_request_id);
        let episode = episode_state(
            &self.episodes,
            &episode_id,
            self.router.manifest().workers.len(),
        )
        .await;
        // Hold this through upstream response headers: a successful response
        // commits exactly once, while a pre-response failure rolls state back.
        let mut state = episode.lock().await;
        let prior_arm = state.previous_arm;
        let turn_index = state.turn_index;
        let route = self.router.route(&episode_id, &turns, &state).await?;
        let worker = self
            .router
            .manifest()
            .workers
            .get(route.decision.selected_arm)
            .ok_or_else(|| anyhow!("C82 selected invalid arm"))?
            .clone();
        let dispatch = policy_owned_dispatch(&self.router, request, &worker)?;
        let upstream_started = Instant::now();
        let (response, attempts) = self
            .dispatch_with_retries(headers, &dispatch, &worker)
            .await?;
        let upstream_header_latency_ms = upstream_started.elapsed().as_millis() as u64;
        let status = response.status();
        let committed = commit_response_state(
            &mut state,
            status,
            route.decision.selected_arm,
            route.telemetry.serialized_tokens,
            Instant::now(),
        );
        let next_turn_index = state.turn_index;
        drop(state);

        self.write_decision(
            &request_id,
            &episode_id,
            prior_arm,
            turn_index,
            next_turn_index,
            committed,
            &route,
            &worker,
            &dispatch,
            &attempts,
            status.as_u16(),
            upstream_header_latency_ms,
            started.elapsed().as_millis() as u64,
        )
        .await;
        if let Some(metrics) = metrics {
            metrics.record(MetricsUpdate::RouteDecided {
                request_id: request_id.clone(),
                route_id: Some(format!("c82-{turn_index}")),
                target: "remote".to_owned(),
                endpoint_id: Some("openrouter".to_owned()),
                selected_model: Some(worker.id.clone()),
                requested_model: request
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                policy: Some("c82".to_owned()),
                task_class: Some("c82".to_owned()),
                agent_id: None,
                agent_type: None,
            });
        }
        response_to_client(response, &request_id, &worker.id, metrics.cloned()).await
    }

    async fn dispatch_with_retries(
        &self,
        inbound_headers: &HeaderMap,
        dispatch: &Value,
        worker: &WorkerManifest,
    ) -> Result<(reqwest::Response, Vec<Value>)> {
        let total_attempts = worker.openrouter_max_retries.saturating_add(1);
        let mut attempts = Vec::new();
        for attempt in 1..=total_attempts {
            let attempt_started = Instant::now();
            let mut outbound = self
                .http
                .post(&self.options.openrouter_url)
                .bearer_auth(&self.openrouter_key)
                .header("content-type", "application/json")
                .header("http-referer", "https://rayline.ai")
                .header("x-title", "Rayline")
                .json(dispatch);
            if let Some(version) = inbound_headers
                .get("anthropic-version")
                .and_then(header_str)
            {
                outbound = outbound.header("anthropic-version", version);
            } else {
                outbound = outbound.header("anthropic-version", "2023-06-01");
            }
            // Every C82 arm is non-Anthropic. Claude Code enables Anthropic-only
            // deferred/programmatic-tool betas on its native path; forwarding
            // those beta headers makes OpenRouter reject otherwise-portable
            // custom tool schemas. `policy_owned_dispatch` materializes those
            // tools, so no Anthropic beta header is needed here.
            outbound = outbound.timeout(attempt_timeout(worker.attempt_deadline_seconds));
            match outbound.send().await {
                Ok(response) => {
                    let status = response.status();
                    let retryable = is_retryable_status(status);
                    let delay = if retryable && attempt < total_attempts {
                        retry_delay(worker, attempt)
                    } else {
                        Duration::ZERO
                    };
                    attempts.push(json!({
                        "attempt": attempt,
                        "status": status.as_u16(),
                        "transport_error": false,
                        "latency_ms": attempt_started.elapsed().as_millis() as u64,
                        "retry_delay_ms": delay.as_millis() as u64,
                    }));
                    if retryable && attempt < total_attempts {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok((response, attempts));
                }
                Err(error) => {
                    let delay = if attempt < total_attempts {
                        retry_delay(worker, attempt)
                    } else {
                        Duration::ZERO
                    };
                    attempts.push(json!({
                        "attempt": attempt,
                        "status": null,
                        "transport_error": true,
                        "error_class": transport_error_class(&error),
                        "latency_ms": attempt_started.elapsed().as_millis() as u64,
                        "retry_delay_ms": delay.as_millis() as u64,
                    }));
                    if attempt < total_attempts {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    self.write_pre_response_failure(worker, &attempts).await;
                    return Err(anyhow!(
                        "C82 OpenRouter transport failed after {total_attempts} attempts"
                    ));
                }
            }
        }
        Err(anyhow!("C82 dispatch exhausted without a terminal result"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_decision(
        &self,
        request_id: &str,
        episode_id: &str,
        prior_arm: Option<usize>,
        turn_index: u64,
        next_turn_index: u64,
        committed: bool,
        route: &Route,
        worker: &WorkerManifest,
        dispatch: &Value,
        attempts: &[Value],
        status: u16,
        upstream_header_latency_ms: u64,
        total_latency_ms: u64,
    ) {
        let prior_worker = prior_arm.and_then(|index| {
            self.router
                .manifest()
                .workers
                .get(index)
                .map(|worker| worker.id.as_str())
        });
        let record = json!({
            "schema_version": "rayline.c82-decision.v1",
            "artifact_commit": self.options.artifact_commit,
            "request_id": request_id,
            "episode_id_sha256": format!("{:x}", Sha256::digest(episode_id.as_bytes())),
            "turn_index": turn_index,
            "next_turn_index": next_turn_index,
            "state_committed": committed,
            "prior_arm": prior_worker,
            "selected_arm": worker.id,
            "selected_arm_index": route.decision.selected_arm,
            "device": route.telemetry.device,
            "encode_mode": route.telemetry.encode_mode,
            "cache": {
                "cached_prefix_tokens": route.telemetry.cached_prefix_tokens,
                "serialized_tokens": route.telemetry.serialized_tokens,
                "kv_session_retained": route.telemetry.kv_session_retained,
                "evictions": route.telemetry.kv_evictions,
            },
            "scores": {
                "raw": route.decision.raw_scores,
                "adjusted": route.decision.adjusted_scores,
                "switch_cost_usd": route.decision.switch_cost_usd,
                "cache_miss_tokens": route.decision.cache_miss_tokens,
                "stay_margin": self.router.manifest().policy.previous_worker_stay_margin,
                "cold_switch_multiplier": self.router.manifest().policy.cold_switch_margin_per_usd,
                "stayed": route.decision.stayed,
            },
            "dispatch": {
                "provider": worker.openrouter_provider_slug,
                "provider_order": worker.openrouter_provider_order,
                "fallbacks": worker.openrouter_allow_fallbacks,
                "request_shape": dispatch_request_shape(dispatch),
                "attempts": attempts,
                "http_status": status,
            },
            "latency_ms": {
                "encode": route.telemetry.encode_latency_ms,
                "head_us": route.telemetry.head_latency_us,
                "upstream_headers": upstream_header_latency_ms,
                "total_to_headers": total_latency_ms,
            }
        });
        let _ = self.append_log(record).await;
    }

    async fn write_pre_response_failure(&self, worker: &WorkerManifest, attempts: &[Value]) {
        let record = json!({
            "schema_version": "rayline.c82-decision.v1",
            "artifact_commit": self.options.artifact_commit,
            "state_committed": false,
            "selected_arm": worker.id,
            "dispatch": {
                "provider": worker.openrouter_provider_slug,
                "attempts": attempts,
                "terminal": "pre_response_transport_failure",
            }
        });
        let _ = self.append_log(record).await;
    }

    async fn append_log(&self, record: Value) -> io::Result<()> {
        let _guard = self.decision_log.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.options.decision_log_path)
            .await?;
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line).await?;
        file.flush().await
    }
}

async fn episode_state(
    episodes: &Mutex<HashMap<String, Arc<Mutex<EpisodeState>>>>,
    episode_id: &str,
    worker_count: usize,
) -> Arc<Mutex<EpisodeState>> {
    let mut episodes = episodes.lock().await;
    episodes
        .entry(episode_id.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(EpisodeState::new(worker_count))))
        .clone()
}

fn commit_response_state(
    state: &mut EpisodeState,
    status: StatusCode,
    selected_arm: usize,
    input_tokens: usize,
    now: Instant,
) -> bool {
    if !status.is_success() {
        return false;
    }
    state.commit(selected_arm, input_tokens, now);
    true
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn dispatch_request_shape(dispatch: &Value) -> Value {
    let Some(object) = dispatch.as_object() else {
        return json!({"top_level_keys":[]});
    };
    let mut top_level_keys = object.keys().cloned().collect::<Vec<_>>();
    top_level_keys.sort();
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut tool_keys = tools
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|tool| tool.keys().cloned())
        .collect::<Vec<_>>();
    tool_keys.sort();
    tool_keys.dedup();
    json!({
        "top_level_keys": top_level_keys,
        "tool_count": tools.len(),
        "tool_keys": tool_keys,
    })
}

fn policy_owned_dispatch(
    router: &C82Router,
    request: &Value,
    worker: &WorkerManifest,
) -> Result<Value> {
    let arm = router
        .manifest()
        .workers
        .iter()
        .position(|candidate| candidate.id == worker.id)
        .ok_or_else(|| anyhow!("C82 worker is absent from manifest"))?;
    let mut body = router.dispatch_body(arm, request)?;
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("Anthropic request body must be an object"))?;
    let requested = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut completion = requested.max(worker.minimum_completion_tokens);
    if let Some(cap) = worker.max_completion_tokens {
        if cap < worker.minimum_completion_tokens {
            return Err(anyhow!("C82 worker completion limits are incompatible"));
        }
        completion = completion.min(cap);
    }
    if completion > 0 {
        object.insert("max_tokens".to_owned(), Value::from(completion));
    }
    match worker.temperature {
        Some(value) => {
            object.insert("temperature".to_owned(), json!(value));
        }
        None => {
            object.remove("temperature");
        }
    }
    materialize_claude_tools_for_non_anthropic(object);
    remove_anthropic_only_controls(object);
    Ok(body)
}

fn materialize_claude_tools_for_non_anthropic(object: &mut serde_json::Map<String, Value>) {
    let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        if let Some(tool) = tool.as_object_mut() {
            tool.remove("defer_loading");
            tool.remove("allowed_callers");
            tool.remove("eager_input_streaming");
        }
    }
}

fn remove_anthropic_only_controls(object: &mut serde_json::Map<String, Value>) {
    // Claude Code sends these controls to Anthropic's native Messages API.
    // C82's workers are non-Anthropic OpenRouter endpoints and the immutable
    // worker manifest owns reasoning/output policy. Leaving the client copies
    // in the body makes `require_parameters=true` reject an otherwise portable
    // request before it reaches the pinned provider.
    for field in [
        "context_management",
        "diagnostics",
        "output_config",
        "thinking",
    ] {
        object.remove(field);
    }
}

fn retry_delay(worker: &WorkerManifest, failed_attempt: u64) -> Duration {
    retry_delay_for(
        worker.openrouter_retry_base_seconds,
        worker.openrouter_retry_cap_seconds,
        failed_attempt,
    )
}

fn retry_delay_for(base_seconds: f64, cap_seconds: f64, failed_attempt: u64) -> Duration {
    let exponent = i32::try_from(failed_attempt.saturating_sub(1).min(30)).unwrap_or(30);
    Duration::from_secs_f64((base_seconds * 2_f64.powi(exponent)).min(cap_seconds))
}

fn transport_error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else {
        "transport"
    }
}

fn anthropic_messages_to_turns(request: &Value) -> Result<Vec<HistoryTurn>> {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("C82 Anthropic request must contain messages"))?;
    let mut turns = Vec::new();
    let mut tool_names = HashMap::<String, String>::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let content = message.get("content").unwrap_or(&Value::Null);
        let owned;
        let blocks = if let Some(text) = content.as_str() {
            owned = vec![json!({"type":"text","text":text})];
            owned.as_slice()
        } else {
            content.as_array().map(Vec::as_slice).unwrap_or(&[])
        };
        match role {
            "assistant" => {
                let mut parts = Vec::new();
                for block in blocks.iter().filter_map(Value::as_object) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                parts.push(text.to_owned());
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            tool_names.insert(id, name.clone());
                            let arguments = block.get("input").unwrap_or(&Value::Null);
                            let rendered = if arguments.is_object() {
                                compact_json(arguments)
                            } else {
                                string_coerce(arguments)
                            };
                            parts.push(format!("[tool_call {name}] {rendered}"));
                        }
                        _ => {}
                    }
                }
                turns.push(HistoryTurn {
                    role: "assistant".to_owned(),
                    text: parts.join("\n"),
                });
            }
            "user" => {
                let mut texts = Vec::new();
                let mut results = Vec::new();
                for block in blocks.iter().filter_map(Value::as_object) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                texts.push(text.to_owned());
                            }
                        }
                        Some("tool_result") => {
                            let name = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .and_then(|id| tool_names.get(id))
                                .cloned()
                                .unwrap_or_default();
                            let inner = block.get("content").unwrap_or(&Value::Null);
                            let inner_text = if let Some(text) = inner.as_str() {
                                text.to_owned()
                            } else {
                                inner
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter(|item| {
                                        item.get("type").and_then(Value::as_str) == Some("text")
                                    })
                                    .map(|item| {
                                        item.get("text")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            };
                            let marker = if block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                            {
                                " [tool error]"
                            } else {
                                ""
                            };
                            results.push(format!("[tool_result {name}]{marker}\n{inner_text}"));
                        }
                        _ => {}
                    }
                }
                let text = if results.is_empty() {
                    texts.join("\n")
                } else if texts.is_empty() {
                    results.join("\n\n")
                } else {
                    format!("{}\n\n{}", texts.join("\n"), results.join("\n\n"))
                };
                turns.push(HistoryTurn {
                    role: "user".to_owned(),
                    text,
                });
            }
            _ => {}
        }
    }
    Ok(turns)
}

fn string_coerce(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => {
            if *value {
                "True".to_owned()
            } else {
                "False".to_owned()
            }
        }
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

fn compact_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => ascii_json_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(compact_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut values = values.iter().collect::<Vec<_>>();
            values.sort_by(|(left, _), (right, _)| left.cmp(right));
            format!(
                "{{{}}}",
                values
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}: {}",
                        ascii_json_string(key),
                        compact_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn ascii_json_string(value: &str) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"\"".to_owned())
        .chars()
        .flat_map(|character| {
            if character.is_ascii() {
                vec![character]
            } else {
                let code = character as u32;
                if code <= 0xffff {
                    format!("\\u{code:04x}").chars().collect()
                } else {
                    let adjusted = code - 0x1_0000;
                    let high = 0xd800 + (adjusted >> 10);
                    let low = 0xdc00 + (adjusted & 0x3ff);
                    format!("\\u{high:04x}\\u{low:04x}").chars().collect()
                }
            }
        })
        .collect()
}

async fn response_to_client(
    response: reqwest::Response,
    request_id: &str,
    selected_worker: &str,
    metrics: Option<SharedMetricsSink>,
) -> Result<Response<BoxBody>> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        if is_hop_by_hop_str(name.as_str()) || name.as_str().eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.append(name, value);
        }
    }
    headers.insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(request_id).unwrap_or_else(|_| HeaderValue::from_static("c82")),
    );
    headers.insert(
        "x-rayline-selected-model",
        HeaderValue::from_str(selected_worker).unwrap_or_else(|_| HeaderValue::from_static("c82")),
    );
    headers.insert("x-rayline-policy", HeaderValue::from_static("c82"));
    headers.insert(
        "x-rayline-artifact-commit",
        HeaderValue::from_static(C82_ARTIFACT_COMMIT),
    );
    if content_type.contains("text/event-stream") {
        let (tx, rx) = mpsc::channel::<io::Result<Frame<Bytes>>>(16);
        let request_id = request_id.to_owned();
        let selected_worker = selected_worker.to_owned();
        tokio::spawn(async move {
            let mut upstream = response.bytes_stream();
            let mut buffer = Vec::new();
            let mut outcome = SseOutcome::default();
            while let Some(chunk) = upstream.next().await {
                match chunk {
                    Ok(chunk) => {
                        buffer.extend_from_slice(&chunk);
                        while let Some((frame, consumed)) = next_sse_frame(&buffer) {
                            observe_sse_frame(&frame, &mut outcome);
                            let rewritten = rewrite_sse_frame(&frame);
                            buffer.drain(..consumed);
                            if tx
                                .send(Ok(Frame::data(Bytes::from(rewritten))))
                                .await
                                .is_err()
                            {
                                record_response_terminal(
                                    metrics.as_ref(),
                                    &request_id,
                                    &selected_worker,
                                    status,
                                    Some("C82 downstream stream closed".to_owned()),
                                );
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let error = format!(
                            "C82 upstream stream failed: {}",
                            transport_error_class(&error)
                        );
                        let _ = tx.send(Err(io::Error::other(error.clone()))).await;
                        record_response_terminal(
                            metrics.as_ref(),
                            &request_id,
                            &selected_worker,
                            status,
                            Some(error),
                        );
                        return;
                    }
                }
            }
            if !buffer.is_empty() {
                observe_sse_frame(&buffer, &mut outcome);
                let rewritten = rewrite_sse_frame(&buffer);
                if tx
                    .send(Ok(Frame::data(Bytes::from(rewritten))))
                    .await
                    .is_err()
                {
                    record_response_terminal(
                        metrics.as_ref(),
                        &request_id,
                        &selected_worker,
                        status,
                        Some("C82 downstream stream closed".to_owned()),
                    );
                    return;
                }
            }
            record_response_terminal(
                metrics.as_ref(),
                &request_id,
                &selected_worker,
                status,
                sse_terminal_error(&outcome),
            );
        });
        let body = http_body_util::BodyExt::boxed(StreamBody::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ));
        let mut output = Response::new(body);
        *output.status_mut() = status;
        *output.headers_mut() = headers;
        Ok(output)
    } else {
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                record_response_terminal(
                    metrics.as_ref(),
                    request_id,
                    selected_worker,
                    status,
                    Some(format!(
                        "C82 upstream body failed: {}",
                        transport_error_class(&error)
                    )),
                );
                return Err(error.into());
            }
        };
        let rewritten = serde_json::from_slice::<Value>(&body)
            .map(|mut value| {
                rewrite_response_models(&mut value);
                serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec())
            })
            .unwrap_or_else(|_| body.to_vec());
        record_response_terminal(metrics.as_ref(), request_id, selected_worker, status, None);
        let mut output = Response::new(full_body(rewritten));
        *output.status_mut() = status;
        *output.headers_mut() = headers;
        Ok(output)
    }
}

fn next_sse_frame(buffer: &[u8]) -> Option<(Vec<u8>, usize)> {
    for index in 0..buffer.len().saturating_sub(1) {
        if buffer[index..].starts_with(b"\n\n") {
            return Some((buffer[..index + 2].to_vec(), index + 2));
        }
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((buffer[..index + 4].to_vec(), index + 4));
        }
    }
    None
}

fn rewrite_sse_frame(frame: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(frame) else {
        return frame.to_vec();
    };
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let ending = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let core = line.trim_end_matches(['\r', '\n']);
        if let Some(data) = core.strip_prefix("data:") {
            let data = data.trim_start();
            if data != "[DONE]"
                && let Ok(mut value) = serde_json::from_str::<Value>(data)
            {
                rewrite_response_models(&mut value);
                output.push_str("data: ");
                output.push_str(&serde_json::to_string(&value).unwrap_or_else(|_| data.to_owned()));
                output.push_str(ending);
                continue;
            }
        }
        output.push_str(core);
        output.push_str(ending);
    }
    output.into_bytes()
}

#[derive(Default)]
struct SseOutcome {
    saw_terminal: bool,
    saw_error: bool,
}

fn observe_sse_frame(frame: &[u8], outcome: &mut SseOutcome) {
    let Ok(text) = std::str::from_utf8(frame) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().eq_ignore_ascii_case("event: error") {
            outcome.saw_error = true;
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            outcome.saw_terminal = true;
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_stop") => outcome.saw_terminal = true,
            Some("error") => outcome.saw_error = true,
            _ => {}
        }
    }
}

fn sse_terminal_error(outcome: &SseOutcome) -> Option<String> {
    if outcome.saw_error {
        Some("C82 upstream emitted an SSE error event".to_owned())
    } else if !outcome.saw_terminal {
        Some("C82 upstream stream ended without a terminal event".to_owned())
    } else {
        None
    }
}

fn rewrite_response_models(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.contains_key("model") {
        object.insert(
            "model".to_owned(),
            Value::String(PUBLIC_MODEL_ALIAS.to_owned()),
        );
    }
    if object.get("type").and_then(Value::as_str) == Some("message_start")
        && let Some(message) = object.get_mut("message").and_then(Value::as_object_mut)
        && message.contains_key("model")
    {
        message.insert(
            "model".to_owned(),
            Value::String(PUBLIC_MODEL_ALIAS.to_owned()),
        );
    }
}

fn attempt_timeout(configured_seconds: Option<f64>) -> Duration {
    configured_seconds
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .filter(|duration| !duration.is_zero())
        .unwrap_or(DEFAULT_ATTEMPT_TIMEOUT)
}

fn record_response_terminal(
    metrics: Option<&SharedMetricsSink>,
    request_id: &str,
    selected_worker: &str,
    status: StatusCode,
    error: Option<String>,
) {
    let Some(metrics) = metrics else {
        return;
    };
    if let Some(error) =
        error.or_else(|| (!status.is_success()).then(|| format!("upstream returned HTTP {status}")))
    {
        metrics.record(MetricsUpdate::RequestErrored {
            request_id: request_id.to_owned(),
            status_code: Some(status.as_u16()),
            error,
        });
    } else {
        metrics.record(MetricsUpdate::RequestCompleted {
            request_id: request_id.to_owned(),
            status_code: Some(status.as_u16()),
            input_tokens: None,
            output_tokens: None,
            selected_model: Some(selected_worker.to_owned()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_episode_requests_share_one_serial_lock() {
        let episodes = Mutex::new(HashMap::new());
        let first = episode_state(&episodes, "episode-a", 7).await;
        let second = episode_state(&episodes, "episode-a", 7).await;
        let other = episode_state(&episodes, "episode-b", 7).await;
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));

        let guard = first.lock().await;
        let waiter = tokio::spawn(async move {
            let state = second.lock().await;
            state.turn_index
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(guard);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("same-episode waiter should be released")
                .expect("same-episode waiter should not panic"),
            0
        );
    }

    #[test]
    fn response_headers_define_commit_and_rollback_boundary() {
        let now = Instant::now();
        let mut state = EpisodeState::new(7);
        assert!(!commit_response_state(
            &mut state,
            StatusCode::BAD_GATEWAY,
            3,
            128,
            now,
        ));
        assert_eq!(state.turn_index, 0);
        assert_eq!(state.previous_arm, None);

        assert!(commit_response_state(
            &mut state,
            StatusCode::OK,
            3,
            128,
            now,
        ));
        assert_eq!(state.turn_index, 1);
        assert_eq!(state.previous_arm, Some(3));
        assert_eq!(state.warmth[3].as_ref().unwrap().last_input_tokens, 128);
    }

    #[test]
    fn retry_classification_and_backoff_are_bounded() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::OK));
        assert_eq!(retry_delay_for(2.0, 30.0, 1), Duration::from_secs(2));
        assert_eq!(retry_delay_for(2.0, 30.0, 2), Duration::from_secs(4));
        assert_eq!(retry_delay_for(2.0, 30.0, 8), Duration::from_secs(30));
        assert_eq!(attempt_timeout(None), DEFAULT_ATTEMPT_TIMEOUT);
        assert_eq!(attempt_timeout(Some(42.0)), Duration::from_secs(42));
        assert_eq!(attempt_timeout(Some(0.0)), DEFAULT_ATTEMPT_TIMEOUT);
        assert_eq!(attempt_timeout(Some(f64::NAN)), DEFAULT_ATTEMPT_TIMEOUT);
    }

    #[test]
    fn tool_rendering_matches_compact_json_spacing_and_ascii() {
        let request = json!({
            "messages": [
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"1","name":"run","input":{"z":"é","a":1}}
                ]},
                {"role":"user","content":[
                    {"type":"text","text":"context"},
                    {"type":"tool_result","tool_use_id":"1","is_error":true,"content":"failed"}
                ]}
            ]
        });
        let turns = anthropic_messages_to_turns(&request).unwrap();
        assert_eq!(
            turns[0].text,
            "[tool_call run] {\"a\": 1, \"z\": \"\\u00e9\"}"
        );
        assert_eq!(
            turns[1].text,
            "context\n\n[tool_result run] [tool error]\nfailed"
        );
    }

    #[test]
    fn rewrites_complete_sse_event_models() {
        let frame = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"private/model\"}}\n\n";
        let rewritten = String::from_utf8(rewrite_sse_frame(frame)).unwrap();
        assert!(rewritten.contains("\"model\":\"rayline/router\""));
        assert!(!rewritten.contains("private/model"));
    }

    #[test]
    fn sse_outcome_requires_terminal_success_without_error() {
        let mut complete = SseOutcome::default();
        observe_sse_frame(
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            &mut complete,
        );
        assert!(sse_terminal_error(&complete).is_none());

        let mut failed = SseOutcome::default();
        observe_sse_frame(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}\n\n",
            &mut failed,
        );
        assert!(sse_terminal_error(&failed).unwrap().contains("error event"));

        let mut truncated = SseOutcome::default();
        observe_sse_frame(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
            &mut truncated,
        );
        assert!(
            sse_terminal_error(&truncated)
                .unwrap()
                .contains("without a terminal")
        );
    }

    #[test]
    fn response_model_rewrite_preserves_tool_inputs() {
        let mut response = json!({
            "model":"private/model",
            "content":[{
                "type":"tool_use",
                "name":"deploy",
                "input":{"model":"user-selected/model"}
            }]
        });
        rewrite_response_models(&mut response);
        assert_eq!(response["model"], PUBLIC_MODEL_ALIAS);
        assert_eq!(
            response["content"][0]["input"]["model"],
            "user-selected/model"
        );
    }

    #[test]
    fn response_terminal_metrics_close_active_records() {
        use rayline_metrics::RouterMetrics;

        let metrics = RouterMetrics::new("c82-test");
        let sink: SharedMetricsSink = metrics.clone();
        for request_id in ["completed", "errored"] {
            sink.record(MetricsUpdate::RouteDecided {
                request_id: request_id.to_owned(),
                route_id: Some("c82-0".to_owned()),
                target: "remote".to_owned(),
                endpoint_id: Some("openrouter".to_owned()),
                selected_model: Some("worker".to_owned()),
                requested_model: Some(PUBLIC_MODEL_ALIAS.to_owned()),
                policy: Some("c82".to_owned()),
                task_class: Some("c82".to_owned()),
                agent_id: None,
                agent_type: None,
            });
        }
        record_response_terminal(Some(&sink), "completed", "worker", StatusCode::OK, None);
        record_response_terminal(
            Some(&sink),
            "errored",
            "worker",
            StatusCode::OK,
            Some("stream failed".to_owned()),
        );

        let snapshot = metrics.snapshot();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.totals.completed_requests, 2);
        assert_eq!(snapshot.totals.errored_requests, 1);
    }

    #[test]
    fn extracts_only_complete_sse_frames() {
        let buffer = b"event: ping\ndata: {}\n\nevent: partial";
        let (frame, consumed) = next_sse_frame(buffer).unwrap();
        assert_eq!(frame, b"event: ping\ndata: {}\n\n");
        assert_eq!(&buffer[consumed..], b"event: partial");
    }

    #[test]
    fn materializes_claude_tools_for_non_anthropic_arms() {
        let mut object = json!({
            "tools": [
                {
                    "name":"Bash",
                    "defer_loading":true,
                    "allowed_callers":["code_execution"],
                    "eager_input_streaming":true
                },
                {"name":"Read"}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        materialize_claude_tools_for_non_anthropic(&mut object);
        assert_eq!(object["tools"][0], json!({"name":"Bash"}));
        assert_eq!(object["tools"][1], json!({"name":"Read"}));
    }

    #[test]
    fn removes_anthropic_only_controls_owned_by_c82_policy() {
        let mut object = json!({
            "context_management":{"edits":[]},
            "diagnostics":{"enabled":true},
            "output_config":{"effort":"high"},
            "thinking":{"type":"adaptive"},
            "reasoning":{"effort":"high"},
            "metadata":{"user_id":"kept"}
        })
        .as_object()
        .unwrap()
        .clone();
        remove_anthropic_only_controls(&mut object);
        assert_eq!(
            Value::Object(object),
            json!({
                "reasoning":{"effort":"high"},
                "metadata":{"user_id":"kept"}
            })
        );
    }

    #[test]
    fn dispatch_shape_records_only_field_names_and_counts() {
        let shape = dispatch_request_shape(&json!({
            "model":"secret-model",
            "messages":[{"role":"user","content":"private prompt"}],
            "tools":[
                {"name":"Bash","description":"private description","input_schema":{"type":"object"}},
                {"name":"Read","input_schema":{"type":"object"}}
            ]
        }));
        assert_eq!(
            shape,
            json!({
                "top_level_keys":["messages","model","tools"],
                "tool_count":2,
                "tool_keys":["description","input_schema","name"],
            })
        );
        assert!(!shape.to_string().contains("private"));
        assert!(!shape.to_string().contains("secret-model"));
    }
}

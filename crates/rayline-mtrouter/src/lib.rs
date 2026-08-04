//! C82's fail-closed Rust decision plane.
//!
//! The GPU-resident frozen Qwen encoder lives in a pinned native libllama
//! process. This crate verifies the immutable runtime bundle, runs the
//! seven-arm switch-aware head on CPU, and owns cache-aware hysteresis.

mod manifest;
mod model;
mod native;
mod policy;

pub use manifest::{
    CHECKPOINT_SHA256, ENCODER_MODEL, ENCODER_REVISION, Manifest, NativeBinaryManifest,
    NativeEncoderManifest, WORKER_ORDER, WorkerManifest, verify_file_hash,
};
pub use native::NativeEncoderOptions;
pub use policy::{Decision, EpisodeState, WorkerWarmth, argmax_first};

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use model::Estimator;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryTurn {
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncoderHealth {
    pub status: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub backend_revision: String,
    pub device: String,
    pub bf16_supported: bool,
    pub mixed_device_fallback: bool,
    #[serde(default)]
    pub backend_active: bool,
    pub encoder_model: String,
    pub encoder_revision: String,
    pub encoder_dimension: usize,
    pub max_tokens: usize,
    pub pooling: String,
    pub serialization: String,
    pub kv_chunk_tokens: usize,
    #[serde(default)]
    pub flash_attention: Option<bool>,
    #[serde(default)]
    pub physical_batch_tokens: Option<usize>,
    #[serde(default)]
    pub max_sessions: Option<usize>,
    #[serde(default)]
    pub kv_cache_type: Option<String>,
    #[serde(default)]
    pub kv_unified: Option<bool>,
    #[serde(default)]
    pub swa_full: Option<bool>,
    #[serde(default)]
    pub cuda_nccl: Option<bool>,
    #[serde(default)]
    pub selected_device_compute_nodes: Option<usize>,
    #[serde(default)]
    pub host_boundary_nodes: Option<usize>,
    #[serde(default)]
    pub other_device_compute_nodes: Option<usize>,
    pub kv_sessions: usize,
    pub kv_resident_tokens: usize,
    pub kv_evictions: usize,
    pub requests: usize,
    #[serde(default)]
    pub device_free_bytes: Option<u64>,
    #[serde(default)]
    pub device_total_bytes: Option<u64>,
    #[serde(default)]
    pub device_allocated_bytes: Option<u64>,
    #[serde(default)]
    pub memory_budget_bytes: Option<u64>,
    #[serde(default)]
    pub kv_session_budget_tokens: Option<usize>,
    #[serde(default)]
    pub kv_process_budget_tokens: Option<usize>,
}

#[derive(Clone, Debug, Deserialize)]
struct EncodeResponse {
    embedding: Vec<f32>,
    device: String,
    encode_mode: String,
    serialized_tokens: usize,
    full_history_tokens: usize,
    truncated_tokens: usize,
    cached_prefix_tokens: usize,
    kv_session_retained: bool,
    kv_evictions: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoutingTelemetry {
    pub device: String,
    pub encode_mode: String,
    pub serialized_tokens: usize,
    pub full_history_tokens: usize,
    pub truncated_tokens: usize,
    pub cached_prefix_tokens: usize,
    pub kv_session_retained: bool,
    pub kv_evictions: usize,
    pub encode_latency_ms: u64,
    pub head_latency_us: u64,
}

#[derive(Clone, Debug)]
pub struct Route {
    pub decision: Decision,
    pub telemetry: RoutingTelemetry,
}

#[derive(Clone)]
pub struct C82Router {
    runtime_dir: std::path::PathBuf,
    manifest: Manifest,
    estimator: Estimator,
    encoder: native::NativeEncoderClient,
}

impl C82Router {
    pub async fn load_native(
        runtime_dir: impl AsRef<Path>,
        options: NativeEncoderOptions,
    ) -> Result<Self> {
        let runtime_dir = runtime_dir.as_ref();
        let manifest_path = runtime_dir.join("manifest.json");
        let manifest = Manifest::load(&manifest_path)?;
        manifest.verify_core_files(runtime_dir)?;
        let binary = manifest
            .encoder
            .native
            .binaries
            .iter()
            .find(|binary| runtime_dir.join(&binary.file) == options.binary_path)
            .ok_or_else(|| anyhow!("C82 native binary is not declared by the manifest"))?;
        manifest.verify_native_files(runtime_dir, binary)?;
        let expected_model = runtime_dir.join(&manifest.encoder.native.gguf.file);
        if options.model_path != expected_model {
            return Err(anyhow!("C82 native GGUF is not the manifest-pinned file"));
        }
        let estimator = Estimator::load(runtime_dir, &manifest)?;
        let encoder = native::NativeEncoderClient::spawn(options).await?;
        Ok(Self {
            runtime_dir: runtime_dir.to_path_buf(),
            manifest,
            estimator,
            encoder,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn verify_head_golden(&self) -> Result<HeadParityReport> {
        let path = self.runtime_dir.join(&self.manifest.golden.head.file);
        let fixture: HeadGolden = serde_json::from_slice(
            &std::fs::read(&path)
                .with_context(|| format!("read C82 head golden {}", path.display()))?,
        )
        .with_context(|| format!("parse C82 head golden {}", path.display()))?;
        validate_head_golden_contract(&fixture)?;
        let tolerance = self
            .manifest
            .golden
            .head
            .score_tolerance
            .min(fixture.score_tolerance);
        let mut max_score_drift = 0.0_f32;
        let mut selection_matches = 0usize;
        for case in &fixture.cases {
            let previous = usize::try_from(case.previous_model_index).ok();
            let scores =
                self.estimator
                    .q_values(&case.embedding, previous, case.route_call_index)?;
            if scores.len() != case.scores.len() {
                return Err(anyhow!("C82 head golden score count mismatch"));
            }
            for (actual, expected) in scores.iter().zip(&case.scores) {
                max_score_drift = max_score_drift.max((actual - expected).abs());
            }
            if policy::argmax_first(&scores) == Some(case.selected_index) {
                selection_matches += 1;
            }
        }
        let selection_parity = selection_matches as f32 / fixture.cases.len() as f32;
        if max_score_drift > tolerance
            || selection_parity < self.manifest.golden.required_selection_parity
        {
            return Err(anyhow!(
                "C82 head parity failed: max drift {max_score_drift:.8} (limit {tolerance:.8}), selection parity {selection_parity:.6}"
            ));
        }
        Ok(HeadParityReport {
            cases: fixture.cases.len(),
            max_score_drift,
            selection_parity,
            tolerance,
        })
    }

    /// Run every synthetic encoder fixture through the selected runtime,
    /// including the identical incremental and clean-recompute histories.
    pub async fn verify_encoder_golden(&self) -> Result<EncoderParityReport> {
        let golden = self
            .manifest
            .encoder
            .golden
            .as_ref()
            .ok_or_else(|| anyhow!("C82 manifest has no encoder golden"))?;
        let path = self.runtime_dir.join(&golden.file);
        let fixture: EncoderGolden = serde_json::from_slice(
            &std::fs::read(&path)
                .with_context(|| format!("read C82 encoder golden {}", path.display()))?,
        )
        .with_context(|| format!("parse C82 encoder golden {}", path.display()))?;
        if fixture.schema_version != "rayline.c82-encoder-golden.v1"
            || fixture.checkpoint_sha256 != CHECKPOINT_SHA256
            || fixture.encoder.model != self.manifest.encoder.model
            || fixture.encoder.revision != self.manifest.encoder.revision
        {
            return Err(anyhow!("C82 encoder golden contract is incompatible"));
        }
        if fixture.cases.is_empty() {
            return Err(anyhow!("C82 encoder golden has no cases"));
        }
        let mut reports = Vec::with_capacity(fixture.cases.len());
        let mut embeddings = std::collections::HashMap::<String, Vec<f32>>::new();
        let mut max_score_drift = 0.0_f32;
        let mut max_top_two_gap_drift = 0.0_f32;
        let mut selection_matches = 0usize;
        for case in &fixture.cases {
            let episode_id = case.episode_id.as_deref().unwrap_or(case.id.as_str());
            let (encoded, telemetry) = self.encode_history(episode_id, &case.turns).await?;
            if telemetry.serialized_tokens != case.serialized_tokens
                || telemetry.full_history_tokens != case.full_history_tokens
                || telemetry.truncated_tokens != case.truncated_tokens
                || telemetry.encode_mode != case.encode_mode
            {
                return Err(anyhow!(
                    "C82 encoder contract fields differ for {}: expected mode {}/{}/{}/{}, got {}/{}/{}/{}",
                    case.id,
                    case.encode_mode,
                    case.serialized_tokens,
                    case.full_history_tokens,
                    case.truncated_tokens,
                    telemetry.encode_mode,
                    telemetry.serialized_tokens,
                    telemetry.full_history_tokens,
                    telemetry.truncated_tokens,
                ));
            }
            let previous_arm = usize::try_from(case.previous_model_index).ok();
            let scores =
                self.estimator
                    .q_values(&encoded.embedding, previous_arm, case.route_call_index)?;
            if scores.len() != case.scores.len() {
                return Err(anyhow!("C82 encoder golden score count mismatch"));
            }
            let case_score_drift = scores
                .iter()
                .zip(&case.scores)
                .map(|(actual, expected)| (actual - expected).abs())
                .fold(0.0_f32, f32::max);
            let top_two_gap_drift = (top_two_gap(&scores)? - top_two_gap(&case.scores)?).abs();
            let selected_arm = argmax_first(&scores)
                .ok_or_else(|| anyhow!("C82 encoder parity produced no selection"))?;
            let selection_flip = selected_arm != case.selected_index;
            if !selection_flip {
                selection_matches += 1;
            }
            max_score_drift = max_score_drift.max(case_score_drift);
            max_top_two_gap_drift = max_top_two_gap_drift.max(top_two_gap_drift);
            embeddings.insert(case.id.clone(), encoded.embedding);
            reports.push(EncoderCaseParityReport {
                case: case.id.clone(),
                expected_selected_arm: case.selected_index,
                selected_arm,
                max_score_drift: case_score_drift,
                top_two_gap_drift,
                selection_flip,
                telemetry,
            });
        }
        let selection_parity = selection_matches as f32 / fixture.cases.len() as f32;
        let incremental_clean_embedding_max_abs = fixture
            .incremental_parity
            .as_ref()
            .map(|parity| {
                let delta = embeddings
                    .get(&parity.delta_case)
                    .ok_or_else(|| anyhow!("C82 incremental parity delta case is missing"))?;
                let full = embeddings
                    .get(&parity.full_case)
                    .ok_or_else(|| anyhow!("C82 incremental parity full case is missing"))?;
                if delta.len() != full.len() {
                    return Err(anyhow!("C82 incremental parity dimensions differ"));
                }
                Ok(delta
                    .iter()
                    .zip(full)
                    .map(|(left, right)| (left - right).abs())
                    .fold(0.0_f32, f32::max))
            })
            .transpose()?;
        if selection_parity < self.manifest.golden.required_selection_parity
            || max_top_two_gap_drift >= self.manifest.golden.adjusted_top_two_gap_tolerance
            || incremental_clean_embedding_max_abs.is_some_and(|drift| drift != 0.0)
        {
            return Err(anyhow!(
                "C82 encoder parity failed: selection parity {selection_parity:.6}, top-two gap drift {max_top_two_gap_drift:.8} (limit {:.8}), incremental/clean embedding drift {:?}",
                self.manifest.golden.adjusted_top_two_gap_tolerance,
                incremental_clean_embedding_max_abs
            ));
        }
        Ok(EncoderParityReport {
            cases: reports,
            max_score_drift,
            max_top_two_gap_drift,
            gap_drift_tolerance: self.manifest.golden.adjusted_top_two_gap_tolerance,
            selection_parity,
            incremental_clean_embedding_max_abs,
        })
    }

    pub async fn health(&self) -> Result<EncoderHealth> {
        let health: EncoderHealth = serde_json::from_value(
            self.encoder
                .call(serde_json::json!({"op": "health"}))
                .await?,
        )
        .context("parse C82 native encoder health")?;
        if health.status != "ready"
            || health.mixed_device_fallback
            || !health.bf16_supported
            || health.encoder_model != self.manifest.encoder.model
            || health.encoder_revision != self.manifest.encoder.revision
            || health.encoder_dimension != self.manifest.encoder.dimension
            || health.max_tokens != self.manifest.encoder.max_tokens
            || health.pooling != self.manifest.encoder.pooling
            || health.serialization != self.manifest.encoder.serialization
            || health.kv_chunk_tokens != self.manifest.encoder.kv_chunk_tokens
        {
            return Err(anyhow!(
                "C82 encoder health does not match the artifact contract"
            ));
        }
        let native = &self.manifest.encoder.native;
        if health.backend != "libllama"
            || health.backend_revision != native.llama_cpp_revision
            || !health.backend_active
            || !matches!(health.device.as_str(), "metal" | "cuda" | "cpu")
            || health.flash_attention != Some(native.flash_attention)
            || health.physical_batch_tokens != Some(native.physical_batch_tokens)
            || health.max_sessions != Some(native.max_sessions)
            || health.kv_cache_type.as_deref() != Some(native.kv_cache_type.as_str())
            || health.kv_unified != Some(native.kv_unified)
            || health.swa_full != Some(native.swa_full)
            || health.cuda_nccl != Some(native.cuda_nccl)
            || health.selected_device_compute_nodes.unwrap_or_default() == 0
            || health.other_device_compute_nodes != Some(0)
        {
            return Err(anyhow!(
                "C82 native encoder did not prove the pinned libllama backend"
            ));
        }
        Ok(health)
    }

    pub async fn route(
        &self,
        episode_id: &str,
        turns: &[HistoryTurn],
        state: &EpisodeState,
    ) -> Result<Route> {
        if episode_id.is_empty() {
            return Err(anyhow!("C82 episode ID cannot be empty"));
        }
        let (encoded, telemetry) = self.encode_history(episode_id, turns).await?;
        let head_started = Instant::now();
        let scores =
            self.estimator
                .q_values(&encoded.embedding, state.previous_arm, state.turn_index)?;
        let decision = policy::select(
            &self.manifest,
            scores,
            state,
            encoded.serialized_tokens,
            Instant::now(),
        )?;
        let head_latency_us = head_started.elapsed().as_micros() as u64;
        Ok(Route {
            decision,
            telemetry: RoutingTelemetry {
                head_latency_us,
                ..telemetry
            },
        })
    }

    async fn encode_history(
        &self,
        episode_id: &str,
        turns: &[HistoryTurn],
    ) -> Result<(EncodeResponse, RoutingTelemetry)> {
        let encode_started = Instant::now();
        let encoded: EncodeResponse = serde_json::from_value(
            self.encoder
                .call(serde_json::json!({
                    "op": "encode",
                    "episode_id": episode_id,
                    "turns": turns,
                }))
                .await?,
        )
        .context("parse C82 native encoder response")?;
        if encoded.device.is_empty() || encoded.embedding.len() != self.manifest.encoder.dimension {
            return Err(anyhow!("C82 encoder response is incompatible"));
        }
        let telemetry = RoutingTelemetry {
            device: encoded.device.clone(),
            encode_mode: encoded.encode_mode.clone(),
            serialized_tokens: encoded.serialized_tokens,
            full_history_tokens: encoded.full_history_tokens,
            truncated_tokens: encoded.truncated_tokens,
            cached_prefix_tokens: encoded.cached_prefix_tokens,
            kv_session_retained: encoded.kv_session_retained,
            kv_evictions: encoded.kv_evictions,
            encode_latency_ms: encode_started.elapsed().as_millis() as u64,
            head_latency_us: 0,
        };
        Ok((encoded, telemetry))
    }

    pub fn dispatch_body(&self, arm: usize, request: &Value) -> Result<Value> {
        let worker = self
            .manifest
            .workers
            .get(arm)
            .ok_or_else(|| anyhow!("C82 selected arm index is out of range"))?;
        dispatch_body_for_worker(worker, request)
    }

    pub fn score_embedding(
        &self,
        embedding: &[f32],
        previous_arm: Option<usize>,
        turn_index: u64,
    ) -> Result<Vec<f32>> {
        self.estimator.q_values(embedding, previous_arm, turn_index)
    }
}

pub fn dispatch_body_for_worker(worker: &WorkerManifest, request: &Value) -> Result<Value> {
    let mut body = request.clone();
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("Anthropic request body must be an object"))?;
    object.insert("model".to_owned(), Value::String(worker.model.clone()));
    object.insert(
        "provider".to_owned(),
        serde_json::json!({
            "order": worker.openrouter_provider_order,
            "allow_fallbacks": worker.openrouter_allow_fallbacks,
            "require_parameters": worker.openrouter_require_parameters
        }),
    );
    let extra = worker
        .extra_body
        .as_object()
        .ok_or_else(|| anyhow!("C82 worker extra_body must be an object"))?;
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    Ok(body)
}

#[derive(Clone, Debug, Serialize)]
pub struct HeadParityReport {
    pub cases: usize,
    pub max_score_drift: f32,
    pub selection_parity: f32,
    pub tolerance: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct EncoderParityReport {
    pub cases: Vec<EncoderCaseParityReport>,
    pub max_score_drift: f32,
    pub max_top_two_gap_drift: f32,
    pub gap_drift_tolerance: f32,
    pub selection_parity: f32,
    pub incremental_clean_embedding_max_abs: Option<f32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EncoderCaseParityReport {
    pub case: String,
    pub expected_selected_arm: usize,
    pub selected_arm: usize,
    pub max_score_drift: f32,
    pub top_two_gap_drift: f32,
    pub selection_flip: bool,
    pub telemetry: RoutingTelemetry,
}

#[derive(Debug, Deserialize)]
struct HeadGolden {
    schema_version: String,
    checkpoint_sha256: String,
    score_tolerance: f32,
    pool: Vec<String>,
    cases: Vec<HeadGoldenCase>,
}

#[derive(Debug, Deserialize)]
struct HeadGoldenCase {
    embedding: Vec<f32>,
    previous_model_index: i64,
    route_call_index: u64,
    scores: Vec<f32>,
    selected_index: usize,
}

fn validate_head_golden_contract(fixture: &HeadGolden) -> Result<()> {
    if fixture.schema_version != "rayline.mtrouter-head-golden.v1"
        || fixture.checkpoint_sha256 != CHECKPOINT_SHA256
        || fixture.pool.iter().map(String::as_str).collect::<Vec<_>>() != WORKER_ORDER
    {
        return Err(anyhow!("C82 head golden contract is incompatible"));
    }
    if fixture.cases.is_empty() {
        return Err(anyhow!("C82 head golden has no cases"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct EncoderGolden {
    schema_version: String,
    checkpoint_sha256: String,
    encoder: EncoderGoldenContract,
    cases: Vec<EncoderGoldenCase>,
    #[serde(default)]
    incremental_parity: Option<IncrementalParityContract>,
}

#[derive(Debug, Deserialize)]
struct EncoderGoldenContract {
    model: String,
    revision: String,
}

#[derive(Debug, Deserialize)]
struct EncoderGoldenCase {
    id: String,
    turns: Vec<HistoryTurn>,
    encode_mode: String,
    serialized_tokens: usize,
    full_history_tokens: usize,
    truncated_tokens: usize,
    scores: Vec<f32>,
    selected_index: usize,
    #[serde(default = "null_previous_model")]
    previous_model_index: i64,
    #[serde(default)]
    route_call_index: u64,
    #[serde(default)]
    episode_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IncrementalParityContract {
    delta_case: String,
    full_case: String,
}

fn null_previous_model() -> i64 {
    -1
}

fn top_two_gap(scores: &[f32]) -> Result<f32> {
    if scores.len() < 2 || scores.iter().any(|score| !score.is_finite()) {
        return Err(anyhow!(
            "C82 parity scores do not contain two finite values"
        ));
    }
    let mut top = f32::NEG_INFINITY;
    let mut second = f32::NEG_INFINITY;
    for score in scores {
        if *score > top {
            second = top;
            top = *score;
        } else if *score > second {
            second = *score;
        }
    }
    Ok(top - second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn worker(index: usize) -> WorkerManifest {
        WorkerManifest {
            id: format!("worker-{index}"),
            model: format!("provider/model-{index}"),
            api_key_env: "OPENROUTER_API_KEY".to_owned(),
            estimated_input_cost_per_token: 0.0,
            estimated_cache_read_cost_per_token: 0.0,
            estimated_cache_write_cost_per_token: 0.0,
            estimated_output_cost_per_token: 0.0,
            openrouter_provider_slug: format!("provider-{index}"),
            openrouter_provider_name: format!("Provider {index}"),
            openrouter_provider_order: vec![format!("provider-{index}")],
            openrouter_allow_fallbacks: false,
            openrouter_require_parameters: true,
            thinking_mode: if index >= 5 { "on" } else { "off" }.to_owned(),
            reasoning_budget_tokens: if index >= 5 { 32_768 } else { 0 },
            minimum_completion_tokens: if index >= 5 { 65_536 } else { 0 },
            max_completion_tokens: None,
            temperature: Some(1.0),
            supports_output_effort: false,
            extra_body: if index >= 5 {
                json!({"reasoning":{"enabled":true,"max_tokens":32768}})
            } else {
                json!({"reasoning":{"enabled":false}})
            },
            openrouter_max_retries: 3,
            openrouter_retry_base_seconds: 2.0,
            openrouter_retry_cap_seconds: 30.0,
            attempt_deadline_seconds: None,
        }
    }

    #[test]
    fn all_seven_dispatch_payloads_overwrite_client_policy_fields() {
        for index in 0..7 {
            let worker = worker(index);
            let body = dispatch_body_for_worker(
                &worker,
                &json!({
                    "model":"client/model",
                    "messages":[],
                    "provider":{"order":["client"],"allow_fallbacks":true},
                    "reasoning":{"enabled":"client"}
                }),
            )
            .unwrap();
            assert_eq!(body["model"], worker.model);
            assert_eq!(
                body["provider"]["order"],
                json!(worker.openrouter_provider_order)
            );
            assert_eq!(body["provider"]["allow_fallbacks"], false);
            assert_eq!(body["provider"]["require_parameters"], true);
            assert_eq!(body["reasoning"], worker.extra_body["reasoning"]);
        }
    }

    #[test]
    fn top_two_gap_rejects_invalid_and_handles_ties() {
        assert!(top_two_gap(&[1.0]).is_err());
        assert!(top_two_gap(&[1.0, f32::NAN]).is_err());
        assert_eq!(top_two_gap(&[1.0, 1.0, 0.0]).unwrap(), 0.0);
    }

    #[test]
    fn empty_head_golden_is_rejected() {
        let fixture: HeadGolden = serde_json::from_value(json!({
            "schema_version":"rayline.mtrouter-head-golden.v1",
            "checkpoint_sha256":CHECKPOINT_SHA256,
            "score_tolerance":1.0e-5,
            "pool":WORKER_ORDER,
            "cases":[]
        }))
        .unwrap();
        let error =
            validate_head_golden_contract(&fixture).expect_err("empty fixture must fail closed");
        assert!(error.to_string().contains("has no cases"));
    }
}

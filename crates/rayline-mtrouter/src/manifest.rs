use std::fs;
use std::io::{BufReader, Read as _};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const MANIFEST_SCHEMA: &str = "rayline.mtrouter-runtime.v3";
pub const ENCODER_MODEL: &str = "Qwen/Qwen3.5-0.8B";
pub const ENCODER_REVISION: &str = "2fc06364715b967f1860aea9cf38778875588b17";
pub const LLAMA_CPP_REVISION: &str = "8c5d694fe7e28e8973349b634a72fe7683ecc940";
pub const CHECKPOINT_SHA256: &str =
    "c2b0e63216c11f1496b47b22dff9f6c83baa6ef065e205a34897deff7493920f";
pub const WORKER_ORDER: [&str; 7] = [
    "z-ai/glm-5.2@thinking-off",
    "deepseek/deepseek-v4-pro@thinking-off",
    "deepseek/deepseek-v4-flash@thinking-off",
    "xiaomi/mimo-v2.5-pro@thinking-off",
    "qwen/qwen3.6-35b-a3b@thinking-off",
    "xiaomi/mimo-v2.5-pro@thinking-on",
    "deepseek/deepseek-v4-flash@thinking-on",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub schema_version: String,
    pub artifact_id: String,
    pub exporter_commit: String,
    pub weights: WeightsManifest,
    pub source: SourceManifest,
    pub encoder: EncoderManifest,
    pub architecture: ArchitectureManifest,
    pub policy: PolicyManifest,
    pub workers: Vec<WorkerManifest>,
    pub pricing_snapshot: PricingSnapshot,
    pub golden: GoldenManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WeightsManifest {
    pub file: String,
    pub sha256: String,
    pub dtype: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceManifest {
    pub checkpoint: SourceCheckpoint,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceCheckpoint {
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EncoderManifest {
    pub model: String,
    pub revision: String,
    pub dimension: usize,
    pub max_tokens: usize,
    pub min_recent_turns: usize,
    pub min_recent_tokens: usize,
    pub serialization: String,
    pub pooling: String,
    pub normalize_embeddings: bool,
    pub attention_implementation: String,
    pub dtype: String,
    pub incremental_default: bool,
    pub kv_chunk_tokens: usize,
    pub kv_session_budget_tokens: usize,
    pub kv_process_budget_tokens: usize,
    pub kv_idle_ttl_seconds: f64,
    pub golden: Option<EncoderGoldenManifest>,
    pub native: NativeEncoderManifest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncoderGoldenManifest {
    pub file: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeEncoderManifest {
    pub runtime: String,
    pub llama_cpp_repository: String,
    pub llama_cpp_revision: String,
    pub llama_cpp_tag: String,
    pub pooling_implementation: String,
    pub flash_attention: bool,
    pub physical_batch_tokens: usize,
    pub max_sessions: usize,
    pub kv_cache_type: String,
    pub kv_unified: bool,
    pub swa_full: bool,
    pub cuda_nccl: bool,
    pub gguf_conversion_command: String,
    pub gguf: NativeFileManifest,
    pub binaries: Vec<NativeBinaryManifest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeFileManifest {
    pub file: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeBinaryManifest {
    pub target: String,
    pub accelerator: String,
    pub file: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectureManifest {
    pub name: String,
    pub history_dimension: usize,
    pub arm_embedding_dimension: usize,
    pub joint_input_dimension: usize,
    pub hidden_dimensions: Vec<usize>,
    pub dropout: f64,
    pub pool: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PolicyManifest {
    pub previous_worker_stay_margin: f32,
    pub cold_switch_margin_per_usd: f32,
    pub cold_switch_upgrade_exempt: bool,
    pub stay_margin_upgrade_exempt: bool,
    pub reference_worker: String,
    pub reference_margin: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkerManifest {
    pub id: String,
    pub model: String,
    pub api_key_env: String,
    pub estimated_input_cost_per_token: f64,
    pub estimated_cache_read_cost_per_token: f64,
    pub estimated_cache_write_cost_per_token: f64,
    pub estimated_output_cost_per_token: f64,
    pub openrouter_provider_slug: String,
    pub openrouter_provider_name: String,
    pub openrouter_provider_order: Vec<String>,
    pub openrouter_allow_fallbacks: bool,
    pub openrouter_require_parameters: bool,
    pub thinking_mode: String,
    pub reasoning_budget_tokens: u64,
    pub minimum_completion_tokens: u64,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub supports_output_effort: bool,
    pub extra_body: Value,
    pub openrouter_max_retries: u64,
    pub openrouter_retry_base_seconds: f64,
    pub openrouter_retry_cap_seconds: f64,
    pub attempt_deadline_seconds: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PricingSnapshot {
    pub config_path: String,
    pub config_commit: String,
    pub mutable_live_prices_affect_decisions: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GoldenManifest {
    pub head: HeadGoldenManifest,
    pub adjusted_top_two_gap_tolerance: f32,
    pub required_selection_parity: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HeadGoldenManifest {
    pub file: String,
    pub sha256: String,
    pub score_tolerance: f32,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("read C82 manifest {}", path.display()))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse C82 manifest {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != MANIFEST_SCHEMA {
            return Err(anyhow!(
                "unsupported C82 manifest schema {:?}; expected {MANIFEST_SCHEMA}",
                self.schema_version
            ));
        }
        if self.source.checkpoint.sha256 != CHECKPOINT_SHA256 {
            return Err(anyhow!("C82 source checkpoint hash is not trusted"));
        }
        if self.weights.dtype != "F32" {
            return Err(anyhow!("C82 head weights must be F32"));
        }
        if self.encoder.model != ENCODER_MODEL
            || self.encoder.revision != ENCODER_REVISION
            || self.encoder.dimension != 1024
            || self.encoder.max_tokens != 262_144
            || self.encoder.min_recent_turns != 1
            || self.encoder.min_recent_tokens != 64
            || self.encoder.serialization != "mtrouter-token-blocks-v2"
            || self.encoder.pooling != "masked_mean"
            || !self.encoder.normalize_embeddings
            || self.encoder.attention_implementation != "sdpa"
            || self.encoder.dtype != "BF16"
            || !self.encoder.incremental_default
            || self.encoder.kv_chunk_tokens != 8_192
        {
            return Err(anyhow!(
                "C82 encoder contract does not match the validated runtime"
            ));
        }
        let native = &self.encoder.native;
        if native.runtime != "llama_cpp_native"
            || native.llama_cpp_repository != "davidvgilmore/llama.cpp"
            || native.llama_cpp_revision != LLAMA_CPP_REVISION
            || native.llama_cpp_tag != "b10153+rayline-cumulative-mean"
            || native.pooling_implementation != "libllama_fp32_cumulative_mean"
            || native.flash_attention
            || native.physical_batch_tokens != 512
            || native.max_sessions != 2
            || native.kv_cache_type != "BF16"
            || native.kv_unified
            || native.swa_full
            || native.cuda_nccl
            || native.gguf_conversion_command.is_empty()
            || native.gguf.file.is_empty()
            || native.binaries.is_empty()
            || native.binaries.iter().any(|binary| {
                binary.target.is_empty()
                    || binary.accelerator.is_empty()
                    || binary.file.is_empty()
                    || binary.sha256.is_empty()
            })
        {
            return Err(anyhow!("C82 native encoder contract is incompatible"));
        }
        let order = self
            .workers
            .iter()
            .map(|worker| worker.id.as_str())
            .collect::<Vec<_>>();
        let pool = self
            .architecture
            .pool
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if order != WORKER_ORDER || pool != WORKER_ORDER {
            return Err(anyhow!(
                "C82 arm order does not match the checkpoint index contract"
            ));
        }
        if self.architecture.name != "switch_aware"
            || self.architecture.history_dimension != 1024
            || self.architecture.arm_embedding_dimension != 64
            || self.architecture.joint_input_dimension != 1154
            || self.architecture.hidden_dimensions != [256, 256]
            || (self.architecture.dropout - 0.1).abs() > f64::EPSILON
        {
            return Err(anyhow!("C82 head architecture is incompatible"));
        }
        if (self.policy.previous_worker_stay_margin - 0.05).abs() > f32::EPSILON
            || (self.policy.cold_switch_margin_per_usd - 1.0).abs() > f32::EPSILON
            || !self.policy.cold_switch_upgrade_exempt
            || self.policy.stay_margin_upgrade_exempt
            || self.policy.reference_worker != WORKER_ORDER[0]
            || self.policy.reference_margin != 0.0
        {
            return Err(anyhow!("C82 decision policy is incompatible"));
        }
        if self.pricing_snapshot.mutable_live_prices_affect_decisions {
            return Err(anyhow!("C82 must use its immutable pricing snapshot"));
        }
        for worker in &self.workers {
            if worker.api_key_env != "OPENROUTER_API_KEY"
                || worker.openrouter_allow_fallbacks
                || !worker.openrouter_require_parameters
                || worker.openrouter_provider_slug.is_empty()
                || worker.openrouter_provider_order != [worker.openrouter_provider_slug.clone()]
                || worker.openrouter_max_retries != 3
                || worker.openrouter_retry_base_seconds != 2.0
                || worker.openrouter_retry_cap_seconds != 30.0
            {
                return Err(anyhow!(
                    "C82 worker {} has incompatible dispatch policy",
                    worker.id
                ));
            }
        }
        Ok(())
    }

    pub fn verify_core_files(&self, runtime_dir: &Path) -> Result<()> {
        for (relative, expected, label) in [
            (
                self.weights.file.as_str(),
                self.weights.sha256.as_str(),
                "head weights",
            ),
            (
                self.golden.head.file.as_str(),
                self.golden.head.sha256.as_str(),
                "head golden",
            ),
        ] {
            verify_file_hash(&runtime_dir.join(relative), expected)
                .with_context(|| format!("verify C82 {label}"))?;
        }
        if let Some(golden) = self.encoder.golden.as_ref() {
            verify_file_hash(&runtime_dir.join(&golden.file), &golden.sha256)
                .context("verify C82 encoder golden")?;
        }
        Ok(())
    }

    pub fn verify_native_files(
        &self,
        runtime_dir: &Path,
        binary: &NativeBinaryManifest,
    ) -> Result<()> {
        let native = &self.encoder.native;
        verify_file_hash(&runtime_dir.join(&native.gguf.file), &native.gguf.sha256)
            .context("verify C82 native GGUF")?;
        verify_file_hash(&runtime_dir.join(&binary.file), &binary.sha256)
            .context("verify C82 native binary")?;
        Ok(())
    }
}

pub fn verify_file_hash(path: &Path, expected: &str) -> Result<()> {
    if expected.len() != 64 || !expected.chars().all(|value| value.is_ascii_hexdigit()) {
        return Err(anyhow!("invalid expected SHA256 for {}", path.display()));
    }
    let file = fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let actual = format!("{:x}", digest.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(anyhow!(
            "SHA256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected,
            actual
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn corrupted_runtime_file_fails_hash_verification() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should follow the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rayline-mtrouter-corrupt-{}-{nonce}",
            std::process::id()
        ));
        fs::write(&path, b"trusted bytes").expect("write hash fixture");
        let trusted = format!("{:x}", Sha256::digest(b"trusted bytes"));
        verify_file_hash(&path, &trusted).expect("trusted fixture should verify");

        fs::write(&path, b"corrupted bytes").expect("corrupt hash fixture");
        let error =
            verify_file_hash(&path, &trusted).expect_err("corrupted fixture must fail closed");
        assert!(error.to_string().contains("SHA256 mismatch"));
        fs::remove_file(path).expect("remove hash fixture");
    }

    #[test]
    fn malformed_expected_hash_is_rejected() {
        let error = verify_file_hash(Path::new("unused"), "not-a-sha256")
            .expect_err("malformed expected hash must be rejected");
        assert!(error.to_string().contains("invalid expected SHA256"));
    }
}

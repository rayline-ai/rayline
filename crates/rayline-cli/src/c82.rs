//! Immutable C82 artifact and native libllama runtime provisioning.

use std::io;
use std::path::{Path, PathBuf};

use rand::Rng as _;
use rayline_mtrouter::{Manifest, NativeBinaryManifest, NativeEncoderOptions};

pub const REPO: &str = "rayline-ai/mtrouter-c82";
pub const COMMIT: &str = rayline_local_router::C82_ARTIFACT_COMMIT;
pub const MANIFEST_FILE: &str = "runtime/manifest.json";
pub const MANIFEST_SHA256: &str =
    "05e1a23105ec9d537d6cc5b1da7a06b01c7536b6c773d119d967d397bb95e043";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartConfig {
    pub runtime_dir: PathBuf,
    pub native_binary: PathBuf,
    pub native_model: PathBuf,
    pub device: String,
    pub memory_budget_gib: Option<String>,
    pub episode_prefix: String,
}

#[derive(Clone, Debug)]
pub struct Provisioned {
    pub runtime_dir: PathBuf,
    pub native_binary: PathBuf,
    pub native_model: PathBuf,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorRequest {
    pub device: String,
    pub memory_budget_gib: Option<String>,
}

pub async fn provision(home: &Path) -> io::Result<Provisioned> {
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || provision_blocking(&home))
        .await
        .map_err(|error| io::Error::other(format!("C82 provisioning task failed: {error}")))?
}

pub async fn start_config(
    home: &Path,
    device: &str,
    memory_budget_gib: Option<&str>,
) -> io::Result<StartConfig> {
    if std::env::var_os("OPENROUTER_API_KEY").is_none() {
        return Err(io::Error::other(
            "C82 dispatch requires inherited OPENROUTER_API_KEY",
        ));
    }
    let provisioned = provision(home).await?;
    Ok(StartConfig {
        runtime_dir: provisioned.runtime_dir,
        native_binary: provisioned.native_binary,
        native_model: provisioned.native_model,
        device: validate_device(device)
            .ok_or_else(|| io::Error::other("invalid C82 router device"))?,
        memory_budget_gib: memory_budget_gib
            .map(|value| {
                validate_memory_budget(value)
                    .ok_or_else(|| io::Error::other("invalid C82 router memory budget"))
            })
            .transpose()?,
        episode_prefix: random_secret(),
    })
}

fn provision_blocking(_home: &Path) -> io::Result<Provisioned> {
    if rayline_hf::hf_token().is_none() {
        return Err(io::Error::other(
            "C82 is private: inherit HF_TOKEN or HF_API_TOKEN before launching",
        ));
    }
    let manifest_path = download(MANIFEST_FILE, Some(MANIFEST_SHA256), "c82-manifest")?;
    let runtime_dir = manifest_path
        .parent()
        .ok_or_else(|| io::Error::other("C82 manifest has no runtime directory"))?
        .to_path_buf();
    let manifest = Manifest::load(&manifest_path).map_err(io::Error::other)?;
    let native = &manifest.encoder.native;
    let binary = platform_binary(&native.binaries)?;

    for (file, hash, stage) in [
        (
            format!("runtime/{}", manifest.weights.file),
            manifest.weights.sha256.clone(),
            "c82-weights",
        ),
        (
            format!("runtime/{}", manifest.golden.head.file),
            manifest.golden.head.sha256.clone(),
            "c82-head-golden",
        ),
        (
            format!("runtime/{}", native.gguf.file),
            native.gguf.sha256.clone(),
            "c82-native-gguf",
        ),
        (
            format!("runtime/{}", binary.file),
            binary.sha256.clone(),
            "c82-native-binary",
        ),
    ] {
        download(&file, Some(&hash), stage)?;
    }
    if let Some(golden) = manifest.encoder.golden.as_ref() {
        download(
            &format!("runtime/{}", golden.file),
            Some(&golden.sha256),
            "c82-encoder-golden",
        )?;
    }

    let native_binary = runtime_dir.join(&binary.file);
    let native_model = runtime_dir.join(&native.gguf.file);
    make_executable(&native_binary)?;
    manifest
        .verify_core_files(&runtime_dir)
        .and_then(|_| manifest.verify_native_files(&runtime_dir, binary))
        .map_err(io::Error::other)?;
    Ok(Provisioned {
        runtime_dir,
        native_binary,
        native_model,
        manifest,
    })
}

fn platform_binary(binaries: &[NativeBinaryManifest]) -> io::Result<&NativeBinaryManifest> {
    let (target, accelerator) = platform_contract()?;
    binaries
        .iter()
        .find(|binary| binary.target == target && binary.accelerator == accelerator)
        .ok_or_else(|| {
            io::Error::other(format!(
                "C82 artifact has no native binary for {target}/{accelerator}"
            ))
        })
}

fn platform_contract() -> io::Result<(&'static str, &'static str)> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok(("aarch64-apple-darwin", "metal"))
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok(("x86_64-unknown-linux-gnu", "cuda"))
    }
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    )))]
    {
        Err(io::Error::other(
            "C82 native runtime currently supports Apple Silicon Metal and Linux x86_64 CUDA",
        ))
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o500);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn download(filename: &str, expected_sha256: Option<&str>, stage: &str) -> io::Result<PathBuf> {
    if let Some(path) = rayline_hf::verified_hf_cache_file(REPO, filename, COMMIT, expected_sha256)
        .map_err(io::Error::other)?
    {
        return Ok(path);
    }
    rayline_hf::download_to_hf_cache(
        REPO,
        filename,
        COMMIT,
        expected_sha256,
        None,
        stage,
        None,
        0,
        0,
        None,
    )
    .map_err(io::Error::other)
}

pub fn validate_device(value: &str) -> Option<String> {
    matches!(value, "auto" | "mps" | "metal" | "cuda" | "cpu").then(|| value.to_owned())
}

pub fn validate_memory_budget(value: &str) -> Option<String> {
    let parsed = value.parse::<f64>().ok()?;
    (parsed.is_finite() && parsed > 0.0).then(|| value.to_owned())
}

fn native_device(value: &str) -> String {
    if value == "mps" {
        "metal".to_owned()
    } else {
        value.to_owned()
    }
}

fn native_options(
    provisioned: &Provisioned,
    request: &DoctorRequest,
) -> io::Result<NativeEncoderOptions> {
    let mut options = NativeEncoderOptions::c82(
        provisioned.native_binary.clone(),
        provisioned.native_model.clone(),
        native_device(&request.device),
    );
    options.memory_budget_gib = request
        .memory_budget_gib
        .as_deref()
        .map(str::parse::<f64>)
        .transpose()
        .map_err(|error| io::Error::other(format!("invalid C82 memory budget: {error}")))?;
    Ok(options)
}

pub async fn doctor(request: &DoctorRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    let provisioned = provision(&home).await?;
    let options = native_options(&provisioned, request)?;
    let router = rayline_mtrouter::C82Router::load_native(&provisioned.runtime_dir, options)
        .await
        .map_err(io::Error::other)?;
    let health = router.health().await.map_err(io::Error::other)?;
    let head = router.verify_head_golden().map_err(io::Error::other)?;
    let encoder = router
        .verify_encoder_golden()
        .await
        .map_err(io::Error::other)?;
    let native = &provisioned.manifest.encoder.native;
    let binary = platform_binary(&native.binaries)?;
    rayline_mtrouter::verify_file_hash(&provisioned.native_binary, &binary.sha256)
        .map_err(io::Error::other)?;
    let output = serde_json::json!({
        "status": "ready",
        "orchestrator": "c82",
        "runtime": "llama_cpp_native",
        "artifact": {
            "repo": REPO,
            "commit": COMMIT,
            "manifest_sha256": MANIFEST_SHA256,
            "weights_sha256": provisioned.manifest.weights.sha256,
            "gguf_sha256": native.gguf.sha256,
            "native_binary_sha256": binary.sha256,
            "checkpoint_sha256": rayline_mtrouter::CHECKPOINT_SHA256,
        },
        "accelerator": {
            "requested": request.device,
            "selected": health.device,
            "backend": health.backend,
            "backend_revision": health.backend_revision,
            "backend_active": health.backend_active,
            "bf16_supported": health.bf16_supported,
            "mixed_device_fallback": health.mixed_device_fallback,
            "selected_device_compute_nodes": health.selected_device_compute_nodes,
            "host_boundary_nodes": health.host_boundary_nodes,
            "other_device_compute_nodes": health.other_device_compute_nodes,
            "cuda_nccl": health.cuda_nccl,
            "device_free_bytes": health.device_free_bytes,
            "device_total_bytes": health.device_total_bytes,
            "device_allocated_bytes": health.device_allocated_bytes,
            "memory_budget_gib": request.memory_budget_gib,
        },
        "encoder": {
            "ready": true,
            "model": health.encoder_model,
            "revision": health.encoder_revision,
            "dimension": health.encoder_dimension,
            "max_tokens": health.max_tokens,
            "pooling": health.pooling,
            "serialization": health.serialization,
            "kv_session_budget_tokens": health.kv_session_budget_tokens,
            "kv_process_budget_tokens": health.kv_process_budget_tokens,
        },
        "parity": {
            "head": head,
            "encoder_probe": encoder,
        }
    });
    serde_json::to_string_pretty(&output).map_err(io::Error::other)
}

pub fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut value, "{byte:02x}");
    }
    value
}

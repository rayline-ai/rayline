//! Curated local-model catalog for the `<cli> local` Recommended picker.
//!
//! The list comes from the published Rayline model registry
//! (`registry.rayline.ai/models.json`) filtered to entries whose `curated` tags
//! include this build's CLI binary name.
//! Hardware fit is computed client-side with the same
//! formula as the desktop's `recommendation.ts`:
//! `baseRamBytes + kvCacheBytesPerToken * context`, green below 70% of total
//! RAM, amber below 90%, red above.
//!
//! Downloads reuse `rayline-hf` — the identical implementation the `rld`
//! daemon uses — so files land in the standard HF hub cache and the daemon
//! sees them as a warm cache on its next start.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

const REGISTRY_PROD_URL: &str = "https://registry.rayline.ai/models.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Context budgets mirroring the desktop's `recommendedContextLength`:
/// 64K tokens on <=16 GB machines, 128K above.
const SMALL_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const CONTEXT_SMALL: u64 = 65_536;
const CONTEXT_LARGE: u64 = 131_072;

/// Leave ~10% of a budget for the OS / driver / runtime allocator. Applied to
/// discrete-VRAM and CPU-RAM ceilings; the Apple ⅔ figure already bakes in the
/// OS reserve, so it is used as-is.
// consumed by Task 3/4 call sites; allow until wired up
#[allow(dead_code)]
const MEMORY_HEADROOM_NUM: u64 = 9;
#[allow(dead_code)]
const MEMORY_HEADROOM_DEN: u64 = 10;
/// Apple Silicon caps GPU-wired memory at ~⅔ of unified RAM (the figure the old
/// `fit()` comment cited). Conservative by default; advanced users can raise
/// `iogpu.wired_limit_pct`.
#[allow(dead_code)]
const APPLE_UNIFIED_GPU_NUM: u64 = 2;
#[allow(dead_code)]
const APPLE_UNIFIED_GPU_DEN: u64 = 3;

/// Two derived memory budgets for a machine.
// consumed by Task 3/4 call sites; allow until wired up
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
struct Budgets {
    /// Accelerator memory the model's working set must fit in.
    ceiling_bytes: u64,
    /// All memory the resident weights can occupy (host + device).
    total_bytes: u64,
}

/// Derive the hot ceiling and total resident budget from detected hardware.
/// `None` when no memory could be detected (no verdict possible).
// consumed by Task 3/4 call sites; allow until wired up
#[allow(dead_code)]
fn budgets(hw: &rayline_llama::HardwareInfo) -> Option<Budgets> {
    let ram = hw.total_ram_bytes;
    let vram = hw.gpu_vram_bytes;
    if ram == 0 && vram == 0 {
        return None;
    }
    let budgets = match hw.gpu_type.as_str() {
        // Discrete GPU: the binding constraint is VRAM; weights may also spill
        // to host RAM, so total = RAM + VRAM guards loadability.
        "nvidia" if vram > 0 => Budgets {
            ceiling_bytes: vram * MEMORY_HEADROOM_NUM / MEMORY_HEADROOM_DEN,
            total_bytes: ram.saturating_add(vram),
        },
        // Apple Silicon: one unified pool; GPU-wired cap ~⅔ of RAM.
        "apple-silicon" => Budgets {
            ceiling_bytes: ram * APPLE_UNIFIED_GPU_NUM / APPLE_UNIFIED_GPU_DEN,
            total_bytes: ram,
        },
        // CPU-only or an accelerator detection missed: budget against RAM.
        _ => Budgets {
            ceiling_bytes: ram * MEMORY_HEADROOM_NUM / MEMORY_HEADROOM_DEN,
            total_bytes: ram,
        },
    };
    Some(budgets)
}

/// Detect this machine's hardware once per process (cached), mirroring
/// `detect_total_ram`'s `OnceLock`. Returns `None` only if a future detector
/// signals total failure; today `detect_hardware` always returns a value.
pub fn detect_hardware() -> Option<&'static rayline_llama::HardwareInfo> {
    static HARDWARE: std::sync::OnceLock<rayline_llama::HardwareInfo> = std::sync::OnceLock::new();
    Some(HARDWARE.get_or_init(rayline_llama::detect_hardware))
}

/// One curated registry entry (the subset of `ModelEntry` fields this CLI
/// needs; unknown fields are ignored).
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub filename: String,
    pub revision: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub base_ram_bytes: u64,
    pub kv_cache_bytes_per_token: u64,
    pub max_context_window: u64,
    pub quality_score: u64,
    pub description: String,
}

/// Hardware fit at the recommended context length, matching the desktop's
/// green/amber/red thresholds (70% / 90% of total RAM).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fit {
    Green,
    Amber,
    Red,
    /// RAM could not be detected on this platform; no verdict.
    Unknown,
}

impl Fit {
    fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Amber => "amber",
            Self::Red => "red",
            Self::Unknown => "unknown",
        }
    }

    /// Preference order for the auto-pick: comfortable fits first, but a red
    /// fit still ranks (a legacy download on a small machine beats nothing).
    fn rank(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Amber => 1,
            Self::Unknown => 2,
            Self::Red => 3,
        }
    }
}

/// Fetch the registry catalog and keep only the entries curated for this CLI,
/// smallest first. Entries without registry-provided revision/SHA pins are
/// ignored, because curated downloads must be verifiable without embedding a
/// model allowlist in the binary. Multi-file (sharded) entries are skipped —
/// the download path handles a single GGUF. `RAYLINE_MODELS_REGISTRY_URL`
/// overrides the registry URL (testing against a local/staged catalog).
pub async fn fetch_curated(env_name: &str) -> Vec<CatalogModel> {
    match try_fetch_curated(env_name).await {
        Ok(models) if !models.is_empty() => models,
        _ => fallback_curated(),
    }
}

/// One client per process: reuses the connection pool / TLS session across
/// the catalog fetch and any retries within a command.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .expect("default reqwest client config is infallible")
    })
}

async fn try_fetch_curated(env_name: &str) -> Result<Vec<CatalogModel>, String> {
    let override_url = std::env::var("RAYLINE_MODELS_REGISTRY_URL").ok();
    let url = override_url.as_deref().unwrap_or(registry_url(env_name));
    let response = http_client()
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(response.status().to_string());
    }
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    Ok(parse_curated(&body))
}

/// There is intentionally no embedded trust fallback: model revision/SHA pins
/// live in the registry so new curated models can be added without a CLI
/// release. If the registry is unavailable or missing pins, the recommended
/// picker shows no download candidates instead of starting an unverified model.
fn fallback_curated() -> Vec<CatalogModel> {
    Vec::new()
}

fn registry_url(_env_name: &str) -> &'static str {
    REGISTRY_PROD_URL
}

fn parse_curated(body: &Value) -> Vec<CatalogModel> {
    let Some(entries) = body.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut models = entries
        .iter()
        .filter(|entry| {
            entry
                .get("curated")
                .and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(crate::CLI_BIN)))
                && entry.get("shardedFilenames").is_none()
        })
        .filter_map(parse_model)
        .collect::<Vec<_>>();
    models.sort_by_key(|model| model.base_ram_bytes);
    models
}

fn parse_model(entry: &Value) -> Option<CatalogModel> {
    let revision = parse_revision(entry)?;
    let sha256 = parse_sha256(entry)?;
    Some(CatalogModel {
        id: string_field(entry, "id")?,
        name: string_field(entry, "name")?,
        repo: string_field(entry, "repo")?,
        filename: string_field(entry, "filename")?,
        revision,
        sha256,
        size_bytes: entry.get("sizeBytes")?.as_u64()?,
        base_ram_bytes: entry.get("baseRamBytes")?.as_u64()?,
        kv_cache_bytes_per_token: entry.get("kvCacheBytesPerToken")?.as_u64()?,
        max_context_window: entry.get("maxContextWindow")?.as_u64()?,
        quality_score: entry.get("qualityScore")?.as_u64()?,
        description: entry.get("description")?.as_str()?.to_owned(),
    })
}

fn string_field(entry: &Value, field: &str) -> Option<String> {
    let value = entry.get(field)?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_revision(entry: &Value) -> Option<String> {
    let revision = entry
        .get("revision")
        .or_else(|| entry.get("hfRevision"))?
        .as_str()?
        .trim();
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(revision.to_ascii_lowercase())
}

fn parse_sha256(entry: &Value) -> Option<String> {
    let digest = entry
        .get("sha256")
        .or_else(|| entry.get("digest"))?
        .as_str()?;
    rayline_hf::normalize_sha256(digest).ok()
}

/// RAM the model needs at the context length this machine would run it with.
pub fn required_ram_bytes(model: &CatalogModel, total_ram_bytes: u64) -> u64 {
    let context = if total_ram_bytes <= SMALL_RAM_BYTES {
        CONTEXT_SMALL
    } else {
        CONTEXT_LARGE
    }
    .min(model.max_context_window);
    model.base_ram_bytes + model.kv_cache_bytes_per_token * context
}

/// Coarse fit verdict against **total physical RAM**.
///
/// KNOWN LIMITATION (follow-up: GPU-aware fit + context autosizing): this
/// compares against total RAM, but the real ceiling for the local model is
/// the GPU memory budget, which is smaller — on Apple Silicon the OS caps
/// GPU-wired memory at ~⅔ of RAM, and on discrete GPUs it is VRAM, not system
/// RAM. It also assumes a fixed context tier that may not match the context
/// the daemon actually loads. As a result a `green`/`amber` verdict can still
/// OOM at llama-server warmup on memory-constrained machines (e.g. a 27B model
/// on a 24GB Mac). `rayline_llama::detect_hardware()` already exposes gpu_type /
/// gpu_vram_bytes; a future revision should budget against those and size the
/// context to fit, passing the same value to the daemon as `RAYLINE_CTX_SIZE`.
pub fn fit(model: &CatalogModel, total_ram_bytes: Option<u64>) -> Fit {
    let Some(total) = total_ram_bytes.filter(|total| *total > 0) else {
        return Fit::Unknown;
    };
    let required = required_ram_bytes(model, total) as f64;
    let ratio = required / total as f64;
    if ratio <= 0.70 {
        Fit::Green
    } else if ratio <= 0.90 {
        Fit::Amber
    } else {
        Fit::Red
    }
}

/// Total physical RAM, detected once per process (the macOS path spawns a
/// `sysctl` subprocess — no reason to repeat it within a command).
/// `None` when detection fails (no fit verdict then).
pub fn detect_total_ram() -> Option<u64> {
    static TOTAL_RAM: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TOTAL_RAM.get_or_init(detect_total_ram_uncached)
}

fn detect_total_ram_uncached() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = meminfo
            .lines()
            .find(|line| line.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            length: u32,
            memory_load: u32,
            total_phys: u64,
            avail_phys: u64,
            total_page_file: u64,
            avail_page_file: u64,
            total_virtual: u64,
            avail_virtual: u64,
            avail_extended_virtual: u64,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a properly initialized MEMORYSTATUSEX with
        // `length` set, as the Win32 API requires.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            Some(status.total_phys)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Whether the model's GGUF is present in the HF hub cache at its pinned
/// revision and with its expected digest.
pub fn is_downloaded(model: &CatalogModel) -> bool {
    downloaded_path(model).is_some()
}

/// Resolve the model to serve when local routing is enabled but no model has
/// been picked yet: the best already-downloaded curated model, or `None` when
/// nothing is downloaded. Never downloads anything.
pub async fn auto_select_downloaded(env_name: &str) -> Option<CatalogModel> {
    let models = fetch_curated(env_name).await;
    let total_ram = detect_total_ram();
    let downloaded = models.into_iter().filter(is_downloaded).collect::<Vec<_>>();
    choose_auto_pick(downloaded, total_ram)
}

/// Best of the already-downloaded curated models: hardware fit first, quality
/// second. Pure so the policy is unit-testable.
fn choose_auto_pick(downloaded: Vec<CatalogModel>, total_ram: Option<u64>) -> Option<CatalogModel> {
    downloaded.into_iter().min_by_key(|model| {
        (
            fit(model, total_ram).rank(),
            std::cmp::Reverse(model.quality_score),
        )
    })
}

fn downloaded_path(model: &CatalogModel) -> Option<PathBuf> {
    rayline_hf::verified_hf_cache_file(
        &model.repo,
        &model.filename,
        &model.revision,
        Some(&model.sha256),
    )
    .ok()
    .flatten()
}

/// A curated model annotated with this machine's status, for `local models`.
pub struct ModelListing {
    pub model: CatalogModel,
    pub fit: Fit,
    pub downloaded: bool,
    pub selected: bool,
}

pub fn listings(
    models: Vec<CatalogModel>,
    total_ram_bytes: Option<u64>,
    selected_id: Option<&str>,
) -> Vec<ModelListing> {
    models
        .into_iter()
        .map(|model| ModelListing {
            fit: fit(&model, total_ram_bytes),
            downloaded: is_downloaded(&model),
            selected: selected_id == Some(model.id.as_str()),
            model,
        })
        .collect()
}

/// Machine-readable `local models --json` payload (consumed by the menu bar
/// app). One JSON object on stdout.
pub fn render_listings_json(listings: &[ModelListing], total_ram_bytes: Option<u64>) -> String {
    let models = listings
        .iter()
        .map(|listing| {
            json!({
                "id": listing.model.id,
                "name": listing.model.name,
                "repo": listing.model.repo,
                "filename": listing.model.filename,
                "size_bytes": listing.model.size_bytes,
                "quality_score": listing.model.quality_score,
                "description": listing.model.description,
                "required_ram_bytes": total_ram_bytes
                    .map(|total| required_ram_bytes(&listing.model, total)),
                "fit": listing.fit.as_str(),
                "downloaded": listing.downloaded,
                "selected": listing.selected,
            })
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "total_ram_bytes": total_ram_bytes,
        "models": models,
    });
    format!("{payload}\n")
}

/// Two sections: installed models (selectable, whatever their fit — they are
/// already on disk) and suitable downloads (red-fit entries hidden as noise,
/// with a count so the omission is visible).
pub fn render_listings_human(listings: &[ModelListing], total_ram_bytes: Option<u64>) -> String {
    let cli = crate::CLI_BIN;
    let mut output = String::new();
    match total_ram_bytes {
        Some(total) => output.push_str(&format!(
            "Recommended local models (this machine: {} RAM):\n",
            format_bytes(total)
        )),
        None => output.push_str("Recommended local models:\n"),
    }

    let fit_label = |fit: Fit| match fit {
        Fit::Green => "fits well",
        Fit::Amber => "tight fit",
        Fit::Red => "too large for this machine",
        Fit::Unknown => "fit unknown",
    };
    let entry = |listing: &ModelListing| {
        format!(
            "{marker} {id}\n    {name} — {size}, {fit}\n    {description}\n",
            marker = if listing.selected { "*" } else { " " },
            id = listing.model.id,
            name = listing.model.name,
            size = format_bytes(listing.model.size_bytes),
            fit = fit_label(listing.fit),
            description = listing.model.description,
        )
    };

    output.push_str("\nInstalled:\n");
    let installed = listings.iter().filter(|l| l.downloaded).collect::<Vec<_>>();
    if installed.is_empty() {
        output.push_str("  (none yet)\n");
    }
    for listing in installed {
        output.push_str(&entry(listing));
    }

    output.push_str("\nAvailable to download:\n");
    let available = listings
        .iter()
        .filter(|l| !l.downloaded && l.fit != Fit::Red)
        .collect::<Vec<_>>();
    let hidden = listings
        .iter()
        .filter(|l| !l.downloaded && l.fit == Fit::Red)
        .count();
    if available.is_empty() {
        output.push_str("  No models suitable to download for this machine.\n");
    }
    for listing in available {
        output.push_str(&entry(listing));
    }
    if hidden > 0 {
        output.push_str(&format!(
            "  ({hidden} larger model{s} hidden — too large for this machine)\n",
            s = if hidden == 1 { "" } else { "s" },
        ));
    }

    output.push_str(&format!(
        "\nSelect (downloading first if needed) with `{cli} local use <model-id>`.\n"
    ));
    output
}

pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

/// Download `model` into the HF hub cache, reporting progress. With
/// `json` the progress stream is NDJSON on stdout (`download_progress`
/// events mirroring the daemon's `RAYLINE_PROGRESS` payload, then `complete`);
/// otherwise a live progress bar is drawn on stderr. Resumes partial
/// downloads (rayline-hf keeps a `.tmp` + URL sidecar).
pub async fn download(model: &CatalogModel, json: bool) -> Result<PathBuf, String> {
    if let Some(path) = downloaded_path(model) {
        if json {
            emit_json_line(&json!({
                "event": "complete",
                "id": model.id,
                "path": path.display().to_string(),
                "cached": true,
            }));
        } else {
            eprintln!("{} is already downloaded ({}).", model.id, path.display());
        }
        return Ok(path);
    }

    if json {
        emit_json_line(&json!({
            "event": "start",
            "id": model.id,
            "repo": model.repo,
            "filename": model.filename,
            "total": model.size_bytes,
        }));
    } else {
        eprintln!(
            "Downloading {} ({}) from {}…",
            model.id,
            format_bytes(model.size_bytes),
            model.repo
        );
    }

    let repo = model.repo.clone();
    let filename = model.filename.clone();
    let revision = model.revision.clone();
    let sha256 = model.sha256.clone();
    let path = tokio::task::spawn_blocking(move || {
        let callback = |progress: rayline_hf::DownloadProgress| {
            if json {
                emit_json_line(&json!({
                    "event": "download_progress",
                    "stage": progress.stage,
                    "filename": progress.filename,
                    "bytes": progress.bytes_downloaded,
                    "total": progress.total_bytes,
                    "percent": progress.percent,
                }));
            } else {
                render_progress_bar(&progress);
            }
        };
        // Report the real GGUF name in progress events, not the `.tmp` blob.
        rayline_hf::download_to_hf_cache(
            &repo,
            &filename,
            &revision,
            Some(&sha256),
            Some(&callback),
            "model",
            None,
            0,
            0,
            Some(filename.as_str()),
        )
    })
    .await
    .map_err(|error| format!("download task failed: {error}"))??;

    if json {
        emit_json_line(&json!({
            "event": "complete",
            "id": model.id,
            "path": path.display().to_string(),
            "cached": false,
        }));
    } else {
        // Terminate the in-place progress bar line before the summary.
        eprintln!();
        eprintln!("Downloaded to {}.", path.display());
    }
    Ok(path)
}

/// `<cli> local models [--json]`.
pub async fn models_command(env_name: Option<&str>, json: bool) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let env_name = crate::status::resolve_env(env_name, Some(&home));
    let models = fetch_curated(&env_name).await;
    let total_ram = detect_total_ram();
    // Only a Recommended-mode pick counts as the selected catalog model. In
    // Custom mode `model_id` is still populated (preserved for switching back),
    // but the active selection is the custom endpoint, not this row.
    let selected_id = crate::local_model::read_from_home(&home).and_then(|config| {
        matches!(config.mode, crate::local_model::LocalModelMode::Recommended)
            .then_some(config.model_id)
            .flatten()
    });
    let listings = listings(models, total_ram, selected_id.as_deref());
    Ok(if json {
        render_listings_json(&listings, total_ram)
    } else {
        render_listings_human(&listings, total_ram)
    })
}

/// `<cli> local download <model-id> [--json]`. One catalog fetch serves both
/// the id lookup and the post-download auto-select.
pub async fn download_command(
    env_name: Option<&str>,
    model_id: &str,
    json: bool,
) -> Result<(), String> {
    let (home, models) = fetch_for_command(env_name).await?;
    let model = find_in(&models, model_id)?;
    download(&model, json).await?;
    auto_select_if_sole_model(&home, &models, &model, json);
    Ok(())
}

/// Resolve home + env and fetch the curated catalog once for a command.
async fn fetch_for_command(
    env_name: Option<&str>,
) -> Result<(std::path::PathBuf, Vec<CatalogModel>), String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let env_name = crate::status::resolve_env(env_name, Some(&home));
    let models = fetch_curated(&env_name).await;
    Ok((home, models))
}

/// After a successful download, adopt the model as the selection when it is
/// the only added model on this machine (nothing else downloaded, no complete
/// custom endpoint) — a first download shouldn't need a second "select" step.
/// Other Rayline clients can reload config afterwards; unknown NDJSON events are
/// ignored by older consumers.
fn auto_select_if_sole_model(
    home: &std::path::Path,
    models: &[CatalogModel],
    model: &CatalogModel,
    json: bool,
) {
    let cfg = crate::local_model::read_from_home(home);
    let downloaded_ids = models
        .iter()
        .filter(|m| is_downloaded(m))
        .map(|m| m.id.clone())
        .collect::<Vec<_>>();
    if !should_auto_select(cfg.as_ref(), &downloaded_ids, &model.id) {
        return;
    }
    if crate::local_model::set_recommended_in_home(home, model).is_ok() {
        if json {
            emit_json_line(&json!({ "event": "auto_selected", "id": model.id }));
        } else {
            eprintln!("Selected `{}` — your only added model.", model.id);
        }
    }
}

/// `<cli> local remove <model-id>`: delete the model's GGUF from the local
/// cache. Clears the pick when the removed model was the selected one (the
/// selection would otherwise point at a file that no longer exists).
pub async fn remove_command(env_name: Option<&str>, model_id: &str) -> Result<String, String> {
    let (home, models) = fetch_for_command(env_name).await?;
    let model = find_in(&models, model_id)?;
    let Some(path) = downloaded_path(&model) else {
        return Err(format!("`{model_id}` is not downloaded."));
    };
    rayline_hf::delete_model_and_shards_from_hf_cache(&path)?;

    let mut output = format!(
        "Removed {id} ({size} freed). The file came out of the shared Hugging Face cache, so other apps using it lose it too.",
        id = model.id,
        size = format_bytes(model.size_bytes),
    );
    if let Some(cfg) = crate::local_model::read_from_home(&home) {
        if cfg.model_id.as_deref() == Some(model.id.as_str()) {
            crate::local_model::clear_recommended_pick_in_home(&home)
                .map_err(|error| format!("failed to update settings: {error}"))?;
            output.push_str("\nIt was your selected model — the selection has been cleared.");
        }
    }
    // When exactly one added model remains and nothing valid is selected,
    // select it — even a custom endpoint.
    if let Some(note) = ensure_sole_added_model_selected(&models, &home) {
        output.push_str(&note);
    }
    Ok(output)
}

/// When no valid selection exists and the added-models list (downloaded
/// curated models + saved custom endpoints) has exactly one entry, select it.
/// Returns a user-facing note when a selection was made.
fn ensure_sole_added_model_selected(
    models: &[CatalogModel],
    home: &std::path::Path,
) -> Option<String> {
    let cfg = crate::local_model::read_from_home(home);
    if cfg.as_ref().is_some_and(|cfg| cfg.is_engageable()) {
        return None;
    }
    let downloaded = models
        .iter()
        .filter(|m| is_downloaded(m))
        .cloned()
        .collect::<Vec<_>>();
    let endpoints = cfg.map(|cfg| cfg.custom_endpoints).unwrap_or_default();
    match (downloaded.as_slice(), endpoints.as_slice()) {
        ([model], []) => crate::local_model::set_recommended_in_home(home, model)
            .ok()
            .map(|_| format!("\nSelected `{}` — your only added model.", model.id)),
        ([], [endpoint]) => crate::local_model::activate_custom_endpoint_in_home(home, endpoint)
            .ok()
            .map(|_| {
                format!(
                    "\nSelected your custom endpoint `{}` ({}) — your only added model.",
                    endpoint.model, endpoint.base_url,
                )
            }),
        _ => None,
    }
}

/// Pure decision: auto-select only when the just-downloaded model is the sole
/// added model — no other curated download and no saved custom endpoint
/// (`read_from_home` already counts a bare active URL+model pair as one).
fn should_auto_select(
    cfg: Option<&crate::local_model::LocalModelConfig>,
    downloaded_ids: &[String],
    just_downloaded: &str,
) -> bool {
    let endpoints_added = cfg.is_some_and(|cfg| !cfg.custom_endpoints.is_empty());
    !endpoints_added && downloaded_ids == [just_downloaded]
}

/// `<cli> local use <model-id>`: download if missing, then select.
pub async fn use_command(env_name: Option<&str>, model_id: &str) -> Result<String, String> {
    let (_, models) = fetch_for_command(env_name).await?;
    let model = find_in(&models, model_id)?;
    download(&model, false).await?;
    crate::local_model::set_recommended(&model)?;
    let cli = crate::CLI_BIN;
    Ok(format!(
        "Local model set to {id} ({name}).\nLocal routing uses it once enabled for your account (`{cli} local on`).",
        id = model.id,
        name = model.name,
    ))
}

fn find_in(models: &[CatalogModel], model_id: &str) -> Result<CatalogModel, String> {
    models
        .iter()
        .find(|model| model.id == model_id)
        .cloned()
        .ok_or_else(|| {
            let available = models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("Unknown model id `{model_id}`. Available: {available}")
        })
}

/// Final `error` NDJSON event for `--json` consumers (the menu bar app).
pub fn emit_error_json(message: &str) {
    emit_json_line(&json!({ "event": "error", "message": message }));
}

fn emit_json_line(payload: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{payload}");
    let _ = stdout.flush();
}

fn render_progress_bar(progress: &rayline_hf::DownloadProgress) {
    const WIDTH: usize = 30;
    let percent = progress.percent.clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * WIDTH as f64).round() as usize;
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));
    let total = if progress.total_bytes > 0 {
        format_bytes(progress.total_bytes)
    } else {
        "?".to_owned()
    };
    let mut stderr = std::io::stderr().lock();
    let _ = write!(
        stderr,
        "\r  [{bar}] {percent:5.1}%  {downloaded} / {total}   ",
        downloaded = format_bytes(progress.bytes_downloaded),
    );
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(
        total_ram_bytes: u64,
        gpu_type: &str,
        gpu_vram_bytes: u64,
    ) -> rayline_llama::HardwareInfo {
        rayline_llama::HardwareInfo {
            total_ram_bytes,
            gpu_type: gpu_type.to_owned(),
            gpu_model: String::new(),
            gpu_vram_bytes,
            os: "test".to_owned(),
            arch: "test".to_owned(),
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn budgets_apple_unified_caps_at_two_thirds_of_ram() {
        let b = budgets(&hw(24 * GIB, "apple-silicon", 0)).unwrap();
        assert_eq!(b.ceiling_bytes, 24 * GIB * 2 / 3);
        assert_eq!(b.total_bytes, 24 * GIB);
    }

    #[test]
    fn budgets_discrete_gpu_uses_vram_ceiling_and_summed_total() {
        let b = budgets(&hw(32 * GIB, "nvidia", 8 * GIB)).unwrap();
        assert_eq!(b.ceiling_bytes, 8 * GIB * 9 / 10); // 10% headroom
        assert_eq!(b.total_bytes, 32 * GIB + 8 * GIB);
    }

    #[test]
    fn budgets_cpu_only_uses_ram_with_headroom() {
        let b = budgets(&hw(32 * GIB, "none", 0)).unwrap();
        assert_eq!(b.ceiling_bytes, 32 * GIB * 9 / 10);
        assert_eq!(b.total_bytes, 32 * GIB);
    }

    #[test]
    fn budgets_unknown_when_no_memory_detected() {
        assert!(budgets(&hw(0, "none", 0)).is_none());
    }

    fn registry_model(id: &str, base_ram_bytes: u64) -> Value {
        json!({
            "id": id,
            "name": id,
            "repo": "example/repo",
            "filename": "model.gguf",
            "revision": "ffffffffffffffffffffffffffffffffffffffff",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sizeBytes": 1,
            "baseRamBytes": base_ram_bytes,
            "kvCacheBytesPerToken": 1,
            "maxContextWindow": 1024,
            "qualityScore": 1,
            "description": "test model",
            "curated": [crate::CLI_BIN],
        })
    }

    #[test]
    fn parse_curated_trusts_pinned_registry_curated_entries() {
        let body = json!({
            "models": [
                registry_model("qwen2.5-coder-7b-q5km", 1),
                registry_model("qwen3.6-27b-q4km", 2),
            ],
        });

        let models = parse_curated(&body);

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["qwen2.5-coder-7b-q5km", "qwen3.6-27b-q4km"]
        );
    }

    #[test]
    fn parse_curated_uses_registry_revision_and_sha_for_dynamic_model() {
        let body = json!({ "models": [registry_model("qwen3.6-27b-new", 2)] });

        let models = parse_curated(&body);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3.6-27b-new");
        assert_eq!(models[0].repo, "example/repo");
        assert_eq!(models[0].filename, "model.gguf");
        assert_eq!(
            models[0].revision,
            "ffffffffffffffffffffffffffffffffffffffff"
        );
        assert_eq!(
            models[0].sha256,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn parse_curated_rejects_entry_without_registry_revision_or_sha() {
        let mut model = registry_model("qwen3.6-27b-q4km", 2);
        let object = model.as_object_mut().unwrap();
        object.remove("revision");
        object.remove("sha256");
        let body = json!({ "models": [model] });

        let models = parse_curated(&body);

        assert!(models.is_empty());
    }

    #[test]
    fn parse_curated_rejects_invalid_registry_revision_or_sha() {
        let mut bad_revision = registry_model("qwen3.6-27b-bad-revision", 1);
        bad_revision
            .as_object_mut()
            .unwrap()
            .insert("revision".to_owned(), Value::String("main".to_owned()));
        let mut bad_sha = registry_model("qwen3.6-27b-bad-sha", 2);
        bad_sha
            .as_object_mut()
            .unwrap()
            .insert("sha256".to_owned(), Value::String("bad".to_owned()));
        let body = json!({ "models": [bad_revision, bad_sha] });

        let models = parse_curated(&body);

        assert!(models.is_empty());
    }

    #[test]
    fn parse_curated_accepts_prefixed_registry_sha_and_revision_alias() {
        let mut model = registry_model("qwen3.6-27b-q4km", 2);
        let object = model.as_object_mut().unwrap();
        object.remove("revision");
        object.insert(
            "hfRevision".to_owned(),
            Value::String("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF".to_owned()),
        );
        object.insert(
            "sha256".to_owned(),
            Value::String(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
            ),
        );
        let body = json!({ "models": [model] });

        let models = parse_curated(&body);

        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].revision,
            "ffffffffffffffffffffffffffffffffffffffff"
        );
        assert_eq!(
            models[0].sha256,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn parse_curated_accepts_new_curated_model_when_registry_supplies_pins() {
        let mut model = registry_model("qwen3.5-9b-q4km", 2);
        model
            .as_object_mut()
            .unwrap()
            .insert("filename".to_owned(), Value::String("new.gguf".to_owned()));
        let body = json!({ "models": [model] });

        let models = parse_curated(&body);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3.5-9b-q4km");
        assert_eq!(models[0].filename, "new.gguf");
    }
}

//! Hugging Face cache scan + download for Rayline local models.
//!
//! The on-disk layout (models--org--repo/{refs,blobs,snapshots/{commit}}) is
//! byte-identical to what huggingface_hub creates, so Rayline can reuse an
//! existing Hugging Face cache.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A GGUF model found in the HF cache.
#[derive(Debug, Clone)]
pub struct HfCacheGguf {
    pub repo: String,
    pub filename: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// Progress event emitted by `download_to_hf_cache`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub stage: String,
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub percent: f64,
    pub filename: String,
}

/// Callback type for progress reporting. `None` is a valid no-op.
pub type ProgressCallback<'a> = Option<&'a dyn Fn(DownloadProgress)>;

#[derive(Debug, Deserialize)]
struct HfModelInfo {
    sha: String,
}

// ---------------------------------------------------------------------------
// Retry classification (inlined from download_utils.rs)
// ---------------------------------------------------------------------------

const MAX_DOWNLOAD_ATTEMPTS: u32 = 4;

enum DownloadAttemptError {
    Cancelled,
    Fatal(String),
    Retryable(String),
}

fn is_retryable_io_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::TimedOut
            | ErrorKind::BrokenPipe
            | ErrorKind::Interrupted
            | ErrorKind::Other
    )
}

fn is_retryable_reqwest_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_body() || e.is_request()
}

// ---------------------------------------------------------------------------
// Cache directory resolution
// ---------------------------------------------------------------------------

/// Resolve the HF cache directory. Priority: `HF_HUB_CACHE`, `HF_HOME`/hub,
/// then `~/.cache/huggingface/hub` (or `%LOCALAPPDATA%\huggingface\hub` on
/// Windows).
pub fn hf_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HF_HUB_CACHE") {
        return PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HF_HOME") {
        return PathBuf::from(home).join("hub");
    }
    let cache =
        dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().expect("no home dir").join(".cache"));
    cache.join("huggingface").join("hub")
}

/// "org/repo" → "models--org--repo".
pub fn repo_to_folder_name(repo: &str) -> String {
    format!("models--{}", repo.replace('/', "--"))
}

/// "models--org--repo" → Some("org/repo"). Resolves hyphenated org/name names
/// by preferring the split where neither side ends/starts with `-`.
pub fn folder_name_to_repo(folder: &str) -> Option<String> {
    let stripped = folder.strip_prefix("models--")?;
    if stripped.is_empty() {
        return None;
    }

    let mut candidates: Vec<(usize, &str)> = Vec::new();
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1] == b'-' {
            let org = &stripped[..i];
            let name = &stripped[(i + 2)..];
            if !org.is_empty() && !name.is_empty() {
                candidates.push((i, name));
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        let (pos, _) = candidates[0];
        return Some(format!("{}/{}", &stripped[..pos], &stripped[(pos + 2)..]));
    }

    let cache = hf_cache_dir();
    let folder_path = cache.join(folder);
    for &(pos, _) in &candidates {
        let org = &stripped[..pos];
        let name = &stripped[(pos + 2)..];
        let refs_path = folder_path.join("refs");
        let snapshots_path = folder_path.join("snapshots");
        if (refs_path.is_dir() || snapshots_path.is_dir())
            && !org.ends_with('-')
            && !name.starts_with('-')
        {
            return Some(format!("{org}/{name}"));
        }
    }

    for &(pos, _) in candidates.iter().rev() {
        let org = &stripped[..pos];
        let name = &stripped[(pos + 2)..];
        if !org.ends_with('-') && !name.starts_with('-') {
            return Some(format!("{org}/{name}"));
        }
    }

    let (pos, _) = candidates[0];
    Some(format!("{}/{}", &stripped[..pos], &stripped[(pos + 2)..]))
}

// ---------------------------------------------------------------------------
// Cache scanning
// ---------------------------------------------------------------------------

/// Scan the HF cache for all GGUF files. Returns models across all cached repos.
pub fn scan_hf_cache_gguf() -> Vec<HfCacheGguf> {
    let cache = hf_cache_dir();
    let mut results = Vec::new();

    if !cache.is_dir() {
        return results;
    }

    let entries = match fs::read_dir(&cache) {
        Ok(e) => e,
        Err(e) => {
            warn!("[HfCache] Failed to read cache dir {:?}: {}", cache, e);
            return results;
        }
    };

    for entry in entries.flatten() {
        let folder_name = entry.file_name().to_string_lossy().to_string();
        if !folder_name.starts_with("models--") {
            continue;
        }

        let repo = match folder_name_to_repo(&folder_name) {
            Some(r) => r,
            None => continue,
        };

        let snapshots_dir = entry.path().join("snapshots");
        if !snapshots_dir.is_dir() {
            continue;
        }

        let refs_main = entry.path().join("refs").join("main");
        let preferred_commit = fs::read_to_string(&refs_main)
            .ok()
            .map(|s| s.trim().to_string());

        if let Some(ref commit) = preferred_commit {
            let commit_dir = snapshots_dir.join(commit);
            if commit_dir.is_dir() {
                collect_gguf_from_snapshot(&commit_dir, &commit_dir, &repo, &mut results);
                continue;
            }
        }

        let commit_dirs = match fs::read_dir(&snapshots_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let mut seen_filenames: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let pre_len = results.len();

        for commit_entry in commit_dirs.flatten() {
            if !commit_entry.path().is_dir() {
                continue;
            }
            collect_gguf_from_snapshot(
                &commit_entry.path(),
                &commit_entry.path(),
                &repo,
                &mut results,
            );
        }

        let mut i = pre_len;
        while i < results.len() {
            if !seen_filenames.insert(results[i].filename.clone()) {
                results.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    results
}

fn collect_gguf_from_snapshot(
    dir: &Path,
    snapshot_root: &Path,
    repo: &str,
    results: &mut Vec<HfCacheGguf>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            collect_gguf_from_snapshot(&path, snapshot_root, repo, results);
            continue;
        }

        let filename_str = path.file_name().unwrap_or_default().to_string_lossy();

        if filename_str.ends_with(".download")
            || !filename_str.ends_with(".gguf")
            || filename_str.contains("mmproj")
            || is_non_first_shard(&filename_str)
        {
            continue;
        }

        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let rel = path
            .strip_prefix(snapshot_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        results.push(HfCacheGguf {
            repo: repo.to_string(),
            filename: rel,
            path: path.clone(),
            size_bytes: size,
        });
    }
}

fn is_non_first_shard(filename: &str) -> bool {
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    if let Some(of_idx) = stem.rfind("-of-") {
        let total = &stem[(of_idx + 4)..];
        if total.len() == 5 && total.chars().all(|c| c.is_ascii_digit()) {
            let before_of = &stem[..of_idx];
            if let Some(shard_sep) = before_of.rfind('-') {
                let shard_idx = &before_of[(shard_sep + 1)..];
                if shard_idx.len() == 5 && shard_idx.chars().all(|c| c.is_ascii_digit()) {
                    return shard_idx != "00001";
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// HF API
// ---------------------------------------------------------------------------

/// Latest commit hash for a repo via the HF API.
pub fn hf_api_get_commit(repo: &str) -> Result<String, String> {
    let url = format!("https://huggingface.co/api/models/{repo}");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let response = client
        .get(&url)
        .header("User-Agent", "rayline")
        .send()
        .map_err(|e| format!("HF API request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("HF API returned status {}", response.status()));
    }

    let body = response
        .text()
        .map_err(|e| format!("Failed to read HF API response: {e}"))?;
    let info: HfModelInfo =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse HF API response: {e}"))?;

    Ok(info.sha)
}

// ---------------------------------------------------------------------------
// SHA256
// ---------------------------------------------------------------------------

fn compute_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("Failed to open file for hashing: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Read error during hashing: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Download to HF cache
// ---------------------------------------------------------------------------

/// Download a file into the HF cache structure. Returns the snapshot symlink
/// path. Steps mirror `huggingface_hub`: write to `blobs/.download-*`,
/// SHA256-rename to `blobs/sha256-{hash}`, write `refs/main`, symlink
/// `snapshots/{commit}/{filename}` → blob.
#[allow(clippy::too_many_arguments)]
pub fn download_to_hf_cache(
    repo: &str,
    filename: &str,
    commit: &str,
    on_progress: ProgressCallback<'_>,
    stage: &str,
    cancel: Option<&AtomicBool>,
    bytes_offset: u64,
    total_override: u64,
    progress_filename: Option<&str>,
) -> Result<PathBuf, String> {
    let cache = hf_cache_dir();
    let repo_dir = cache.join(repo_to_folder_name(repo));
    let refs_dir = repo_dir.join("refs");
    let blobs_dir = repo_dir.join("blobs");
    let snapshot_dir = repo_dir.join("snapshots").join(commit);

    fs::create_dir_all(&refs_dir).map_err(|e| format!("Failed to create refs dir: {e}"))?;
    fs::create_dir_all(&blobs_dir).map_err(|e| format!("Failed to create blobs dir: {e}"))?;
    fs::create_dir_all(&snapshot_dir).map_err(|e| format!("Failed to create snapshot dir: {e}"))?;

    let snapshot_file = snapshot_dir.join(filename);
    if let Some(parent) = snapshot_file.parent() {
        if parent != snapshot_dir {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create snapshot subdir: {e}"))?;
        }
    }

    let safe_name = filename.replace('/', "_");
    let tmp_path = blobs_dir.join(format!(".download-{safe_name}.tmp"));
    // Sidecar that records which URL produced the current `.tmp`. Lets us
    // resume across process restarts: if the URL matches we keep the
    // partial file and ask the server for a Range; if it differs (or is
    // missing) we wipe the stale bytes before re-downloading.
    let tmp_url_path = blobs_dir.join(format!(".download-{safe_name}.tmp.url"));
    let download_url = format!("https://huggingface.co/{repo}/resolve/{commit}/{filename}");

    let prior_url = fs::read_to_string(&tmp_url_path).ok();
    let resumable = prior_url.as_deref().map(str::trim) == Some(download_url.as_str());
    if !resumable {
        if tmp_path.exists() {
            info!(
                "[HfCache] Discarding stale partial download at {:?} (URL changed)",
                tmp_path
            );
            let _ = fs::remove_file(&tmp_path);
        }
        if let Err(e) = fs::write(&tmp_url_path, &download_url) {
            warn!(
                "[HfCache] Failed to write tmp URL sidecar {:?}: {}",
                tmp_url_path, e
            );
        }
    } else if let Ok(meta) = fs::metadata(&tmp_path) {
        info!(
            "[HfCache] Resuming previous download at {:?} ({} bytes already on disk)",
            tmp_path,
            meta.len()
        );
    }

    info!(
        "[HfCache] Downloading {} -> temp {:?}",
        download_url, tmp_path
    );

    download_file_to_path(
        &download_url,
        &tmp_path,
        on_progress,
        stage,
        cancel,
        bytes_offset,
        total_override,
        progress_filename,
    )?;

    info!("[HfCache] Computing SHA256 for {:?}", tmp_path);
    let sha256 = compute_sha256(&tmp_path)?;
    let blob_name = format!("sha256-{sha256}");
    let blob_path = blobs_dir.join(&blob_name);

    if blob_path.exists() {
        let _ = fs::remove_file(&tmp_path);
        info!("[HfCache] Blob already exists: {:?}", blob_path);
    } else {
        fs::rename(&tmp_path, &blob_path).map_err(|e| format!("Failed to rename blob: {e}"))?;
        info!("[HfCache] Blob created: {:?}", blob_path);
    }

    let _ = fs::remove_file(&tmp_url_path);

    let refs_main = refs_dir.join("main");
    let mut f =
        fs::File::create(&refs_main).map_err(|e| format!("Failed to create refs/main: {e}"))?;
    f.write_all(commit.as_bytes())
        .map_err(|e| format!("Failed to write refs/main: {e}"))?;

    let depth = filename.matches('/').count();
    let mut rel_prefix = String::from("../../");
    for _ in 0..depth {
        rel_prefix.push_str("../");
    }
    let symlink_target = format!("{rel_prefix}blobs/{blob_name}");

    let _ = fs::remove_file(&snapshot_file);
    create_symlink_or_copy(&symlink_target, &snapshot_file, &blob_path)?;

    info!(
        "[HfCache] Cache entry created: {:?} -> {}",
        snapshot_file, symlink_target
    );

    Ok(snapshot_file)
}

/// Create the HF cache entry for an already-downloaded file (for migration
/// of legacy non-HF-cache files into the HF cache layout).
pub fn import_to_hf_cache(
    existing_path: &Path,
    repo: &str,
    filename: &str,
    commit: &str,
) -> Result<PathBuf, String> {
    let cache = hf_cache_dir();
    let repo_dir = cache.join(repo_to_folder_name(repo));
    let refs_dir = repo_dir.join("refs");
    let blobs_dir = repo_dir.join("blobs");
    let snapshot_dir = repo_dir.join("snapshots").join(commit);

    fs::create_dir_all(&refs_dir).map_err(|e| format!("Failed to create refs dir: {e}"))?;
    fs::create_dir_all(&blobs_dir).map_err(|e| format!("Failed to create blobs dir: {e}"))?;
    fs::create_dir_all(&snapshot_dir).map_err(|e| format!("Failed to create snapshot dir: {e}"))?;

    let snapshot_file = snapshot_dir.join(filename);
    if let Some(parent) = snapshot_file.parent() {
        if parent != snapshot_dir {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create snapshot subdir: {e}"))?;
        }
    }

    let sha256 = compute_sha256(existing_path)?;
    let blob_name = format!("sha256-{sha256}");
    let blob_path = blobs_dir.join(&blob_name);

    if blob_path.exists() {
        let _ = fs::remove_file(existing_path);
    } else {
        fs::rename(existing_path, &blob_path).or_else(|_| {
            fs::copy(existing_path, &blob_path)
                .map_err(|e| format!("Failed to copy file to blob: {e}"))?;
            let _ = fs::remove_file(existing_path);
            Ok::<(), String>(())
        })?;
    }

    let refs_main = refs_dir.join("main");
    let mut f =
        fs::File::create(&refs_main).map_err(|e| format!("Failed to create refs/main: {e}"))?;
    f.write_all(commit.as_bytes())
        .map_err(|e| format!("Failed to write refs/main: {e}"))?;

    let depth = filename.matches('/').count();
    let mut rel_prefix = String::from("../../");
    for _ in 0..depth {
        rel_prefix.push_str("../");
    }
    let symlink_target = format!("{rel_prefix}blobs/{blob_name}");
    let _ = fs::remove_file(&snapshot_file);
    create_symlink_or_copy(&symlink_target, &snapshot_file, &blob_path)?;

    info!(
        "[HfCache] Imported {:?} -> {:?}",
        existing_path, snapshot_file
    );

    Ok(snapshot_file)
}

// ---------------------------------------------------------------------------
// Deletion helpers
// ---------------------------------------------------------------------------

/// Delete a model from the HF cache. Removes the snapshot symlink and the
/// blob if no other snapshot references it.
pub fn delete_from_hf_cache(model_path: &Path) -> Result<(), String> {
    let blob_path = if model_path.is_symlink() {
        fs::read_link(model_path)
            .ok()
            .and_then(|target| model_path.parent().map(|p| p.join(target)))
            .and_then(|p| fs::canonicalize(p).ok())
    } else {
        find_matching_blob(model_path)
    };

    if model_path.exists() || model_path.is_symlink() {
        fs::remove_file(model_path).map_err(|e| format!("Failed to remove snapshot entry: {e}"))?;
        info!("[HfCache] Removed snapshot entry: {:?}", model_path);
    }

    if let Some(parent) = model_path.parent() {
        let _ = fs::remove_dir(parent);
        if let Some(grandparent) = parent.parent() {
            let _ = fs::remove_dir(grandparent);
        }
    }

    if let Some(ref bp) = blob_path {
        if bp.exists() && !is_blob_referenced_elsewhere(bp)? {
            fs::remove_file(bp).map_err(|e| format!("Failed to remove blob: {e}"))?;
            info!("[HfCache] Removed unreferenced blob: {:?}", bp);
        }
    }

    Ok(())
}

/// Delete a model and all its sibling shard files from the HF cache.
pub fn delete_model_and_shards_from_hf_cache(model_path: &Path) -> Result<(), String> {
    let snapshot_dir = model_path
        .parent()
        .ok_or_else(|| "Cannot determine snapshot directory".to_string())?;

    let model_filename = model_path.file_name().unwrap_or_default().to_string_lossy();

    if let Some(shard_prefix) = extract_shard_prefix(&model_filename) {
        if let Ok(entries) = fs::read_dir(snapshot_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&shard_prefix)
                    && name.ends_with(".gguf")
                    && name != *model_filename
                {
                    let shard_path = entry.path();
                    if let Err(e) = delete_from_hf_cache(&shard_path) {
                        warn!("[HfCache] Failed to delete shard {:?}: {}", shard_path, e);
                    }
                }
            }
        }
    }

    delete_from_hf_cache(model_path)
}

fn extract_shard_prefix(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".gguf")?;
    let of_idx = stem.rfind("-of-")?;
    let total = &stem[(of_idx + 4)..];
    if total.len() != 5 || !total.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let before_of = &stem[..of_idx];
    let shard_sep = before_of.rfind('-')?;
    let shard_num = &before_of[(shard_sep + 1)..];
    if shard_num.len() != 5 || !shard_num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(before_of[..(shard_sep + 1)].to_string())
}

fn is_blob_referenced_elsewhere(blob_path: &Path) -> Result<bool, String> {
    let repo_dir = blob_path.parent().and_then(|p| p.parent());

    let repo_dir = match repo_dir {
        Some(d) => d,
        None => return Ok(false),
    };

    let snapshots_dir = repo_dir.join("snapshots");
    if !snapshots_dir.is_dir() {
        return Ok(false);
    }

    let blob_canon = fs::canonicalize(blob_path).unwrap_or_else(|_| blob_path.to_path_buf());

    fn check_dir(dir: &Path, target_blob: &Path) -> Result<bool, String> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if check_dir(&path, target_blob)? {
                    return Ok(true);
                }
            } else if path.is_symlink() {
                if let Ok(link_target) = fs::read_link(&path) {
                    let resolved = path.parent().unwrap_or(&path).join(link_target);
                    if let Ok(canon) = fs::canonicalize(resolved) {
                        if canon == *target_blob {
                            return Ok(true);
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    check_dir(&snapshots_dir, &blob_canon)
}

fn find_matching_blob(snapshot_path: &Path) -> Option<PathBuf> {
    let snapshot_dir = snapshot_path.parent()?;
    let snapshots_root = snapshot_dir.parent()?;
    let repo_dir = snapshots_root.parent()?;
    let blobs_dir = repo_dir.join("blobs");
    if !blobs_dir.is_dir() {
        return None;
    }
    let hash = compute_sha256(snapshot_path).ok()?;
    let expected_blob = blobs_dir.join(format!("sha256-{hash}"));
    expected_blob.exists().then_some(expected_blob)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn create_symlink_or_copy(
    symlink_target: &str,
    symlink_path: &Path,
    _blob_path: &Path,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(symlink_target, symlink_path)
            .map_err(|e| format!("Failed to create symlink: {e}"))?;
    }

    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(symlink_target, symlink_path) {
            Ok(()) => {}
            Err(e) => {
                warn!(
                    "[HfCache] Symlink creation failed ({}), falling back to copy",
                    e
                );
                fs::copy(_blob_path, symlink_path)
                    .map_err(|e| format!("Failed to copy blob to snapshot: {}", e))?;
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        fs::copy(_blob_path, symlink_path)
            .map_err(|e| format!("Failed to copy blob to snapshot: {}", e))?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn download_file_to_path(
    url: &str,
    dest_path: &Path,
    on_progress: ProgressCallback<'_>,
    stage: &str,
    cancel: Option<&AtomicBool>,
    bytes_offset: u64,
    total_override: u64,
    report_filename: Option<&str>,
) -> Result<u64, String> {
    info!("[HfCache] Downloading {} -> {:?}", url, dest_path);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    // No unconditional pre-delete here: the caller (download_to_hf_cache)
    // already validates that any pre-existing tmp file belongs to this same
    // URL via the .tmp.url sidecar. Keeping the partial bytes around lets
    // attempt_download resume mid-file across process restarts.

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match attempt_download(
            url,
            dest_path,
            &client,
            on_progress,
            stage,
            cancel,
            bytes_offset,
            total_override,
            report_filename,
        ) {
            Ok(bytes) => return Ok(bytes),
            Err(DownloadAttemptError::Cancelled) => {
                let _ = fs::remove_file(dest_path);
                return Err("Download cancelled".to_string());
            }
            Err(DownloadAttemptError::Fatal(msg)) => {
                let _ = fs::remove_file(dest_path);
                return Err(msg);
            }
            Err(DownloadAttemptError::Retryable(msg)) => {
                if attempt >= MAX_DOWNLOAD_ATTEMPTS {
                    let _ = fs::remove_file(dest_path);
                    return Err(format!("{msg} (failed after {attempt} attempts)"));
                }
                let backoff = Duration::from_secs(1u64 << (attempt - 1).min(4));
                warn!(
                    "[HfCache] Transient error on attempt {}/{}: {}. Retrying in {:?}.",
                    attempt, MAX_DOWNLOAD_ATTEMPTS, msg, backoff
                );
                let start = Instant::now();
                while start.elapsed() < backoff {
                    if let Some(flag) = cancel {
                        if flag.load(Ordering::Relaxed) {
                            let _ = fs::remove_file(dest_path);
                            return Err("Download cancelled".to_string());
                        }
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt_download(
    url: &str,
    dest_path: &Path,
    client: &reqwest::blocking::Client,
    on_progress: ProgressCallback<'_>,
    stage: &str,
    cancel: Option<&AtomicBool>,
    bytes_offset: u64,
    total_override: u64,
    report_filename: Option<&str>,
) -> Result<u64, DownloadAttemptError> {
    // Honor any partial file on disk — either retained from a previous
    // process invocation (via the .tmp.url sidecar guarding the call site)
    // or left by a previous retry within this same invocation.
    let existing_bytes = fs::metadata(dest_path).map(|m| m.len()).unwrap_or(0);

    let mut request = client.get(url).header("User-Agent", "rayline");
    if existing_bytes > 0 {
        request = request.header("Range", format!("bytes={existing_bytes}-"));
    }

    let response = match request.send() {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Download request failed: {e}");
            return if is_retryable_reqwest_error(&e) {
                Err(DownloadAttemptError::Retryable(msg))
            } else {
                Err(DownloadAttemptError::Fatal(msg))
            };
        }
    };

    let status = response.status();
    if !status.is_success() {
        // 416 Range Not Satisfiable means the partial file on disk is already
        // at (or past) EOF — common when a previous run got killed between
        // the download finishing and the blob being promoted. Wipe the
        // local partial and retry from scratch instead of failing fatally.
        if status.as_u16() == 416 && existing_bytes > 0 {
            let _ = fs::remove_file(dest_path);
            return Err(DownloadAttemptError::Retryable(format!(
                "Server reported partial file out of range ({existing_bytes} bytes); restarting from zero"
            )));
        }
        let msg = format!("Download failed with status {status}");
        return if status.is_server_error() {
            Err(DownloadAttemptError::Retryable(msg))
        } else {
            Err(DownloadAttemptError::Fatal(msg))
        };
    }

    if existing_bytes > 0 && status.as_u16() != 206 {
        let _ = fs::remove_file(dest_path);
        return Err(DownloadAttemptError::Retryable(format!(
            "Server ignored resume request for partial file ({existing_bytes} bytes, status {status}); restarting from zero"
        )));
    }

    let resumed = existing_bytes > 0 && status.as_u16() == 206;
    let (mut file, mut bytes_downloaded, total_bytes) = if resumed {
        let file = fs::OpenOptions::new()
            .append(true)
            .open(dest_path)
            .map_err(|e| DownloadAttemptError::Fatal(format!("Failed to open tmp file: {e}")))?;
        let content_len = response.content_length().unwrap_or(0);
        let total = if content_len > 0 {
            existing_bytes + content_len
        } else {
            0
        };
        info!(
            "[HfCache] Resuming from {} bytes (total {:?})",
            existing_bytes, total
        );
        (file, existing_bytes, total)
    } else {
        let file = fs::File::create(dest_path)
            .map_err(|e| DownloadAttemptError::Fatal(format!("Failed to create file: {e}")))?;
        (file, 0u64, response.content_length().unwrap_or(0))
    };

    let mut reader = response;
    let mut buf = [0u8; 65536];
    let mut last_emit = Instant::now();

    loop {
        if let Some(flag) = cancel {
            if flag.load(Ordering::Relaxed) {
                return Err(DownloadAttemptError::Cancelled);
            }
        }

        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                drop(file);
                let msg = format!("Read error during download: {e}");
                return if is_retryable_io_error(&e) {
                    Err(DownloadAttemptError::Retryable(msg))
                } else {
                    Err(DownloadAttemptError::Fatal(msg))
                };
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = file.write_all(&buf[..n]) {
            drop(file);
            return Err(DownloadAttemptError::Fatal(format!(
                "Write error during download: {e}"
            )));
        }
        bytes_downloaded += n as u64;

        if last_emit.elapsed() > Duration::from_millis(200) {
            if let Some(cb) = on_progress {
                let (effective_downloaded, effective_total) = if total_override > 0 {
                    (bytes_offset + bytes_downloaded, total_override)
                } else {
                    (bytes_downloaded, total_bytes)
                };
                let percent = if effective_total > 0 {
                    (effective_downloaded as f64 / effective_total as f64) * 100.0
                } else {
                    0.0
                };
                let display_filename =
                    report_filename.map(|s| s.to_string()).unwrap_or_else(|| {
                        dest_path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default()
                    });
                cb(DownloadProgress {
                    stage: stage.to_string(),
                    bytes_downloaded: effective_downloaded,
                    total_bytes: effective_total,
                    percent,
                    filename: display_filename,
                });
            }
            last_emit = Instant::now();
        }
    }

    drop(file);

    if total_override == 0 {
        if let Some(cb) = on_progress {
            cb(DownloadProgress {
                stage: stage.to_string(),
                bytes_downloaded,
                total_bytes: if total_bytes > 0 {
                    total_bytes
                } else {
                    bytes_downloaded
                },
                percent: 100.0,
                filename: report_filename.map(|s| s.to_string()).unwrap_or_else(|| {
                    dest_path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default()
                }),
            });
        }
    }

    info!(
        "[HfCache] Download complete: {} bytes -> {:?}",
        bytes_downloaded, dest_path
    );
    Ok(bytes_downloaded)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_repo_to_folder_name() {
        assert_eq!(
            repo_to_folder_name("unsloth/Qwen3.5-2B-GGUF"),
            "models--unsloth--Qwen3.5-2B-GGUF"
        );
    }

    #[test]
    fn test_folder_name_to_repo() {
        assert_eq!(
            folder_name_to_repo("models--unsloth--Qwen3.5-2B-GGUF"),
            Some("unsloth/Qwen3.5-2B-GGUF".to_string())
        );
        assert_eq!(
            folder_name_to_repo("models--meta-llama--Llama-3-8B"),
            Some("meta-llama/Llama-3-8B".to_string())
        );
        assert_eq!(
            folder_name_to_repo("models--google-deepmind--gemma-2-9b-it-GGUF"),
            Some("google-deepmind/gemma-2-9b-it-GGUF".to_string())
        );
        assert_eq!(folder_name_to_repo("not-a-model"), None);
        assert_eq!(folder_name_to_repo("models--singlepart"), None);
    }

    #[test]
    fn test_is_non_first_shard() {
        assert!(!is_non_first_shard("model.gguf"));
        assert!(!is_non_first_shard("model-00001-of-00005.gguf"));
        assert!(is_non_first_shard("model-00002-of-00005.gguf"));
        assert!(is_non_first_shard("model-00003-of-00005.gguf"));
    }

    #[test]
    fn test_extract_shard_prefix() {
        assert_eq!(
            extract_shard_prefix("Model-Q4-00001-of-00005.gguf"),
            Some("Model-Q4-".to_string())
        );
        assert_eq!(
            extract_shard_prefix("Model-Q4-00003-of-00005.gguf"),
            Some("Model-Q4-".to_string())
        );
        assert_eq!(extract_shard_prefix("model.gguf"), None);
        assert_eq!(extract_shard_prefix("model-Q8_0.gguf"), None);
    }

    #[test]
    fn test_resumed_download_restarts_from_zero_when_range_is_ignored() {
        let test_dir = unique_test_dir("range-ignored");
        fs::create_dir_all(&test_dir).unwrap();
        let dest_path = test_dir.join("model.gguf.tmp");
        fs::write(&dest_path, b"hello").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/model.gguf", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut handled = 0;
            while handled < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_http_request(&mut stream);
                        server_requests.lock().unwrap().push(request);
                        let body: &[u8] = if handled == 0 {
                            b"world"
                        } else {
                            b"hello world"
                        };
                        write_http_response(&mut stream, body);
                        handled += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("test server accept failed: {e}"),
                }
            }
        });

        let downloaded = download_file_to_path(
            &url,
            &dest_path,
            None,
            "download",
            None,
            0,
            0,
            Some("model.gguf"),
        )
        .unwrap();

        server.join().unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].to_ascii_lowercase().contains("range: bytes=5-"));
        assert!(!recorded[1].to_ascii_lowercase().contains("range:"));
        assert_eq!(downloaded, 11);
        assert_eq!(fs::read(&dest_path).unwrap(), b"hello world");

        let _ = fs::remove_dir_all(test_dir);
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rayline-hf-{}-{}-{}",
            name,
            std::process::id(),
            nanos
        ))
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = String::new();
        let mut reader = std::io::BufReader::new(stream);
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line).unwrap();
            if bytes == 0 || line == "\r\n" {
                break;
            }
            request.push_str(&line);
        }
        request
    }

    fn write_http_response(stream: &mut std::net::TcpStream, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }
}

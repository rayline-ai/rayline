//! llama.cpp lifecycle: resolve archive URL, download/extract the
//! `llama-server` binary, spawn it, probe health, shut it down.
//!
//! Keeps only the server lifecycle Rayline needs: model archive resolution,
//! binary extraction, process management, and health checks.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub type LogObserver = Arc<dyn Fn(&str) + Send + Sync + 'static>;

// ---------------------------------------------------------------------------
// Hardware detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub total_ram_bytes: u64,
    pub gpu_type: String,
    pub gpu_model: String,
    pub gpu_vram_bytes: u64,
    pub os: String,
    pub arch: String,
}

pub fn detect_hardware() -> HardwareInfo {
    let os = env::consts::OS.to_string();
    let arch = env::consts::ARCH.to_string();
    let total_ram_bytes = detect_total_ram();
    let (gpu_type, gpu_model, gpu_vram_bytes) = detect_gpu();
    HardwareInfo {
        total_ram_bytes,
        gpu_type,
        gpu_model,
        gpu_vram_bytes,
        os,
        arch,
    }
}

#[cfg(target_os = "macos")]
fn detect_total_ram() -> u64 {
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn detect_total_ram() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .map(|c| parse_meminfo_total(&c))
        .unwrap_or(0)
}

#[cfg(target_os = "windows")]
fn detect_total_ram() -> u64 {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut mem_info = MaybeUninit::<MEMORYSTATUSEX>::zeroed();
        let mem_ptr = mem_info.as_mut_ptr();
        (*mem_ptr).dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(mem_ptr) != 0 {
            (*mem_ptr).ullTotalPhys
        } else {
            0
        }
    }
}

pub fn parse_meminfo_total(contents: &str) -> u64 {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(kb_str) = rest.trim().strip_suffix("kB") {
                if let Ok(kb) = kb_str.trim().parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

/// Returns (gpu_type, gpu_model, gpu_vram_bytes). On Apple Silicon VRAM is
/// reported as 0 because memory is unified with system RAM.
#[cfg(target_os = "macos")]
fn detect_gpu() -> (String, String, u64) {
    if let Ok(output) = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
    {
        let cpu = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if cpu.contains("Apple") {
            return ("apple-silicon".to_string(), cpu, 0);
        }
    }
    ("none".to_string(), String::new(), 0)
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> (String, String, u64) {
    if let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            let line = s.trim();
            if !line.is_empty() {
                let parts: Vec<&str> = line.splitn(2, ',').collect();
                let model = parts.first().unwrap_or(&"").trim().to_string();
                let vram_mib: u64 = parts
                    .get(1)
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                return ("nvidia".to_string(), model, vram_mib * 1024 * 1024);
            }
        }
    }
    ("none".to_string(), String::new(), 0)
}

#[cfg(target_os = "windows")]
fn detect_gpu() -> (String, String, u64) {
    if let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            let line = s.trim();
            if !line.is_empty() {
                let parts: Vec<&str> = line.splitn(2, ',').collect();
                let model = parts.first().unwrap_or(&"").trim().to_string();
                let vram_mib: u64 = parts
                    .get(1)
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                return ("nvidia".to_string(), model, vram_mib * 1024 * 1024);
            }
        }
    }
    ("none".to_string(), String::new(), 0)
}

// ---------------------------------------------------------------------------
// Release URL resolution
// ---------------------------------------------------------------------------

const LLAMA_RELEASE_BASE: &str = "https://github.com/ggml-org/llama.cpp/releases/download";
const UNVERIFIED_RUNTIME_DOWNLOAD_ENV: &str = "RAYLINE_LLAMA_ALLOW_UNVERIFIED_RUNTIME_DOWNLOAD";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRuntimeArchive {
    pub url: String,
    pub filename: String,
    pub expected_sha256: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeArchiveManifestEntry {
    tag: &'static str,
    filename: &'static str,
    sha256: &'static str,
}

// Digests are copied from the GitHub release asset `digest` metadata.
const RUNTIME_ARCHIVE_MANIFEST: &[RuntimeArchiveManifestEntry] = &[
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-macos-arm64.tar.gz",
        sha256: "e88f05f82c8c0c0f5a861ff7822f096ad6641128e6f64c666eee743f46730db6",
    },
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-macos-x64.tar.gz",
        sha256: "31151226ac563764df3456b615c261d10a92f09e99be48a64d39985f15e7a15b",
    },
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-ubuntu-x64.tar.gz",
        sha256: "be111dd28e6228fc4cb6a6ec41f03a67947ab61f315a3d22d0e68ac7372a58ab",
    },
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-win-cuda-12.4-x64.zip",
        sha256: "d48de89c397ceb7e8325786808a2edff443e29780ce93a8404066286cdac6b63",
    },
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-win-vulkan-x64.zip",
        sha256: "af6b1b94377b9f78dbb2285b878fb696d36766391499d65e055ecd622b69018a",
    },
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-win-cpu-x64.zip",
        sha256: "23c0e329e2228f7cbcc83884f42c7787f1a3133e5548ea99e89d60202e1fd89c",
    },
    RuntimeArchiveManifestEntry {
        tag: "b9585",
        filename: "llama-b9585-bin-win-cpu-arm64.zip",
        sha256: "9dd7cde8fdc2a5c932f63e4392c1c10ce6f65d39a70a781d9a3978e68ca9c215",
    },
];

/// Resolve the llama-server release archive URL for the given tag and platform.
pub fn resolve_download_url(
    tag: &str,
    os: &str,
    arch: &str,
    gpu_type: &str,
) -> Result<String, String> {
    Ok(resolve_runtime_archive(tag, os, arch, gpu_type)?.url)
}

pub fn resolve_archive_filename(
    tag: &str,
    os: &str,
    arch: &str,
    gpu_type: &str,
) -> Result<String, String> {
    Ok(resolve_runtime_archive(tag, os, arch, gpu_type)?.filename)
}

pub fn resolve_runtime_archive(
    tag: &str,
    os: &str,
    arch: &str,
    gpu_type: &str,
) -> Result<ResolvedRuntimeArchive, String> {
    resolve_runtime_archive_checked(tag, os, arch, gpu_type, allow_unverified_runtime_download())
}

fn resolve_runtime_archive_checked(
    tag: &str,
    os: &str,
    arch: &str,
    gpu_type: &str,
    allow_unverified: bool,
) -> Result<ResolvedRuntimeArchive, String> {
    let filename = archive_filename_for_platform(tag, os, arch, gpu_type)?;
    let expected_sha256 = expected_archive_sha256(tag, &filename);
    if expected_sha256.is_none() && !allow_unverified {
        return Err(format!(
            "No committed SHA256 for llama.cpp runtime archive {filename} (tag {tag}); refusing to download. Add it to the runtime archive manifest or set {UNVERIFIED_RUNTIME_DOWNLOAD_ENV}=1 for local development only."
        ));
    }

    Ok(ResolvedRuntimeArchive {
        url: format!("{LLAMA_RELEASE_BASE}/{tag}/{filename}"),
        filename,
        expected_sha256,
    })
}

fn archive_filename_for_platform(
    tag: &str,
    os: &str,
    arch: &str,
    gpu_type: &str,
) -> Result<String, String> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(format!("llama-{tag}-bin-macos-arm64.tar.gz")),
        ("macos", "x86_64") => Ok(format!("llama-{tag}-bin-macos-x64.tar.gz")),
        ("linux", "x86_64") => Ok(format!("llama-{tag}-bin-ubuntu-x64.tar.gz")),
        ("windows", "x86_64") => {
            let variant = match gpu_type {
                "nvidia" => "win-cuda-12.4-x64",
                "amd" | "amd-apu" => "win-vulkan-x64",
                _ => "win-cpu-x64",
            };
            Ok(format!("llama-{tag}-bin-{variant}.zip"))
        }
        ("windows", "aarch64") => Ok(format!("llama-{tag}-bin-win-cpu-arm64.zip")),
        _ => Err(format!("Unsupported platform: os={os}, arch={arch}")),
    }
}

fn expected_archive_sha256(tag: &str, filename: &str) -> Option<&'static str> {
    RUNTIME_ARCHIVE_MANIFEST
        .iter()
        .find(|entry| entry.tag == tag && entry.filename == filename)
        .map(|entry| entry.sha256)
}

fn allow_unverified_runtime_download() -> bool {
    env::var(UNVERIFIED_RUNTIME_DOWNLOAD_ENV)
        .ok()
        .map(|value| {
            matches!(
                value.trim(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Download + extract
// ---------------------------------------------------------------------------

/// Simple one-shot blocking download with up to 4 retries on transient errors.
/// No resume — the release tarball is small enough (~100MB) that a fresh
/// re-download on retry is cheaper than the resume bookkeeping.
pub fn download_file(url: &str, dest_path: &Path) -> Result<(), String> {
    let expected_sha256 = expected_archive_sha256_for_url(url)?;
    info!("[rayline-llama] Downloading {} -> {:?}", url, dest_path);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    const MAX_ATTEMPTS: u32 = 4;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match attempt_download(&client, url, dest_path) {
            Ok(()) => {
                if let Some(expected) = expected_sha256 {
                    verify_archive_sha256(dest_path, expected)?;
                }
                return Ok(());
            }
            Err(msg) => {
                if attempt >= MAX_ATTEMPTS {
                    let _ = fs::remove_file(dest_path);
                    return Err(format!("{msg} (failed after {attempt} attempts)"));
                }
                let backoff = Duration::from_secs(1u64 << (attempt - 1).min(4));
                warn!(
                    "[rayline-llama] Download attempt {}/{} failed: {}. Retrying in {:?}.",
                    attempt, MAX_ATTEMPTS, msg, backoff
                );
                thread::sleep(backoff);
            }
        }
    }
}

fn expected_archive_sha256_for_url(url: &str) -> Result<Option<&'static str>, String> {
    let Some(rest) = url.strip_prefix(&format!("{LLAMA_RELEASE_BASE}/")) else {
        if allow_unverified_runtime_download() {
            warn!(
                "[rayline-llama] {}=1: downloading runtime from unrecognized URL without SHA256 verification: {}",
                UNVERIFIED_RUNTIME_DOWNLOAD_ENV, url
            );
            return Ok(None);
        }
        return Err(format!(
            "No committed SHA256 for llama.cpp runtime URL {url}; refusing to download. Set {UNVERIFIED_RUNTIME_DOWNLOAD_ENV}=1 for local development only."
        ));
    };
    let Some((tag, filename)) = rest.split_once('/') else {
        return Err(format!("Malformed llama.cpp runtime download URL: {url}"));
    };
    if tag.is_empty() || filename.is_empty() || filename.contains('/') {
        return Err(format!("Malformed llama.cpp runtime download URL: {url}"));
    }
    match expected_archive_sha256(tag, filename) {
        Some(expected) => Ok(Some(expected)),
        None if allow_unverified_runtime_download() => {
            warn!(
                "[rayline-llama] {}=1: downloading {} without committed SHA256 verification",
                UNVERIFIED_RUNTIME_DOWNLOAD_ENV, filename
            );
            Ok(None)
        }
        None => Err(format!(
            "No committed SHA256 for llama.cpp runtime archive {filename} (tag {tag}); refusing to download. Add it to the runtime archive manifest or set {UNVERIFIED_RUNTIME_DOWNLOAD_ENV}=1 for local development only."
        )),
    }
}

pub fn verify_archive_sha256(archive_path: &Path, expected_sha256: &str) -> Result<(), String> {
    let actual = sha256_file(archive_path)?;
    if actual != expected_sha256 {
        let _ = fs::remove_file(archive_path);
        return Err(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            archive_path.display(),
            expected_sha256,
            actual
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("Failed to read archive for SHA256: {e}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn attempt_download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest_path: &Path,
) -> Result<(), String> {
    let mut response = client
        .get(url)
        .header("User-Agent", "rayline")
        .send()
        .map_err(|e| format!("Request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("Download failed with status {}", response.status()));
    }

    let mut file =
        fs::File::create(dest_path).map_err(|e| format!("Failed to create dest file: {e}"))?;
    let mut buf = [0u8; 65536];
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| format!("Read error: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("Write error: {e}"))?;
    }
    Ok(())
}

/// Extract the `llama-server` binary and adjacent shared libraries from an
/// archive into `dest_dir`. Returns the path to the extracted binary.
pub fn extract_runtime_binary(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dest_dir).map_err(|e| format!("Failed to create dest dir: {e}"))?;

    let binary_name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };

    let archive_name = archive_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    let temp_dir = dest_dir.join("_extract_tmp");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp extraction dir: {e}"))?;

    if archive_name.ends_with(".tar.gz") {
        extract_tar_gz(archive_path, &temp_dir)?;
    } else if archive_name.ends_with(".zip") {
        extract_zip(archive_path, &temp_dir)?;
    } else {
        return Err(format!("Unknown archive format: {archive_name}"));
    }

    let binary_src = find_file_recursive(&temp_dir, binary_name)
        .ok_or_else(|| format!("Could not find {binary_name} in extracted archive"))?;
    let src_parent = binary_src.parent().unwrap_or(&temp_dir);

    let binary_dest = dest_dir.join(binary_name);
    fs::copy(&binary_src, &binary_dest).map_err(|e| format!("Failed to copy binary: {e}"))?;

    if let Ok(entries) = fs::read_dir(src_parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let is_shared_lib = name.ends_with(".dylib")
                || name.ends_with(".so")
                || (name.ends_with(".dll") && !name.ends_with(".exe"))
                || name.contains(".so.");
            if is_shared_lib {
                let lib_dest = dest_dir.join(path.file_name().unwrap());
                if let Err(e) = fs::copy(&path, &lib_dest) {
                    warn!(
                        "[rayline-llama] Failed to copy shared lib {:?}: {}",
                        name, e
                    );
                } else {
                    info!("[rayline-llama] Copied shared library: {}", name);
                }
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&binary_dest)
            .map_err(|e| format!("Failed to read metadata: {e}"))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_dest, perms)
            .map_err(|e| format!("Failed to set permissions: {e}"))?;
    }

    // On macOS, re-sign ad-hoc so a notarized parent application
    // can spawn this binary as a child without AMFI killing it.
    #[cfg(target_os = "macos")]
    {
        let path_str = binary_dest.to_string_lossy().to_string();
        let _ = Command::new("codesign")
            .args(["--force", "--sign", "-", &path_str])
            .output();
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine", &path_str])
            .output();
    }

    let _ = fs::remove_dir_all(&temp_dir);
    let _ = fs::remove_file(archive_path);

    Ok(binary_dest)
}

fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let output = Command::new("tar")
        .args([
            "xzf",
            &archive_path.to_string_lossy(),
            "-C",
            &dest_dir.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("Failed to run tar: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn extract_zip(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let output = Command::new("unzip")
            .args([
                "-q",
                &archive_path.to_string_lossy(),
                "-d",
                &dest_dir.to_string_lossy(),
            ])
            .output()
            .map_err(|e| format!("Failed to run unzip: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "unzip failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    #[cfg(windows)]
    {
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    archive_path.to_string_lossy(),
                    dest_dir.to_string_lossy()
                ),
            ])
            .output()
            .map_err(|e| format!("Failed to run PowerShell Expand-Archive: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "Expand-Archive failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(())
}

fn find_file_recursive(dir: &Path, target: &str) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().map(|n| n == target).unwrap_or(false) {
                return Some(path);
            }
            if path.is_dir() {
                if let Some(found) = find_file_recursive(&path, target) {
                    return Some(found);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Process lifecycle
// ---------------------------------------------------------------------------

/// Pick a random free port in [30000, 45000). Returns `None` if 20 attempts fail.
pub fn find_free_port() -> Option<u16> {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    for i in 0..20 {
        // Cheap LCG — we don't need cryptographic randomness for port pick.
        let port =
            30000 + ((seed.wrapping_mul(2862933555777941757).wrapping_add(i)) % 15000) as u16;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// User-configurable llama-server launch options. `None` fields use the
/// llama.cpp default appropriate for the detected runtime variant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerOptions {
    pub gpu_layers: Option<i32>,
    pub flash_attention: Option<bool>,
    pub cache_type_k: Option<String>,
    pub cache_type_v: Option<String>,
    pub parallel: Option<u32>,
    pub threads: Option<u32>,
    pub extra_args: Option<Vec<String>>,
}

/// Owns the spawned `llama-server` child and the bin/log directories.
///
/// Cheap to `clone` — internal state is behind `Arc`. Drop the last clone
/// (or call `stop`) to terminate the child.
pub struct LlamaServerManager {
    port: Arc<AtomicU16>,
    is_running: Arc<AtomicBool>,
    model_path: Arc<Mutex<Option<String>>>,
    data_dir: PathBuf,
    ctx_size: Arc<AtomicU32>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Clone for LlamaServerManager {
    fn clone(&self) -> Self {
        Self {
            port: Arc::clone(&self.port),
            is_running: Arc::clone(&self.is_running),
            model_path: Arc::clone(&self.model_path),
            data_dir: self.data_dir.clone(),
            ctx_size: Arc::clone(&self.ctx_size),
            child: Arc::clone(&self.child),
        }
    }
}

impl LlamaServerManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            port: Arc::new(AtomicU16::new(0)),
            is_running: Arc::new(AtomicBool::new(false)),
            model_path: Arc::new(Mutex::new(None)),
            data_dir,
            ctx_size: Arc::new(AtomicU32::new(0)),
            child: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get_port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }

    pub fn runtime_binary_path(&self) -> PathBuf {
        let name = if cfg!(windows) {
            "llama-server.exe"
        } else {
            "llama-server"
        };
        self.bin_dir().join(name)
    }

    pub fn version_file_path(&self) -> PathBuf {
        self.bin_dir().join(".version")
    }

    pub fn variant_file_path(&self) -> PathBuf {
        self.bin_dir().join(".variant")
    }

    pub fn runtime_variant(&self) -> String {
        fs::read_to_string(self.variant_file_path())
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    pub fn runtime_installed(&self) -> bool {
        self.runtime_binary_path().exists()
    }

    pub fn runtime_version(&self) -> String {
        fs::read_to_string(self.version_file_path())
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    pub fn get_ctx_size(&self) -> u32 {
        self.ctx_size.load(Ordering::SeqCst)
    }

    pub fn log_file_path(&self) -> PathBuf {
        self.data_dir.join("llama-server.log")
    }
}

/// Spawn `llama-server` against `model_path`. Returns the bound port. Does
/// not wait for the model to finish loading — poll [`check_health`] before
/// sending real traffic.
pub fn start_server(
    manager: &LlamaServerManager,
    model_path: &str,
    ctx_size: u32,
    opts: &ServerOptions,
) -> Result<u16, String> {
    start_server_with_log_observer(manager, model_path, ctx_size, opts, None)
}

/// Spawn `llama-server` and tee stdout/stderr to the normal log file while also
/// offering each emitted line to `log_observer`. The observer is for in-memory
/// telemetry only; the log file remains the source for human diagnostics.
pub fn start_server_with_log_observer(
    manager: &LlamaServerManager,
    model_path: &str,
    ctx_size: u32,
    opts: &ServerOptions,
    log_observer: Option<LogObserver>,
) -> Result<u16, String> {
    if manager.is_running() {
        let port = manager.get_port();
        if port > 0 && check_health(port) {
            info!("[rayline-llama] Server already running on port {}", port);
            return Ok(port);
        }
        let _ = stop_server(manager);
    }

    let binary = manager.runtime_binary_path();
    if !binary.exists() {
        return Err("llama-server binary not found. Download it first.".to_string());
    }

    let port = find_free_port().ok_or("Could not find a free port")?;
    info!(
        "[rayline-llama] Starting llama-server on port {} with model {}",
        port, model_path
    );

    let variant = manager.runtime_variant();
    let is_gpu = matches!(variant.as_str(), "cuda" | "vulkan" | "metal");
    let parallel = opts.parallel.unwrap_or(1);

    let mut args = vec![
        "-m".to_string(),
        model_path.to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "-c".to_string(),
        ctx_size.to_string(),
        "--parallel".to_string(),
        parallel.to_string(),
        "--jinja".to_string(),
    ];

    if let Some(threads) = opts.threads {
        args.extend(["-t".to_string(), threads.to_string()]);
    }

    if is_gpu {
        let ngl = opts.gpu_layers.unwrap_or(99);
        let cache_k = opts.cache_type_k.as_deref().unwrap_or("q8_0");
        let cache_v = opts.cache_type_v.as_deref().unwrap_or("q8_0");
        let flash_attn = opts.flash_attention.unwrap_or(true);
        args.extend([
            "-ngl".to_string(),
            ngl.to_string(),
            "--cache-type-k".to_string(),
            cache_k.to_string(),
            "--cache-type-v".to_string(),
            cache_v.to_string(),
        ]);
        if flash_attn {
            args.extend(["-fa".to_string(), "on".to_string()]);
        }
    }

    if let Some(ref extra) = opts.extra_args {
        args.extend(extra.iter().cloned());
    }

    let log_path = manager.log_file_path();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let log_file = fs::File::create(&log_path)
        .map_err(|e| format!("Failed to create llama-server log file: {e}"))?;
    let log_file = Arc::new(Mutex::new(log_file));

    let mut cmd = Command::new(&binary);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn llama-server: {e}"))?;
    if let Some(stdout) = child.stdout.take() {
        spawn_log_pump(stdout, log_file.clone(), log_observer.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_pump(stderr, log_file.clone(), log_observer);
    }
    let pid = child.id();
    info!(
        "[rayline-llama] llama-server spawned pid={} log={:?}",
        pid, log_path
    );

    *manager.child.lock().unwrap() = Some(child);
    manager.port.store(port, Ordering::SeqCst);
    manager.is_running.store(true, Ordering::SeqCst);
    manager.ctx_size.store(ctx_size, Ordering::SeqCst);
    *manager.model_path.lock().unwrap() = Some(model_path.to_string());

    Ok(port)
}

fn spawn_log_pump<R>(reader: R, log_file: Arc<Mutex<fs::File>>, observer: Option<LogObserver>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = match reader.read_line(&mut line) {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!("[rayline-llama] log pump read failed: {error}");
                    break;
                }
            };
            if bytes == 0 {
                break;
            }
            if let Ok(mut file) = log_file.lock() {
                let _ = file.write_all(line.as_bytes());
            }
            if let Some(observer) = observer.as_ref() {
                observer(line.trim_end_matches(['\r', '\n']));
            }
        }
    });
}

/// Probe `GET /health`. Returns true only when status is 2xx AND the body
/// does not contain "loading model" or "error" (llama-server sometimes
/// returns 200 mid-load).
pub fn check_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    let Ok(resp) = client.get(&url).send() else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    match resp.text() {
        Ok(body) => !body.contains("loading model") && !body.contains("error"),
        Err(_) => true,
    }
}

/// Block until `check_health` returns true or the deadline elapses.
pub fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check_health(port) {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Stop the managed `llama-server`. Sends `SIGTERM` then `SIGKILL` after a
/// short grace period on Unix; uses `taskkill /F` on Windows.
pub fn stop_server(manager: &LlamaServerManager) -> Result<(), String> {
    let mut child_slot = manager.child.lock().unwrap();
    let child = child_slot.take();
    manager.is_running.store(false, Ordering::SeqCst);
    manager.port.store(0, Ordering::SeqCst);
    manager.ctx_size.store(0, Ordering::SeqCst);
    *manager.model_path.lock().unwrap() = None;

    if let Some(mut child) = child {
        let pid = child.id();
        info!("[rayline-llama] Stopping llama-server pid {}", pid);

        #[cfg(unix)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }

        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .spawn();
        }

        thread::sleep(Duration::from_millis(500));

        let _ = child.kill();
        let _ = child.wait();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_meminfo_total() {
        let sample = "MemTotal:       16384000 kB\nMemFree: 1234 kB\n";
        assert_eq!(parse_meminfo_total(sample), 16_384_000u64 * 1024);
    }

    #[test]
    fn test_resolve_archive_filename_macos_arm() {
        assert_eq!(
            resolve_archive_filename("b9585", "macos", "aarch64", "apple-silicon").unwrap(),
            "llama-b9585-bin-macos-arm64.tar.gz"
        );
    }

    #[test]
    fn test_resolve_archive_filename_linux_x64() {
        assert_eq!(
            resolve_archive_filename("b9585", "linux", "x86_64", "nvidia").unwrap(),
            "llama-b9585-bin-ubuntu-x64.tar.gz"
        );
    }

    #[test]
    fn test_resolve_archive_filename_windows_variants() {
        assert!(
            resolve_archive_filename("b9585", "windows", "x86_64", "nvidia")
                .unwrap()
                .contains("win-cuda")
        );
        assert!(
            resolve_archive_filename("b9585", "windows", "x86_64", "amd")
                .unwrap()
                .contains("win-vulkan")
        );
        assert!(
            resolve_archive_filename("b9585", "windows", "x86_64", "none")
                .unwrap()
                .contains("win-cpu")
        );
    }

    #[test]
    fn test_resolve_runtime_archive_selects_url_filename_and_checksum() {
        let archive =
            resolve_runtime_archive("b9585", "macos", "aarch64", "apple-silicon").unwrap();
        assert_eq!(archive.filename, "llama-b9585-bin-macos-arm64.tar.gz");
        assert_eq!(
            archive.url,
            "https://github.com/ggml-org/llama.cpp/releases/download/b9585/llama-b9585-bin-macos-arm64.tar.gz"
        );
        assert_eq!(
            archive.expected_sha256,
            Some("e88f05f82c8c0c0f5a861ff7822f096ad6641128e6f64c666eee743f46730db6")
        );
    }

    #[test]
    fn test_resolve_download_url_requires_committed_checksum() {
        let err =
            resolve_runtime_archive_checked("b6000", "macos", "aarch64", "apple-silicon", false)
                .unwrap_err();
        assert!(err.contains("No committed SHA256"));
    }

    #[test]
    fn test_resolve_archive_filename_unsupported() {
        assert!(resolve_archive_filename("b9585", "freebsd", "x86_64", "none").is_err());
    }

    #[test]
    fn test_verify_archive_sha256_removes_mismatched_archive() {
        let path = std::env::temp_dir().join(format!(
            "rayline-llama-sha256-mismatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"not the expected archive").unwrap();

        let err = verify_archive_sha256(
            &path,
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap_err();

        assert!(err.contains("sha256 mismatch"));
        assert!(!path.exists());
    }
}

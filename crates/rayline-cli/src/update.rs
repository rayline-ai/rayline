use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const UPDATE_BASE_URL_ENV: &str = "RAYLINE_UPDATE_BASE_URL";
const UPDATE_INSTALL_PATH_ENV: &str = "RAYLINE_UPDATE_INSTALL_PATH";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateRequest {
    pub channel: Option<String>,
    pub pinned_version: Option<String>,
    pub force: bool,
    pub check_only: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateResult {
    pub message: String,
    pub exit_code: u8,
    pub stderr: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateError {
    Checksum(String),
    InvalidVersion(String),
    Install(String),
    Network(String),
    UnsupportedPlatform(String),
}

struct UpdateArtifacts {
    launcher: PathBuf,
    daemon: PathBuf,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Checksum(message)
            | Self::Install(message)
            | Self::Network(message)
            | Self::UnsupportedPlatform(message) => formatter.write_str(message),
            Self::InvalidVersion(version) => write!(formatter, "invalid --version '{version}'"),
        }
    }
}

impl std::error::Error for UpdateError {}

pub async fn run(request: &UpdateRequest) -> Result<UpdateResult, UpdateError> {
    let home = dirs::home_dir();
    let channel = request
        .channel
        .clone()
        .unwrap_or_else(|| resolve_channel(home.as_deref()));

    if channel == "local" {
        return Ok(UpdateResult {
            message: concat!(
                "Local dev install detected \u{2014} refresh with ",
                "`make install` or `make install-dev`.\n"
            )
            .to_owned(),
            exit_code: 2,
            stderr: true,
        });
    }

    let install_path = install_path().map_err(|error| {
        UpdateError::Install(format!(
            "failed to resolve {} install path: {error}",
            crate::CLI_BIN
        ))
    })?;
    if is_app_bundle_managed(&install_path) {
        return Ok(UpdateResult {
            message: format!(
                "{} is managed by Rayline.app. Update the app to update the CLI:\n  \
                 re-download from https://get.rayline.ai\n",
                crate::CLI_BIN
            ),
            exit_code: 0,
            stderr: false,
        });
    }
    if is_homebrew_managed(&install_path) {
        return Ok(UpdateResult {
            message: format!(
                "Installed via Homebrew. Run `brew upgrade {}` to update.\n",
                crate::CLI_BIN
            ),
            exit_code: 0,
            stderr: false,
        });
    }

    let platform_tag = detect_platform_tag()?;
    let current = Version::parse(crate::RAYLINE_VERSION)
        .map_err(|_| UpdateError::InvalidVersion(crate::RAYLINE_VERSION.to_owned()))?;
    let target = match request.pinned_version.as_deref() {
        Some(version) => {
            Version::parse(version).map_err(|_| UpdateError::InvalidVersion(version.to_owned()))?
        }
        None => fetch_latest_version(&channel).await?,
    };

    let decision = evaluate_check(
        &current,
        &target,
        request.pinned_version.is_some() || request.channel.is_some(),
        request.force,
    );
    if decision.exit_code == 0 || request.check_only {
        return Ok(decision);
    }

    let temp_dir = temp_update_dir()?;
    let artifacts = download_and_verify(&target.normalized, platform_tag, &temp_dir).await;
    let result = match artifacts {
        Ok(artifacts) => {
            if request.dry_run {
                Ok(UpdateResult {
                    message: format!(
                        "Would install {} and {}.\n",
                        artifacts.launcher.display(),
                        artifacts.daemon.display()
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            } else {
                let daemon_install_path = install_path
                    .parent()
                    .ok_or_else(|| {
                        UpdateError::Install(format!(
                            "install path has no parent directory: {}",
                            install_path.display()
                        ))
                    })?
                    // `EXE_SUFFIX` is "" on Unix and ".exe" on Windows, so the
                    // daemon installs as `rld` on Unix and `rld.exe` on Windows
                    // (matching the `.exe` lookup in `router::find_on_path`).
                    .join(format!(
                        "{}{}",
                        crate::DAEMON_BIN,
                        std::env::consts::EXE_SUFFIX
                    ));
                replace_binary(&artifacts.launcher, &install_path)?;
                replace_binary(&artifacts.daemon, &daemon_install_path)?;
                cleanup_legacy_uv_install(crate::UV_TOOL_NAME, &install_path);
                Ok(UpdateResult {
                    message: format!(
                        "Updated {} {} \u{2192} {}.\n",
                        crate::DISPLAY_NAME,
                        current.public,
                        target.public
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            }
        }
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(&temp_dir);
    result
}

pub fn evaluate_check(
    current: &Version,
    target: &Version,
    explicit_intent: bool,
    force: bool,
) -> UpdateResult {
    let update_available = if explicit_intent {
        target.normalized != current.normalized || force
    } else {
        target.public_cmp(current) == Ordering::Greater || force
    };

    if update_available {
        return UpdateResult {
            message: format!(
                "Update available: {} \u{2192} {}. Run: {} update\n",
                current.public,
                target.public,
                crate::CLI_BIN
            ),
            exit_code: 1,
            stderr: false,
        };
    }

    UpdateResult {
        message: format!("Already on latest ({}).\n", current.public),
        exit_code: 0,
        stderr: false,
    }
}

pub fn resolve_channel(home: Option<&Path>) -> String {
    let env_channel = std::env::var("RAYLINE_UPDATE_CHANNEL").ok();
    resolve_channel_from_sources(home, env_channel.as_deref(), crate::RAYLINE_CHANNEL)
}

fn resolve_channel_from_sources(
    home: Option<&Path>,
    env_channel: Option<&str>,
    embedded_channel: &str,
) -> String {
    if let Some(channel) = env_channel {
        if is_valid_channel(channel) {
            return channel.to_owned();
        }
    }

    if let Some(home) = home {
        if let Some(channel) = config_channel(home) {
            return channel;
        }
    }

    if is_valid_channel(embedded_channel) {
        return embedded_channel.to_owned();
    }

    "local".to_owned()
}

fn config_channel(home: &Path) -> Option<String> {
    let path = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join("cli.toml");
    let contents = std::fs::read_to_string(path).ok()?;
    let mut in_update_section = false;

    for raw_line in contents.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_update_section = line == "[update]";
            continue;
        }
        if !in_update_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "channel" {
            continue;
        }
        let channel = value.trim().trim_matches('"').trim_matches('\'');
        if is_valid_channel(channel) {
            return Some(channel.to_owned());
        }
    }

    None
}

fn is_valid_channel(channel: &str) -> bool {
    // Release ladder for both brands: main (rolling, rawest) -> dev (release
    // candidate) -> prod (public X.Y.Z). `local` is the dev-build sentinel.
    matches!(channel, "prod" | "dev" | "main" | "local")
}

async fn fetch_latest_version(channel: &str) -> Result<Version, UpdateError> {
    let url = latest_url_for(channel).ok_or_else(|| {
        UpdateError::Network(format!("channel '{channel}' has no latest pointer"))
    })?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|error| UpdateError::Network(format!("failed to fetch latest version: {error}")))?
        .get(url.clone())
        .send()
        .await
        .map_err(|error| {
            UpdateError::Network(format!("failed to fetch latest version: {error}"))
        })?;

    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "latest pointer returned HTTP {}",
            response.status().as_u16()
        )));
    }

    let text = response
        .text()
        .await
        .map_err(|error| UpdateError::Network(format!("failed to fetch latest version: {error}")))?
        .trim()
        .to_owned();
    Version::parse(&text).map_err(|_| {
        UpdateError::Network(format!("could not parse version from response: {text:?}"))
    })
}

fn latest_url_for(channel: &str) -> Option<String> {
    let latest_key = match channel {
        "prod" => "cli/latest.txt",
        "dev" => "cli/latest-dev.txt",
        "main" => "cli/latest-main.txt",
        _ => return None,
    };
    Some(format!("{}/{latest_key}", base_url()))
}

fn artifact_url_for(version: &str, platform_tag: &str) -> String {
    format!(
        "{}/cli/v{version}/{}-{platform_tag}",
        base_url(),
        crate::CLI_BIN
    )
}

fn daemon_artifact_url_for(version: &str, platform_tag: &str) -> String {
    format!(
        "{}/cli/v{version}/{}-{platform_tag}",
        base_url(),
        crate::DAEMON_BIN
    )
}

fn checksums_url_for(version: &str) -> String {
    format!("{}/cli/v{version}/SHA256SUMS", base_url())
}

pub(crate) fn base_url() -> String {
    std::env::var(UPDATE_BASE_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| crate::UPDATE_BASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn detect_platform_tag() -> Result<&'static str, UpdateError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macosx_11_0_arm64"),
        ("macos", "x86_64") => Ok("macosx_10_12_x86_64"),
        ("linux", "x86_64") => Ok("linux_x86_64"),
        ("linux", "aarch64") => Ok("linux_aarch64"),
        ("windows", "x86_64") => Ok("windows_x86_64"),
        (os, arch) => Err(UpdateError::UnsupportedPlatform(format!(
            "unsupported platform: {os}-{arch}. Supported: linux_aarch64, linux_x86_64, macosx_10_12_x86_64, macosx_11_0_arm64, windows_x86_64",
        ))),
    }
}

async fn download_and_verify(
    version: &str,
    platform_tag: &str,
    dest_dir: &Path,
) -> Result<UpdateArtifacts, UpdateError> {
    fs::create_dir_all(dest_dir).map_err(UpdateError::from)?;
    let launcher_name = format!("{}-{platform_tag}", crate::CLI_BIN);
    let daemon_name = format!("{}-{platform_tag}", crate::DAEMON_BIN);
    let launcher_path = dest_dir.join(&launcher_name);
    let daemon_path = dest_dir.join(&daemon_name);
    let sums_path = dest_dir.join("SHA256SUMS");

    download_to(&artifact_url_for(version, platform_tag), &launcher_path).await?;
    download_to(
        &daemon_artifact_url_for(version, platform_tag),
        &daemon_path,
    )
    .await?;
    download_to(&checksums_url_for(version), &sums_path).await?;

    let sums = fs::read_to_string(&sums_path).map_err(UpdateError::from)?;
    verify_checksum(&sums, &launcher_name, &launcher_path)?;
    verify_checksum(&sums, &daemon_name, &daemon_path)?;

    Ok(UpdateArtifacts {
        launcher: launcher_path,
        daemon: daemon_path,
    })
}

pub(crate) async fn download_to(url: &str, path: &Path) -> Result<(), UpdateError> {
    let part = path.with_extension("part");
    if let Some(source) = url.strip_prefix("file://") {
        fs::copy(source, &part).map_err(UpdateError::from)?;
        fs::rename(&part, path).map_err(UpdateError::from)?;
        return Ok(());
    }

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?
        .get(url)
        .send()
        .await
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "GET {url} returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?;
    fs::write(&part, bytes).map_err(UpdateError::from)?;
    fs::rename(&part, path).map_err(UpdateError::from)?;
    Ok(())
}

fn expected_sha256(sums: &str, artifact_name: &str) -> Result<String, UpdateError> {
    for line in sums.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let mut parts = stripped.split_whitespace();
        let Some(digest) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name.trim().trim_start_matches('*') == artifact_name {
            return Ok(digest.to_owned());
        }
    }
    Err(UpdateError::Checksum(format!(
        "no sha256 entry for {artifact_name} in SHA256SUMS"
    )))
}

pub(crate) fn verify_checksum(
    sums: &str,
    artifact_name: &str,
    path: &Path,
) -> Result<(), UpdateError> {
    let expected = expected_sha256(sums, artifact_name)?;
    let actual = sha256_file(path)?;
    if expected != actual {
        let _ = fs::remove_file(path);
        return Err(UpdateError::Checksum(format!(
            "sha256 mismatch for {artifact_name}: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let bytes = fs::read(path).map_err(UpdateError::from)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn install_path() -> io::Result<PathBuf> {
    std::env::var_os(UPDATE_INSTALL_PATH_ENV)
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)
        .map(resolve_install_path)
}

/// `current_exe()` may return the invoking symlink rather than its target on
/// some platforms (macOS). A migrated install runs the binary through the
/// legacy `rl -> rayline` alias the installer leaves behind, and self-update
/// must replace the real binary: renaming the download over the alias would
/// orphan `rayline` at the old version, reintroducing exactly the divergence
/// the installer migration removes. Nonexistent paths (e.g. an env override
/// pointing at a not-yet-created file) pass through unchanged.
fn resolve_install_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn is_homebrew_managed(install_path: &Path) -> bool {
    let Some(prefix) = homebrew_prefix() else {
        return false;
    };
    path_is_under_prefix(install_path, &prefix)
}

/// True when the running `rayline` lives inside a `.app` bundle (directly, or via a
/// `/usr/local/bin` symlink created by the in-app "Install command line tools"
/// action). The app owns updates in that case; self-replacing would break the
/// notarization seal. Canonicalize first so symlink paths resolve to the real
/// bundle path before matching.
fn is_app_bundle_managed(install_path: &Path) -> bool {
    let resolved =
        std::fs::canonicalize(install_path).unwrap_or_else(|_| install_path.to_path_buf());
    resolved.to_string_lossy().contains(".app/Contents/")
}

fn path_is_under_prefix(install_path: &Path, prefix: &Path) -> bool {
    let install_path =
        fs::canonicalize(install_path).unwrap_or_else(|_| install_path.to_path_buf());
    let prefix = fs::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    install_path.starts_with(prefix)
}

fn homebrew_prefix() -> Option<PathBuf> {
    let brew = command_on_path("brew")?;
    let output = Command::new(brew)
        .arg("--prefix")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    if prefix.is_empty() {
        None
    } else {
        Some(PathBuf::from(prefix))
    }
}

pub(crate) fn replace_binary(downloaded: &Path, install_path: &Path) -> Result<(), UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent directory: {}",
            install_path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        UpdateError::Install(format!(
            "failed to create install directory {}: {error}",
            parent.display()
        ))
    })?;

    let staged = staged_install_path(install_path)?;
    fs::copy(downloaded, &staged).map_err(|error| {
        UpdateError::Install(format!(
            "failed to stage update at {}: {error}",
            staged.display()
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&staged, permissions).map_err(|error| {
            UpdateError::Install(format!(
                "failed to mark update executable at {}: {error}",
                staged.display()
            ))
        })?;
    }

    let result = move_into_place(&staged, install_path);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Atomically move the staged binary onto `install_path`.
///
/// On Unix `rename(2)` replaces the target even while it is running. Windows
/// locks a running `.exe` and rejects renaming over it, but it *does* allow
/// renaming the running file out of the way — so we move the current binary to
/// a sidecar path first, then move the update into place, restoring the
/// original if the second move fails.
#[cfg(not(windows))]
fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    fs::rename(staged, install_path).map_err(|error| {
        UpdateError::Install(format!(
            "failed to replace {}: {error}",
            install_path.display()
        ))
    })
}

#[cfg(windows)]
fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    if !install_path.exists() {
        return fs::rename(staged, install_path).map_err(|error| {
            UpdateError::Install(format!(
                "failed to install {}: {error}",
                install_path.display()
            ))
        });
    }

    let backup = staged_install_path(install_path)?;
    fs::rename(install_path, &backup).map_err(|error| {
        UpdateError::Install(format!(
            "failed to move existing binary {} aside: {error}",
            install_path.display()
        ))
    })?;

    match fs::rename(staged, install_path) {
        Ok(()) => {
            // Best-effort: the old binary stays locked while it is the running
            // process, so removal may fail here and succeed on a later update.
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(error) => {
            // Restore the original so a failed update never leaves a hole.
            let _ = fs::rename(&backup, install_path);
            Err(UpdateError::Install(format!(
                "failed to replace {}: {error}",
                install_path.display()
            )))
        }
    }
}

fn staged_install_path(install_path: &Path) -> Result<PathBuf, UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent directory: {}",
            install_path.display()
        ))
    })?;
    let file_name = install_path.file_name().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no file name: {}",
            install_path.display()
        ))
    })?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let mut staged_name = OsString::from(".");
    staged_name.push(file_name);
    staged_name.push(format!(".tmp-{}-{unique}", std::process::id()));
    Ok(parent.join(staged_name))
}

fn cleanup_legacy_uv_install(tool_name: &str, install_path: &Path) {
    if std::env::var_os(UPDATE_INSTALL_PATH_ENV).is_some() {
        return;
    }
    let Some(uv) = command_on_path("uv") else {
        return;
    };
    if is_uv_tool_managed_install(&uv, tool_name, install_path) {
        return;
    }
    let _ = run_legacy_uv_uninstall(&uv, tool_name);
}

fn is_uv_tool_managed_install(uv: &Path, tool_name: &str, install_path: &Path) -> bool {
    let output = Command::new(uv)
        .args(["tool", "dir"])
        .stdin(Stdio::null())
        .output()
        .ok();
    let Some(output) = output.filter(|output| output.status.success()) else {
        return false;
    };
    let Some(tools_dir) = String::from_utf8(output.stdout)
        .ok()
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    is_uv_tool_managed_path(tool_name, install_path, Path::new(&tools_dir))
}

fn is_uv_tool_managed_path(tool_name: &str, install_path: &Path, tools_dir: &Path) -> bool {
    path_is_under_prefix(install_path, &tools_dir.join(tool_name))
}

fn run_legacy_uv_uninstall(uv: &Path, tool_name: &str) -> io::Result<bool> {
    let status = Command::new(uv)
        .args(["tool", "uninstall", tool_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

fn command_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn temp_update_dir() -> Result<PathBuf, UpdateError> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "{}-update-{}-{unique}",
        crate::CLI_BIN,
        std::process::id()
    ));
    fs::create_dir_all(&path).map_err(UpdateError::from)?;
    Ok(path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    normalized: String,
    public: String,
    base_parts: Vec<u64>,
    suffix: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionParseError;

#[derive(Debug, Eq, PartialEq)]
enum VersionToken {
    Number(u64),
    Text(String),
}

impl Version {
    pub fn parse(raw: &str) -> Result<Self, VersionParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(VersionParseError);
        }
        let (public, local) = trimmed.split_once('+').unwrap_or((trimmed, ""));
        if public.is_empty() || trimmed.matches('+').count() > 1 {
            return Err(VersionParseError);
        }
        if trimmed.contains('+') && local.is_empty() {
            return Err(VersionParseError);
        }
        if !local.is_empty() {
            validate_version_label(local)?;
        }

        let (base_parts, suffix) = parse_public_version(public)?;

        Ok(Self {
            normalized: trimmed.to_ascii_lowercase(),
            public: public.to_owned(),
            base_parts,
            suffix: suffix.map(str::to_ascii_lowercase),
        })
    }

    fn base_cmp(&self, other: &Self) -> Ordering {
        let length = self.base_parts.len().max(other.base_parts.len());
        for index in 0..length {
            let left = self.base_parts.get(index).copied().unwrap_or(0);
            let right = other.base_parts.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    }

    fn public_cmp(&self, other: &Self) -> Ordering {
        match self.base_cmp(other) {
            Ordering::Equal => compare_suffixes(self.suffix.as_deref(), other.suffix.as_deref()),
            ordering => ordering,
        }
    }
}

fn parse_public_version(public: &str) -> Result<(Vec<u64>, Option<&str>), VersionParseError> {
    let mut base_parts = Vec::new();
    let mut index = 0;
    let bytes = public.as_bytes();

    loop {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return Err(VersionParseError);
        }
        base_parts.push(
            public[start..index]
                .parse::<u64>()
                .map_err(|_| VersionParseError)?,
        );

        if index == bytes.len() {
            return Ok((base_parts, None));
        }
        if bytes[index] == b'.' && index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit() {
            index += 1;
            continue;
        }

        let suffix = &public[index..];
        validate_version_label(suffix)?;
        return Ok((base_parts, Some(suffix)));
    }
}

fn validate_version_label(label: &str) -> Result<(), VersionParseError> {
    if label.is_empty()
        || !label.chars().any(|ch| ch.is_ascii_alphanumeric())
        || !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(VersionParseError);
    }
    Ok(())
}

fn compare_suffixes(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(right)) if suffix_is_post_release(right) => Ordering::Less,
        (Some(left), None) if suffix_is_post_release(left) => Ordering::Greater,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => compare_suffix_tokens(left, right),
    }
}

fn suffix_is_post_release(suffix: &str) -> bool {
    suffix_tokens(suffix).first().is_some_and(
        |token| matches!(token, VersionToken::Text(text) if text_rank(text) == POST_RELEASE_RANK),
    )
}

fn compare_suffix_tokens(left: &str, right: &str) -> Ordering {
    let left_tokens = suffix_tokens(left);
    let right_tokens = suffix_tokens(right);
    let length = left_tokens.len().max(right_tokens.len());
    for index in 0..length {
        match (left_tokens.get(index), right_tokens.get(index)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => match compare_version_tokens(left, right) {
                Ordering::Equal => {}
                ordering => return ordering,
            },
        }
    }
    Ordering::Equal
}

fn compare_version_tokens(left: &VersionToken, right: &VersionToken) -> Ordering {
    match (left, right) {
        (VersionToken::Number(left), VersionToken::Number(right)) => left.cmp(right),
        (VersionToken::Text(left), VersionToken::Text(right)) => {
            match text_rank(left).cmp(&text_rank(right)) {
                Ordering::Equal => left.cmp(right),
                ordering => ordering,
            }
        }
        (VersionToken::Number(_), VersionToken::Text(_)) => Ordering::Less,
        (VersionToken::Text(_), VersionToken::Number(_)) => Ordering::Greater,
    }
}

fn suffix_tokens(suffix: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_digit = None;
    for ch in suffix
        .trim_start_matches(['.', '-', '_'])
        .chars()
        .flat_map(char::to_lowercase)
    {
        if matches!(ch, '.' | '-' | '_') {
            push_suffix_token(&mut tokens, &mut current, &mut current_is_digit);
            continue;
        }
        let is_digit = ch.is_ascii_digit();
        if current_is_digit.is_some_and(|current| current != is_digit) {
            push_suffix_token(&mut tokens, &mut current, &mut current_is_digit);
        }
        current_is_digit = Some(is_digit);
        current.push(ch);
    }
    push_suffix_token(&mut tokens, &mut current, &mut current_is_digit);
    tokens
}

fn push_suffix_token(
    tokens: &mut Vec<VersionToken>,
    current: &mut String,
    current_is_digit: &mut Option<bool>,
) {
    if current.is_empty() {
        *current_is_digit = None;
        return;
    }
    if current_is_digit.unwrap_or(false) {
        let number = current.parse::<u64>().unwrap_or(u64::MAX);
        tokens.push(VersionToken::Number(number));
    } else {
        tokens.push(VersionToken::Text(std::mem::take(current)));
    }
    current.clear();
    *current_is_digit = None;
}

/// Rank of a post-release label (`1.0.0-post1`), the only suffix family that
/// sorts *above* a bare release. Kept as a named constant so `text_rank` and
/// `suffix_is_post_release` cannot drift apart.
const POST_RELEASE_RANK: u8 = 6;

fn text_rank(text: &str) -> u8 {
    // Ordered low -> high. `main` sits below `dev` because the release ladder is
    // main (rawest, rolling) -> dev (release candidate) -> prod: a main build
    // must never be reported as newer than the dev build it is promoted into.
    match text {
        "main" => 0,
        "dev" => 1,
        "a" | "alpha" => 2,
        "b" | "beta" => 3,
        "c" | "pre" | "preview" | "rc" => 4,
        "post" | "r" | "rev" => POST_RELEASE_RANK,
        _ => 5,
    }
}

impl From<io::Error> for UpdateError {
    fn from(error: io::Error) -> Self {
        Self::Network(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uv_tool_managed_path_matches_tool_environment() {
        let tools_dir = Path::new("/Users/test/.local/share/uv/tools");
        let install_path = tools_dir.join("rayline-cli").join("bin").join("rayline");

        assert!(is_uv_tool_managed_path(
            "rayline-cli",
            &install_path,
            tools_dir
        ));
    }

    #[test]
    fn uv_tool_managed_path_rejects_other_installs() {
        let tools_dir = Path::new("/Users/test/.local/share/uv/tools");
        let install_path = Path::new("/usr/local/bin/rayline");

        assert!(!is_uv_tool_managed_path(
            "rayline-cli",
            install_path,
            tools_dir
        ));
    }
}

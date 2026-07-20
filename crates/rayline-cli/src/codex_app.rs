//! `rayline codex app` — launch the Codex **desktop app** routed through Rayline.
//!
//! Mirrors `rayline codex` (same `--config`/`--auth`/`--model` flags, same
//! router-daemon path) but targets the desktop app instead of the CLI. The
//! desktop app-server differs from the CLI in two ways that shape this module:
//!
//! 1. It **ignores `-c` overrides** — it only reads persistent config from its
//!    `CODEX_HOME`. So we materialize the Rayline provider into a `config.toml`
//!    inside an isolated `CODEX_HOME` (`~/.rayline/codex-app-home`) rather than
//!    passing `-c` flags.
//! 2. It is **single-instance per machine**, locked to the `CODEX_HOME` its
//!    app-server first spawned with. A second `codex app` launch just focuses
//!    the existing window. So if an app-server is already running on a different
//!    home, we must prompt the user to quit + relaunch under the Rayline home.

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::codex::{
    CodexAuthMode, EffectiveCodexAuthMode, default_rayline_base_url, rayline_provider_config_toml,
};

const CODEX_APP_HOME_DIR: &str = "codex-app-home";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppRunRequest {
    pub model: Option<String>,
    pub config_path: Option<PathBuf>,
    pub auth_mode: CodexAuthMode,
    /// Extra args passed through to `codex app` (e.g. a workspace path).
    pub codex_args: Vec<OsString>,
    pub root_env_explicit: bool,
}

pub async fn run(request: AppRunRequest) -> ExitCode {
    // 1. Start/ensure the Rayline router — identical path to `rayline codex`.
    let start_request = crate::router::RouterStartCliRequest {
        api_mode: crate::router::ROUTER_API_MODE_CODEX.to_owned(),
        proxy_routing_mode: crate::router::PROXY_ROUTING_MODE_ALL.to_owned(),
        config_path: request.config_path.clone(),
        codex_auth_mode: request.auth_mode,
        root_env_explicit: request.root_env_explicit,
    };
    if let Err(error) = crate::router::start_from_cli(&start_request).await {
        eprintln!("Error: failed to start Rayline Codex router: {error}");
        return ExitCode::from(1);
    }
    eprintln!(
        "Rayline Codex router ready at http://127.0.0.1:{}/v1",
        crate::router::DEFAULT_LOCAL_ROUTER_PORT
    );

    // 2. Compute the config we WOULD write — no filesystem changes yet, so a
    //    declined restart below leaves everything untouched.
    let home = match isolated_home_path() {
        Ok(home) => home,
        Err(error) => {
            eprintln!("Error: failed to resolve Codex app home: {error}");
            return ExitCode::from(1);
        }
    };
    let subscription_auth = request
        .auth_mode
        .effective_for_run(request.config_path.as_ref())
        == EffectiveCodexAuthMode::Subscription;
    let config = generate_config(&request, subscription_auth);

    // 3. Reconcile the single-instance app-server. The comparison basis is the
    //    running app-server's OWN loaded config (its CODEX_HOME/config.toml),
    //    not our isolated file — that's what it actually loaded at startup.
    match reconcile_running_app(&home, &config) {
        ReconcileOutcome::AlreadyCurrent => {
            // Config already matches; don't rewrite. Still launch so `codex app`
            // focuses the window / opens the requested workspace.
            eprintln!("The Codex app is already using Rayline. Bringing it to the front.");
        }
        ReconcileOutcome::Cancelled => {
            eprintln!("Leaving the Codex app as it is.");
            return ExitCode::SUCCESS;
        }
        ReconcileOutcome::Proceed => {
            // No conflicting app, or the user agreed to restart: write config now.
            if let Err(error) = write_isolated_home(&home, &config, subscription_auth) {
                eprintln!("Error: failed to prepare Codex app home: {error}");
                return ExitCode::from(1);
            }
        }
    }

    // 4. Launch (or focus) the Codex app on the isolated home.
    let mut command = Command::new("codex");
    command.env("CODEX_HOME", &home).arg("app");
    command.args(&request.codex_args);
    match command.status() {
        // A missing code means the child was killed by a signal — treat it as
        // failure (matching the Codex CLI path), not silent success.
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to launch codex app: {error}");
            ExitCode::from(127)
        }
    }
}

/// Resolve the user's real Codex home (`$CODEX_HOME` or `~/.codex`), used as the
/// source of `auth.json`.
fn user_codex_home() -> io::Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))
}

/// The stable isolated home Rayline points the desktop app at.
fn isolated_home_path() -> io::Result<PathBuf> {
    let base = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(base.join(crate::DOT_CONFIG_DIR).join(CODEX_APP_HOME_DIR))
}

/// True when our freshly-generated Rayline block is not the prefix of the
/// existing `config.toml`. Codex appends its own sections (marketplaces,
/// mcp_servers, …) *after* our block, so an unchanged block is still a prefix;
/// any change to `--model`/`--auth`/base-url makes it no longer match.
fn generated_config_differs(existing: &str, generated: &str) -> bool {
    !existing.starts_with(generated)
}

/// Render the Rayline `config.toml` block for the desktop app from the run
/// flags. Pure — no filesystem effects, so it can be compared against a running
/// app-server's config before deciding whether to write/restart.
fn generate_config(request: &AppRunRequest, subscription_auth: bool) -> String {
    let model = request
        .model
        .as_deref()
        .unwrap_or(crate::codex::CODEX_DEFAULT_SENTINEL_MODEL);
    let base_url = default_rayline_base_url();
    rayline_provider_config_toml(model, &base_url, subscription_auth)
}

/// Write the isolated `CODEX_HOME`: `config.toml` (our generated block over a
/// preserved suffix) plus an `auth.json` link. Called only after we're committed
/// to launching, so a declined restart never mutates the home.
///
/// `auth.json` is linked only when auth resolves to subscription — a `--auth
/// none` / local-only run must not run the desktop app with the user's Codex
/// credentials, so any existing link is removed instead.
fn write_isolated_home(home: &Path, config: &str, subscription_auth: bool) -> io::Result<()> {
    fs::create_dir_all(home)?;
    let config_path = home.join("config.toml");
    // Determine the suffix to preserve beneath our Rayline block:
    //  - isolated config.toml already exists → keep everything after our block
    //    (the Codex desktop app persists settings there: [desktop], [projects], …),
    //  - first creation → seed from the user's main Codex config.toml if present,
    //  - otherwise → no suffix.
    let suffix = match fs::read_to_string(&config_path) {
        Ok(existing) => suffix_below_rayline_block(&existing),
        Err(_) => user_codex_home()
            .ok()
            .and_then(|h| fs::read_to_string(h.join("config.toml")).ok())
            .map(|seed| preservable_suffix(&seed))
            .unwrap_or_default(),
    };
    fs::write(&config_path, compose_config(config, &suffix))?;
    link_auth_json(home, subscription_auth)
}

/// Join our generated Rayline block with a preserved suffix, separated by a blank
/// line when both are non-empty.
fn compose_config(generated: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return generated.to_owned();
    }
    let sep = if generated.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{generated}{sep}{suffix}")
}

/// Extract the portion of an existing isolated `config.toml` that must be
/// preserved beneath a freshly-generated Rayline block.
///
/// Our generated block ends inside `[model_providers.rayline]`, so the cut point
/// is the first TOML table header that is NOT our own `[model_providers.rayline]`
/// — everything from there on is app-managed / user content, preserved verbatim.
/// (Cutting at the *first* header would wrongly grab our own table.) If there's
/// no such header, the file is only ever our block and there's nothing to keep.
fn suffix_below_rayline_block(existing: &str) -> String {
    match first_foreign_table_header_offset(existing) {
        Some(offset) => existing[offset..].to_owned(),
        None => String::new(),
    }
}

/// The part of a seed config (the user's main `config.toml`) that can be safely
/// preserved beneath our generated block on first creation.
///
/// Our block ends inside `[model_providers.rayline]`, so anything placed after it
/// must start with a table header — bare keys would be captured by that table.
/// A user's main config typically opens with bare keys (`model = …`), which we
/// override anyway, so we keep only from its first table header onward
/// (`[mcp_servers]`, `[projects]`, …). A seed that is empty or all
/// blank/comments preserves nothing.
///
/// Any `[model_providers.rayline]` table the seed already defines is stripped —
/// we always emit our own, and a second definition is a TOML duplicate-table
/// error that would make the isolated config fail to load.
fn preservable_suffix(seed: &str) -> String {
    let kept = if starts_with_table_header_or_blank(seed) {
        // Already opens with a header (or is blank): safe to keep whole.
        seed
    } else {
        // Opens with bare keys: drop them, keep from the first table header on.
        match first_foreign_table_header_offset(seed) {
            Some(offset) => &seed[offset..],
            None => return String::new(),
        }
    };
    strip_rayline_provider_table(kept)
}

/// Remove any `[model_providers.rayline]` table (its header through the line
/// before the next table header / EOF) from `text`. Prevents a duplicate of the
/// table our generated block always emits.
fn strip_rayline_provider_table(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // A new table header ends any table we were skipping.
            skipping = is_rayline_provider_header(trimmed);
        }
        if !skipping {
            out.push_str(line);
        }
    }
    out
}

/// Byte offset of the first TOML table / array-of-tables header line that is not
/// our own `[model_providers.rayline]`. This is the boundary between our block
/// and the preserved app/user suffix. `None` if no such header exists.
fn first_foreign_table_header_offset(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && !is_rayline_provider_header(trimmed) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Whether a header line opens our own `[model_providers.rayline]` table (the
/// only table our generated block emits).
fn is_rayline_provider_header(header_line: &str) -> bool {
    let header = header_line.trim();
    header == "[model_providers.rayline]"
}

/// Whether `text`, after skipping leading blank lines and `#` comments, is empty
/// or begins with a TOML table header. Used to decide if seeded content can be
/// safely prepended below our block.
fn starts_with_table_header_or_blank(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return trimmed.starts_with('[');
    }
    // All blank/comments (or empty) — safe to prepend.
    true
}

/// Reconcile the isolated `<home>/auth.json` for the effective auth mode.
///
/// The isolated `auth.json` can be either a **symlink** we created (points at the
/// user's real one) or a **regular file** the desktop app wrote when the user
/// logged in inside the app. Those must be treated differently so we never delete
/// the user's only credentials:
///
///  - Subscription, source present → (re)link to the source. A prior symlink or
///    file is replaced; the source is the fresher truth.
///  - Subscription, source absent → keep a regular `auth.json` the app wrote
///    (the user's only credentials); only remove a *symlink* (it's stale, pointing
///    nowhere useful).
///  - Auth disabled (`--auth none` / local) → remove any isolated `auth.json`
///    (symlink or file) so the app doesn't run with the user's credentials.
fn link_auth_json(home: &Path, subscription_auth: bool) -> io::Result<()> {
    let link = home.join("auth.json");
    if !subscription_auth {
        return remove_if_present(&link);
    }
    let source = user_codex_home()?.join("auth.json");
    reconcile_auth_link(&source, &link)
}

/// Make `link` reflect `source` for the subscription path:
///  - source exists → replace `link` with a symlink to it;
///  - source absent → keep a regular-file `link` (app-written credentials), but
///    drop a dangling symlink.
fn reconcile_auth_link(source: &Path, link: &Path) -> io::Result<()> {
    let existing = fs::symlink_metadata(link).ok();
    let is_symlink = existing.as_ref().is_some_and(|meta| meta.is_symlink());

    if source.exists() {
        if existing.is_some() {
            fs::remove_file(link)?;
        }
        return symlink_file(source, link);
    }
    // No source: a symlink here is stale → remove it; a real file is the user's
    // desktop-app login → keep it.
    if is_symlink {
        fs::remove_file(link)?;
    }
    Ok(())
}

/// Remove `path` if it exists (file or symlink); no-op if absent.
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn symlink_file(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn symlink_file(source: &Path, link: &Path) -> io::Result<()> {
    // Windows symlinks need privilege; fall back to a copy if the link fails.
    match std::os::windows::fs::symlink_file(source, link) {
        Ok(()) => Ok(()),
        Err(_) => fs::copy(source, link).map(|_| ()),
    }
}

enum ReconcileOutcome {
    /// No conflicting app-server, or the user agreed to restart — safe to write
    /// config and launch.
    Proceed,
    /// An app-server is already running and already has our exact config — just
    /// focus it, write nothing.
    AlreadyCurrent,
    /// A conflicting app-server is running and the user declined to restart it.
    Cancelled,
}

/// Inspect the running Codex app-server (if any) and decide whether to launch.
///
/// A running single-instance app loaded its `config.toml` at startup, so new
/// settings only take effect on restart. The comparison basis is the app's OWN
/// loaded config — the `config.toml` in *its* `CODEX_HOME` (which the app wrote
/// to its home at launch), not our isolated file. Restart when the app is on a
/// different home, or on our home but its loaded config no longer matches what
/// we'd generate; otherwise a rerun with new `--model`/`--auth` would silently
/// keep the old config.
fn reconcile_running_app(home: &Path, config: &str) -> ReconcileOutcome {
    let Some(app) = find_running_app_server() else {
        return ReconcileOutcome::Proceed;
    };
    let same_home = app.codex_home.as_deref().map(Path::new) == Some(home);
    if same_home && running_app_config_matches(&app, config) {
        return ReconcileOutcome::AlreadyCurrent;
    }
    if same_home {
        eprintln!("The Codex app is running with an out-of-date Rayline configuration.");
    } else {
        eprintln!("The Codex app is already running with a different configuration.");
    }
    if !confirm_restart() {
        return ReconcileOutcome::Cancelled;
    }
    if let Err(error) = quit_running_app(&app) {
        eprintln!("Couldn't close the running Codex app: {error}");
        return ReconcileOutcome::Cancelled;
    }
    ReconcileOutcome::Proceed
}

/// Whether the running app-server's loaded config already starts with our
/// generated Rayline block. Reads the `config.toml` in the app's own
/// `CODEX_HOME` — what it actually loaded at startup. Unreadable ⇒ treat as a
/// mismatch (safer to restart than to leave it stale).
fn running_app_config_matches(app: &RunningApp, config: &str) -> bool {
    let Some(home) = app.codex_home.as_deref() else {
        return false;
    };
    match fs::read_to_string(Path::new(home).join("config.toml")) {
        Ok(existing) => !generated_config_differs(&existing, config),
        Err(_) => false,
    }
}

struct RunningApp {
    pid: u32,
    codex_home: Option<String>,
}

/// Find the Codex desktop app-server process and read its `CODEX_HOME`.
///
/// The app-server argv is `.../ChatGPT.app/Contents/Resources/codex app-server`
/// on macOS (`Codex.app` on installs predating the rename) and `codex
/// app-server` elsewhere — distinct from the GUI parent and the renderer/GPU
/// helpers. We key off the `app-server` token, so the bundle name is incidental.
fn find_running_app_server() -> Option<RunningApp> {
    let pid = pgrep_app_server()?;
    Some(RunningApp {
        pid,
        codex_home: read_codex_home_env(pid),
    })
}

/// Find the Codex desktop app-server pid.
///
/// We can't match a fixed `"codex app-server"` substring: Codex injects `-c`
/// flags between the binary and the subcommand (e.g. `codex -c features.x=1
/// app-server`). We also can't rely on `pgrep -l` for the argv — on Linux
/// (procps) `-l` prints only the process name even with `-f`, dropping the
/// `app-server` token. So `pgrep -f` gives us candidate pids, then we read each
/// one's full argv from a portable per-pid source and keep the app-server.
fn pgrep_app_server() -> Option<u32> {
    let output = Command::new("pgrep").arg("-f").arg("codex").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|token| token.parse::<u32>().ok())
        .find(|pid| process_argv(*pid).is_some_and(|argv| is_app_server_argv(&argv)))
}

/// Read a process's full command line. Linux: `/proc/<pid>/cmdline` (NUL-joined
/// argv). macOS: `ps -p <pid> -o command=`. Mirrors `router::process_argv`.
fn process_argv(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let argv: Vec<u8> = raw
            .into_iter()
            .map(|byte| if byte == 0 { b' ' } else { byte })
            .collect();
        Some(String::from_utf8_lossy(&argv).trim().to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let argv = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!argv.is_empty()).then_some(argv)
    }
}

/// True for the app-server process argv. The reliable signal is the
/// `app-server` subcommand token, which the CLI (`codex`, `codex resume`) and
/// the renderer/GPU helpers never carry. Robust to injected `-c` flags between
/// the binary and the subcommand.
fn is_app_server_argv(argv: &str) -> bool {
    argv.split_whitespace().any(|tok| tok == "app-server")
}

/// Read `CODEX_HOME` from a running process's environment.
///
/// Linux: `/proc/<pid>/environ`. macOS: `ps eww` (the `e` flag appends the
/// environment to the command line). Mirrors `claude_daemon::read_process_env`.
fn read_codex_home_env(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
        for entry in raw.split(|byte| *byte == 0) {
            let decoded = String::from_utf8_lossy(entry);
            if let Some(value) = decoded.strip_prefix("CODEX_HOME=") {
                return Some(value.to_owned());
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/bin/ps")
            .arg("eww")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("command=")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&output.stdout);
        line.split_whitespace()
            .find_map(|token| token.strip_prefix("CODEX_HOME=").map(ToOwned::to_owned))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Prompt `[y/N]` on the terminal. Defaults to "no" on non-interactive stdin or
/// any read error, so we never restart the user's app without explicit consent.
fn confirm_restart() -> bool {
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        eprintln!("Not an interactive terminal; leaving the running Codex app as-is.");
        return false;
    }
    eprint!("Would you like to restart it to use Rayline? [y/N] ");
    if io::stderr().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Quit the running Codex desktop app. macOS: `osascript ... quit` (clean app
/// teardown). Elsewhere: SIGTERM the app-server process.
fn quit_running_app(app: &RunningApp) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Ask the app to quit gracefully. If AppleScript can't reach it, fall
        // back to signalling the app-server process directly.
        //
        // Target the bundle id, not the app name: the desktop app ships as
        // `Codex.app` on older installs and `ChatGPT.app` since the rename,
        // but `com.openai.codex` is stable across both. Matching by name would
        // silently miss the renamed bundle and degrade every quit to SIGTERM.
        let quit = Command::new("osascript")
            .arg("-e")
            .arg("tell application id \"com.openai.codex\" to quit")
            .status();
        if !matches!(quit, Ok(status) if status.success()) {
            signal_term(app.pid)?;
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        signal_term(app.pid)?;
    }
    #[cfg(not(unix))]
    {
        let _ = app;
        return Err(io::Error::other(
            "quitting a running Codex app is not supported on this platform",
        ));
    }

    // Wait for the app-server to actually exit before we relaunch. Launching
    // while the single instance is still tearing down races its own app
    // management and can leave the app in a bad state.
    #[cfg(unix)]
    if wait_for_exit() {
        Ok(())
    } else {
        Err(io::Error::other(
            "the Codex app did not close in time; not relaunching",
        ))
    }
    #[cfg(not(unix))]
    Ok(())
}

/// Send `SIGTERM` to a pid.
#[cfg(unix)]
fn signal_term(pid: u32) -> io::Result<()> {
    // SAFETY: sending a signal to a pid is a simple libc call.
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Poll until the app-server is gone (up to ~6s). Returns whether it exited.
#[cfg(unix)]
fn wait_for_exit() -> bool {
    for _ in 0..30 {
        if pgrep_app_server().is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_argv_matches_despite_injected_flags() {
        // Regression: Codex injects `-c` flags between the binary and the
        // subcommand, so a fixed "codex app-server" substring misses it.
        assert!(is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/Resources/codex -c features.code_mode_host=true app-server --analytics-default-enabled"
        ));
        assert!(is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
        // Installs predating the ChatGPT.app rename must keep matching.
        assert!(is_app_server_argv(
            "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
    }

    #[test]
    fn config_unchanged_when_block_is_prefix_even_with_app_sections() {
        let generated = "model = \"rayline-local\"\nmodel_provider = \"rayline\"\n";
        // Codex appends its own sections after our block on a prior launch.
        let existing = format!("{generated}\n[marketplaces.openai-bundled]\nx = 1\n");
        assert!(!generated_config_differs(&existing, generated));
    }

    #[test]
    fn running_app_config_matches_reads_the_apps_own_home() {
        // The comparison basis is the running app-server's CODEX_HOME/config.toml
        // (what it loaded), with the app's appended sections tolerated.
        let dir = unique_tmp_dir("running-match");
        let generated = "model = \"rayline-local\"\nmodel_provider = \"rayline\"\n";
        fs::write(
            dir.join("config.toml"),
            format!("{generated}\n[desktop]\nx = 1\n"),
        )
        .unwrap();
        let app = RunningApp {
            pid: 1,
            codex_home: Some(dir.to_string_lossy().into_owned()),
        };
        assert!(running_app_config_matches(&app, generated));

        // A changed generated block no longer matches.
        assert!(!running_app_config_matches(
            &app,
            "model = \"gpt-5.5\"\nmodel_provider = \"rayline\"\n"
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn running_app_config_mismatch_when_home_unknown_or_no_file() {
        // No CODEX_HOME on the process, or no config.toml → treat as mismatch
        // (restart rather than leave stale).
        let no_home = RunningApp {
            pid: 1,
            codex_home: None,
        };
        assert!(!running_app_config_matches(&no_home, "anything"));

        let dir = unique_tmp_dir("running-nofile");
        let empty = RunningApp {
            pid: 1,
            codex_home: Some(dir.to_string_lossy().into_owned()),
        };
        assert!(!running_app_config_matches(&empty, "anything"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_changed_when_model_or_auth_differs() {
        let old = "model = \"rayline-local\"\nmodel_provider = \"rayline\"\n";
        let existing = format!("{old}\n[mcp_servers.x]\ny = 1\n");
        // Rerun with a different --model regenerates a different block.
        let new = "model = \"gpt-5.5\"\nmodel_provider = \"rayline\"\n";
        assert!(generated_config_differs(&existing, new));
    }

    #[test]
    fn auth_link_removed_when_source_absent() {
        // A prior run linked auth.json; the current home has none. The stale link
        // must be cleared so the isolated app can't reuse old credentials.
        let dir = unique_tmp_dir("auth-absent");
        let link = dir.join("auth.json");
        let stale_source = dir.join("old-auth.json");
        fs::write(&stale_source, "OLD").unwrap();
        symlink_file(&stale_source, &link).unwrap();
        assert!(link.exists());

        let missing_source = dir.join("no-such-auth.json");
        reconcile_auth_link(&missing_source, &link).unwrap();

        assert!(!link.exists(), "stale link must be removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_link_points_at_current_source() {
        let dir = unique_tmp_dir("auth-present");
        let link = dir.join("auth.json");
        let source = dir.join("real-auth.json");
        fs::write(&source, "TOKEN").unwrap();

        reconcile_auth_link(&source, &link).unwrap();

        assert_eq!(fs::read_to_string(&link).unwrap(), "TOKEN");
        assert_eq!(fs::read_link(&link).unwrap(), source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_written_auth_file_kept_when_source_absent() {
        // The user logged in inside the desktop app, which wrote a REAL auth.json
        // in the isolated home. With no source auth (main home never logged in),
        // subscription reconcile must NOT delete it — that's their only credential.
        let dir = unique_tmp_dir("auth-desktop-file");
        let link = dir.join("auth.json");
        fs::write(&link, "DESKTOP_TOKEN").unwrap(); // regular file, not a symlink

        let missing_source = dir.join("no-such-auth.json");
        reconcile_auth_link(&missing_source, &link).unwrap();

        assert_eq!(
            fs::read_to_string(&link).unwrap(),
            "DESKTOP_TOKEN",
            "app-written auth file must be preserved"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_auth_mode_removes_desktop_written_auth_file() {
        // --auth none is an explicit opt-out: even a real app-written auth.json
        // is removed so the app doesn't run with the user's credentials.
        let dir = unique_tmp_dir("auth-none-file");
        let link = dir.join("auth.json");
        fs::write(&link, "DESKTOP_TOKEN").unwrap();

        link_auth_json(&dir, false).unwrap();

        assert!(
            fs::symlink_metadata(&link).is_err(),
            "no-auth must remove even a real auth file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_auth_mode_removes_link_and_creates_none() {
        // --auth none must not expose the user's Codex credentials: an existing
        // link is removed and no new one is created.
        let dir = unique_tmp_dir("auth-none");
        let link = dir.join("auth.json");
        let stale = dir.join("stale-auth.json");
        fs::write(&stale, "OLD").unwrap();
        symlink_file(&stale, &link).unwrap();
        assert!(link.exists());

        link_auth_json(&dir, false).unwrap();

        assert!(
            fs::symlink_metadata(&link).is_err(),
            "auth.json must be absent under no-auth"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rayline-codex-app-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // A representative Rayline generated block (ends inside [model_providers.rayline]).
    const RAYLINE_BLOCK: &str = "model = \"rayline-local\"\nmodel_provider = \"rayline\"\n\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = \"http://127.0.0.1:20811/v1\"\nwire_api = \"responses\"\n";

    #[test]
    fn suffix_preserves_app_sections_below_our_block() {
        // Existing isolated config = our block + app-appended sections.
        let existing = format!(
            "{RAYLINE_BLOCK}\n[desktop]\nx = 1\n\n[projects.\"/w\"]\ntrust = \"trusted\"\n"
        );
        let suffix = suffix_below_rayline_block(&existing);
        assert!(suffix.starts_with("[desktop]"));
        assert!(suffix.contains("[projects.\"/w\"]"));
        // Our own table must NOT be in the suffix (else it would duplicate).
        assert!(!suffix.contains("[model_providers.rayline]"));
    }

    #[test]
    fn rayline_block_replaced_not_duplicated_on_rerun() {
        // Prior isolated file has an OLD block + app section. Regenerate with a
        // different model; compose must yield exactly one rayline block.
        let old = "model = \"OLD\"\nmodel_provider = \"rayline\"\n\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = \"http://127.0.0.1:20811/v1\"\nwire_api = \"responses\"\n\n[desktop]\nx = 1\n";
        let suffix = suffix_below_rayline_block(old);
        let composed = compose_config(RAYLINE_BLOCK, &suffix);
        assert_eq!(composed.matches("[model_providers.rayline]").count(), 1);
        assert!(composed.contains("model = \"rayline-local\""));
        assert!(!composed.contains("model = \"OLD\""));
        assert!(composed.contains("[desktop]"));
    }

    #[test]
    fn seed_from_main_config_keeps_tables_drops_bare_keys() {
        // User's main config.toml: bare keys we override + tables to preserve.
        let main = "model = \"gpt-5\"\nmodel_provider = \"openai\"\n\n[mcp_servers.foo]\ncommand = \"x\"\n";
        let suffix = preservable_suffix(main);
        // Bare keys dropped (we override them); tables kept.
        assert!(!suffix.contains("model = \"gpt-5\""));
        assert!(suffix.starts_with("[mcp_servers.foo]"));
    }

    #[test]
    fn seed_that_is_only_bare_keys_preserves_nothing() {
        let main = "model = \"gpt-5\"\nmodel_provider = \"openai\"\n";
        assert_eq!(preservable_suffix(main), "");
    }

    #[test]
    fn seed_with_existing_rayline_table_is_not_duplicated() {
        // A user who already configured Rayline in their main config: the seed's
        // own [model_providers.rayline] must be stripped so composing with our
        // generated block doesn't produce a duplicate table (invalid TOML).
        let main = "model = \"rayline-local\"\nmodel_provider = \"rayline\"\n\n[model_providers.rayline]\nname = \"User Rayline\"\nbase_url = \"http://127.0.0.1:20811/v1\"\n\n[mcp_servers.foo]\ncommand = \"x\"\n";
        let suffix = preservable_suffix(main);
        assert!(
            !suffix.contains("[model_providers.rayline]"),
            "seed's rayline table must be stripped: {suffix}"
        );
        assert!(suffix.contains("[mcp_servers.foo]"), "other tables kept");

        // The composed file has exactly one rayline table.
        let composed = compose_config(RAYLINE_BLOCK, &suffix);
        assert_eq!(composed.matches("[model_providers.rayline]").count(), 1);
    }

    #[test]
    fn strip_rayline_table_only_removes_that_table() {
        let text = "[a]\nx = 1\n\n[model_providers.rayline]\nname = \"y\"\nbase_url = \"z\"\n\n[b]\nq = 2\n";
        let stripped = strip_rayline_provider_table(text);
        assert!(stripped.contains("[a]"));
        assert!(stripped.contains("[b]"));
        assert!(!stripped.contains("[model_providers.rayline]"));
        assert!(!stripped.contains("base_url = \"z\""));
    }

    #[test]
    fn seed_starting_with_table_is_kept_whole() {
        let main = "[mcp_servers.foo]\ncommand = \"x\"\n";
        assert_eq!(preservable_suffix(main), main);
    }

    #[test]
    fn compose_no_suffix_is_just_the_block() {
        assert_eq!(compose_config(RAYLINE_BLOCK, ""), RAYLINE_BLOCK);
    }

    #[test]
    fn write_isolated_home_first_run_seeds_from_main_then_preserves_on_rerun() {
        // End-to-end over the filesystem: first write with no existing isolated
        // config seeds the app suffix; the app then appends a section; a second
        // write with a changed block replaces the block but keeps both suffixes.
        let dir = unique_tmp_dir("merge-e2e");
        let iso = dir.join("config.toml");

        // First write (no existing file). Simulate the seed by writing the file
        // as if seeded, then have the "app" append its own section.
        fs::write(
            &iso,
            format!("{RAYLINE_BLOCK}\n[mcp_servers.foo]\ncommand = \"x\"\n"),
        )
        .unwrap();
        // App persists a setting afterwards.
        fs::write(
            &iso,
            format!(
                "{}\n[desktop]\nqueue = true\n",
                fs::read_to_string(&iso).unwrap().trim_end()
            ),
        )
        .unwrap();

        // Rerun: regenerate with a different block; preserve everything below.
        let existing = fs::read_to_string(&iso).unwrap();
        let new_block = "model = \"gpt-5.5\"\nmodel_provider = \"rayline\"\n\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = \"http://127.0.0.1:20811/v1\"\nwire_api = \"responses\"\n";
        let composed = compose_config(new_block, &suffix_below_rayline_block(&existing));

        assert_eq!(composed.matches("[model_providers.rayline]").count(), 1);
        assert!(composed.contains("model = \"gpt-5.5\""));
        assert!(composed.contains("[mcp_servers.foo]")); // seeded suffix kept
        assert!(composed.contains("[desktop]")); // app-persisted suffix kept
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_server_argv_rejects_cli_and_helpers() {
        assert!(!is_app_server_argv("codex resume 019f3c3e-fcc9"));
        assert!(!is_app_server_argv("codex"));
        assert!(!is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/Frameworks/Codex Helper --type=renderer"
        ));
        assert!(!is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
        ));
    }

    #[test]
    fn config_toml_points_at_rayline_router() {
        let toml =
            rayline_provider_config_toml("rayline-local", &default_rayline_base_url(), false);
        assert!(toml.contains("model_provider = \"rayline\""));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(toml.contains("127.0.0.1:20811/v1"));
        // No subscription auth fields when not subscription.
        assert!(!toml.contains("requires_openai_auth"));
    }

    #[test]
    fn config_toml_subscription_adds_auth_fields() {
        let toml = rayline_provider_config_toml("gpt-5.5", &default_rayline_base_url(), true);
        assert!(toml.contains("requires_openai_auth = true"));
        assert!(toml.contains("forced_login_method = \"chatgpt\""));
        assert!(toml.contains("model = \"gpt-5.5\""));
    }

    #[test]
    fn isolated_home_is_under_rayline_dir() {
        let home = isolated_home_path().unwrap();
        assert!(home.ends_with(format!("{}/{}", crate::DOT_CONFIG_DIR, CODEX_APP_HOME_DIR)));
    }
}

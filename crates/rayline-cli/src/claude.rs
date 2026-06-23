use std::env;
use std::ffi::OsString;
use std::fs;
#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "macos")]
use std::io::Write;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::thread;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use crate::claude_daemon::{LaunchPreflight, LaunchRecord, PreflightOutcome, RequestSpec};
use serde_json::Value;
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};

const DEFAULT_CLAUDE_SETTINGS_SUFFIX: &str = ".claude/settings.json";
const DEFAULT_MODEL: &str = "rayline-router";
const DEFAULT_PROXY_SUBAGENTS_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_ROUTER_KEY_NAME: &str = "rayline-cli";
const DEFAULT_AUTO_COMPACT_WINDOW: &str = "180000";
const DEFAULT_AUTO_COMPACT_WINDOW_1M: &str = "950000";
const DEFAULT_LOCAL_INJECTOR_PORT: u16 = 20809;
const DEFAULT_PROXY_PORT: u16 = 20810;
/// Distinct proxy port for `--isolated` proxy-mode runs, so an isolated session
/// binds its own proxy instead of restarting the shared proxy a normal session
/// may be using. Override with `RAYLINE_ISOLATED_PROXY_PORT`.
const DEFAULT_ISOLATED_PROXY_PORT: u16 = 20812;
const NODE_CA_BUNDLE_FILENAME: &str = "node-ca-bundle.pem";
pub(crate) const ROUTING_MODE_PROXY: &str = "proxy";
pub(crate) const ROUTING_MODE_PROXY_SUBAGENTS: &str = "proxy-subagents";
const ROUTING_MODE_OVERRIDE: &str = "override";
pub(crate) const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
pub(crate) const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const CLAUDE_DISABLE_AGENT_VIEW_ENV: &str = "CLAUDE_CODE_DISABLE_AGENT_VIEW";
pub(crate) const RAYLINE_ENV_NAME_ENV: &str = "RAYLINE_ENV_NAME";
const DIAG_PROBE_TIMEOUT_SECONDS: u64 = 8;
const LEGACY_STATUSLINE_MARKERS: [&str; 2] = ["wksp-route-statusline", "rl-route-statusline"];
const SHELL_COMPOSE_OPERATORS: [&str; 7] = ["&&", "||", ";", "|", "\n", "`", "$("];
const DIAG_EXTERNAL_PROBE_HOSTS: [&str; 3] = [
    "https://api.anthropic.com/v1/models",
    "https://platform.claude.com/v1/oauth/token",
    "https://claude.ai/",
];
const DIAG_ENV_FINGERPRINT_KEYS: [&str; 24] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "NODE_EXTRA_CA_CERTS",
    "NODE_USE_SYSTEM_CA",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    AUTO_COMPACT_WINDOW_ENV,
    RAYLINE_ENV_NAME_ENV,
    "RAYLINE_CLAUDE_ROUTING_MODE",
    "RAYLINE_ROUTER_URL",
    "RAYLINE_ROUTER_API_KEY",
    "RAYLINE_PROXY_PORT",
    "RAYLINE_UPSTREAM_CA_FILE",
    "RUST_LOG",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    CLAUDE_CONFIG_DIR_ENV,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub args: Vec<OsString>,
    pub model: Option<String>,
    pub local_provider: Option<crate::providers::ProviderId>,
    pub local_provider_model: Option<String>,
    pub auto_compact_window: Option<u64>,
    /// Run entirely through the local static router. This bypasses hosted
    /// router auth/settings and points the local proxy/injector at the
    /// on-device decision plane.
    pub local_router: bool,
    /// Run against an isolated Claude config dir (`~/.<brand>/cc`) so this
    /// session can start its own background-agents supervisor alongside a
    /// standard Claude Code daemon instead of conflicting with it. The dir is a
    /// thin overlay: shared content (projects, sessions, history, skills,
    /// commands, agents, CLAUDE.md) is symlinked back to the user's main
    /// `~/.claude`, settings.json is seeded as a local copy, .claude.json is
    /// mirrored from the selected source profile, and daemon/runtime state stays
    /// local so two live supervisors never collide.
    pub isolated: bool,
    pub local_injector_port: Option<u16>,
    pub routing_mode: RoutingMode,
    /// Whether the user pinned the proxy scope with `--route`. When false and
    /// local routing engages (explicit `--local` or implicit account-local), the
    /// scope falls back to the router-dependent subagents-only default. Carried
    /// here because implicit local engagement is only known at run time, after a
    /// `/v1/settings` fetch the parser cannot perform.
    pub route_scope_explicit: bool,
    pub route_statusline_enabled: bool,
    pub diagnose: bool,
    pub upstream_ca_path: Option<PathBuf>,
    pub router_config_path: Option<PathBuf>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingMode {
    Override,
    Proxy,
    ProxySubagents,
}

pub(crate) fn is_proxy_routing_mode(mode: RoutingMode) -> bool {
    matches!(mode, RoutingMode::Proxy | RoutingMode::ProxySubagents)
}

fn default_model_for_routing_mode(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::ProxySubagents => DEFAULT_PROXY_SUBAGENTS_MODEL,
        RoutingMode::Override | RoutingMode::Proxy => DEFAULT_MODEL,
    }
}

/// Whether *implicit* account-local routing (the hosted `enable_local_router`
/// toggle + an on-device config) should engage for this run.
///
/// Env (`Override`) mode is cloud-only by contract — it sets `ANTHROPIC_BASE_URL`
/// directly and the CLI documents it as unable to reach local inference — so it
/// never engages local even when the account toggle is on. `--isolated` also
/// opts out; an isolated local session must be requested explicitly with
/// `--local`. This gate does not apply to explicit `--local` (handled upstream).
fn implicit_local_engages(mode: RoutingMode, isolated: bool, toggle_on: bool) -> bool {
    !matches!(mode, RoutingMode::Override) && !isolated && toggle_on
}

/// The routing mode after accounting for local engagement.
///
/// When local routing is engaged (explicit `--local` or implicit account-local)
/// and the user did not pin `--route`, the scope defaults to subagents-only —
/// the hybrid default where the main agent stays on cloud Claude and only
/// subagents are offloaded on-device. An explicit `--route` (which makes parse
/// time resolve to `Proxy` for route-all) is always respected.
fn effective_routing_mode(
    parsed: RoutingMode,
    local_engaged: bool,
    route_scope_explicit: bool,
) -> RoutingMode {
    if local_engaged && !route_scope_explicit && parsed == RoutingMode::Proxy {
        RoutingMode::ProxySubagents
    } else {
        parsed
    }
}

impl RunRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

#[derive(Debug)]
pub enum RunError {
    ClaudeMissing,
    HomeNotFound,
    DaemonConflict(String),
    MissingRouterKey(String),
    Router(String),
    UnknownEnvironment(String),
    HostedEnvironment(String),
    Auth(crate::status::AuthTokenError),
    Login(String),
    KeyProvision(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeMissing => formatter.write_str(
                "`claude` not found on PATH. Install Claude Code: https://docs.claude.com/en/docs/claude-code/setup",
            ),
            Self::HomeNotFound => formatter.write_str("home directory not found"),
            Self::DaemonConflict(message) => formatter.write_str(message),
            Self::MissingRouterKey(env_name) => write!(
                formatter,
                "No {} router key stored for {env_name}. Run: {} auth login",
                crate::DISPLAY_NAME,
                crate::CLI_BIN
            ),
            Self::Router(message) => formatter.write_str(message),
            Self::UnknownEnvironment(env_name) => {
                write!(formatter, "Unknown environment for router: {env_name}")
            }
            Self::HostedEnvironment(message) => formatter.write_str(message),
            Self::Auth(error) => error.fmt(formatter),
            Self::Login(message) => formatter.write_str(message),
            Self::KeyProvision(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RunError {}

impl From<crate::status::AuthTokenError> for RunError {
    fn from(error: crate::status::AuthTokenError) -> Self {
        Self::Auth(error)
    }
}

async fn explicit_local_router_start_request(
    env_name: &str,
    request: &RunRequest,
    local_cfg: Option<&crate::local_model::LocalModelConfig>,
    home: &Path,
) -> Result<crate::router::RouterStartRequest, RunError> {
    let injector_port = resolve_injector_port(request.local_injector_port)?;
    let mut start_request =
        crate::router::RouterStartRequest::local_router_defaults(request.root_env_explicit);
    start_request.env_name = Some(env_name.to_owned());
    start_request.injector_port = injector_port;
    start_request.router_config_path = request.router_config_path.clone();

    // Default `--local` (subagents scope) routes only the conservative read-only
    // allowlist; materialize it as the proxy's selective config when the user
    // didn't supply their own. `--route all` (RoutingMode::Proxy) skips this and
    // routes all subagents to local.
    if start_request.router_config_path.is_none()
        && request.routing_mode == RoutingMode::ProxySubagents
    {
        start_request.router_config_path = Some(
            crate::onboarding::write_default_local_routes(home).map_err(|error| {
                RunError::Router(format!("failed to write default local routes: {error}"))
            })?,
        );
    }

    let cfg = local_cfg.filter(|cfg| cfg.is_engageable()).ok_or_else(|| {
        RunError::Router(format!(
            "No local model configured. Run `{} local onboard`.",
            crate::CLI_BIN
        ))
    })?;
    Ok(crate::router::RouterStartRequest::from_local_model(
        cfg,
        start_request,
    ))
}

async fn explicit_provider_config(
    request: &RunRequest,
    home: &Path,
) -> Result<Option<(crate::local_model::LocalModelConfig, PathBuf)>, RunError> {
    let Some(provider) = request.local_provider else {
        return Ok(None);
    };
    if provider == crate::providers::ProviderId::LlamaCpp {
        return Ok(None);
    }

    let endpoint = crate::providers::provider_endpoint(provider).map_err(RunError::Router)?;
    let endpoint = endpoint.ok_or_else(|| {
        RunError::Router(format!("{} has no provider endpoint", provider.label()))
    })?;
    let models = crate::providers::list_models_at(&endpoint)
        .await
        .map_err(|error| provider_unavailable_error(provider, &endpoint.base_url, &error))?;
    let model = resolve_provider_model(
        provider,
        &endpoint.base_url,
        &models,
        request.local_provider_model.as_deref(),
    )?;
    let cfg = crate::local_model::set_provider_endpoint_in_home(
        home,
        provider.as_str(),
        &endpoint.base_url,
        &model,
        "openai_chat",
    )
    .map_err(RunError::Router)?;
    let routes = crate::providers::write_provider_routes(
        home,
        provider,
        &crate::providers::provider_openai_base(&endpoint),
        &model,
    )
    .map_err(|error| RunError::Router(format!("failed to write provider routes: {error}")))?;
    Ok(Some((cfg, routes)))
}

async fn provider_routes_for_config(
    home: &Path,
    cfg: &crate::local_model::LocalModelConfig,
) -> Result<Option<PathBuf>, RunError> {
    let Some(provider) = provider_from_config(cfg) else {
        return Ok(None);
    };
    let base_url = cfg
        .base_url
        .as_deref()
        .ok_or_else(|| RunError::Router("provider config is missing base_url".to_owned()))?;
    let model = cfg
        .model
        .as_deref()
        .ok_or_else(|| RunError::Router("provider config is missing model".to_owned()))?;
    let endpoint = crate::providers::explicit_provider_endpoint(provider, base_url)
        .map_err(RunError::Router)?;
    let models = crate::providers::list_models_at(&endpoint)
        .await
        .map_err(|error| provider_unavailable_error(provider, &endpoint.base_url, &error))?;
    if !models.iter().any(|candidate| candidate.model == model) {
        return Err(RunError::Router(format!(
            "{} is running at {}, but model `{model}` was not listed. Pick another model with `{} claude --local --local-provider {provider_name}`.",
            provider.label(),
            endpoint.base_url,
            crate::CLI_BIN,
            provider_name = provider.as_str(),
        )));
    }
    crate::providers::write_provider_routes(
        home,
        provider,
        &crate::providers::provider_openai_base(&endpoint),
        model,
    )
    .map(Some)
    .map_err(|error| RunError::Router(format!("failed to write provider routes: {error}")))
}

fn provider_from_config(
    cfg: &crate::local_model::LocalModelConfig,
) -> Option<crate::providers::ProviderId> {
    if cfg.protocol.as_deref() != Some("openai_chat") {
        return None;
    }
    match cfg.provider.as_deref() {
        Some("ollama") => Some(crate::providers::ProviderId::Ollama),
        Some("lmstudio") => Some(crate::providers::ProviderId::LmStudio),
        _ => None,
    }
}

fn provider_unavailable_error(
    provider: crate::providers::ProviderId,
    base_url: &str,
    detail: &str,
) -> RunError {
    let hint = provider
        .start_hint()
        .map(|hint| format!(" Start it with: {hint}"))
        .unwrap_or_default();
    RunError::Router(format!(
        "{} isn't running at {base_url}.{hint}\nProbe failed: {detail}",
        provider.label()
    ))
}

fn resolve_provider_model(
    provider: crate::providers::ProviderId,
    base_url: &str,
    models: &[crate::providers::ProviderModel],
    requested: Option<&str>,
) -> Result<String, RunError> {
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        if models.iter().any(|model| model.model == requested) {
            return Ok(requested.to_owned());
        }
        let available = models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(RunError::Router(format!(
            "{} at {base_url} did not list model `{requested}`. Available models: {available}",
            provider.label(),
        )));
    }

    if models.is_empty() {
        return Err(RunError::Router(format!(
            "{} is running at {base_url}, but it did not list any models.",
            provider.label()
        )));
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(RunError::Router(format!(
            "No provider model specified. Run `{} claude --local --local-provider {} --model <MODEL>`.",
            crate::CLI_BIN,
            provider.as_str()
        )));
    }

    eprintln!("{} · {base_url}", provider.label());
    for (index, model) in models.iter().enumerate() {
        let size = model
            .size_bytes
            .map(crate::catalog::format_bytes)
            .unwrap_or_else(|| "external".to_owned());
        eprintln!("  {:>3}  {:<32}  {size}", index + 1, model.model);
    }
    eprint!("Model number or name — Enter to cancel › ");
    io::stderr().flush().ok();
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| RunError::Router(format!("failed to read provider selection: {error}")))?;
    let token = input.trim();
    if token.is_empty() {
        return Err(RunError::Router("No provider model chosen.".to_owned()));
    }
    if let Ok(index) = token.parse::<usize>() {
        if (1..=models.len()).contains(&index) {
            return Ok(models[index - 1].model.clone());
        }
    }
    models
        .iter()
        .find(|model| model.model == token)
        .map(|model| model.model.clone())
        .ok_or_else(|| RunError::Router(format!("Unknown provider model `{token}`.")))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DaemonState {
    pid: u32,
    env_vars: std::collections::BTreeMap<String, String>,
    env_unreadable: bool,
    spawned_by_pid: Option<u32>,
    started_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DaemonOwner {
    Rayline { env_name: String, mode: RoutingMode },
    NonRayline,
}

pub async fn run_command(request: &RunRequest) -> Result<Command, RunError> {
    let home = dirs::home_dir().ok_or(RunError::HomeNotFound)?;
    let claude_bin = find_claude_bin(&home).ok_or(RunError::ClaudeMissing)?;
    run_command_from_home(request, &home, claude_bin).await
}

async fn run_command_from_home(
    request: &RunRequest,
    home: &Path,
    claude_bin: PathBuf,
) -> Result<Command, RunError> {
    let env_name = crate::status::resolve_env(request.env_name.as_deref(), Some(home));
    let hosted = if request.local_router {
        None
    } else {
        Some(
            crate::status::resolve_hosted_environment(&env_name, Some(home))
                .map_err(|error| RunError::HostedEnvironment(error.to_string()))?,
        )
    };
    let router_url = router_url_for_run(hosted.as_ref(), request.local_router)?;
    let key = if request.local_router {
        "rayline-local".to_owned()
    } else {
        ensure_router_key(
            &env_name,
            home,
            request.auth_token.as_deref(),
            request.root_env_explicit,
        )
        .await?
    };

    // Single `/v1/settings` fetch per launch, feeding BOTH the pinned-main-model
    // auto-compact window and the account `enable_local_router` toggle. Skip it
    // entirely when neither is needed: the window is already pinned by flag/env
    // AND this machine has no `local_model` config (so the toggle is moot —
    // even an incomplete config needs the toggle, to decide whether to warn).
    let local_cfg = crate::local_model::read_from_home(home);
    let need_settings =
        !request.local_router && (!auto_compact_window_is_explicit(request) || local_cfg.is_some());
    let settings = if need_settings {
        fetch_router_settings(&env_name, &router_url, request.auth_token.as_deref(), &key).await
    } else {
        None
    };

    // Explicit `--local` branch (request.local_router == true): runs first-run
    // onboarding via `ensure_local_model`, which may download the chosen model.
    // Returns a hard error (`NotConfigured → Err`) when no model is configured
    // so the user gets an actionable message instead of silently falling back.
    //
    // Implicit account-local branch (else): engages when a `local_model` config
    // exists, the account toggle is on, and the configured model is usable. This
    // implicit path stays off under `--isolated` and under env (`Override`) mode,
    // which is cloud-only by contract — see `implicit_local_engages`. Explicit
    // `--local-router --isolated` takes the local path above and uses an isolated
    // proxy sidecar. A failed settings fetch defaults the toggle to false (stay
    // cloud). The model always comes from the config (recommended pick, or the
    // user's custom endpoint); when none is picked yet, the best
    // already-downloaded curated model is adopted as the pick — this implicit
    // path never downloads anything, and an unusable config warns + launches with
    // cloud routing instead of blocking.
    let enable_local_router = settings
        .as_ref()
        .map(enable_local_router_from_router_settings)
        .unwrap_or(false);
    let local_start_request = if request.local_router {
        let provider_config = explicit_provider_config(request, home).await?;
        let (cfg, provider_routes_path) = if let Some((cfg, routes)) = provider_config {
            (cfg, Some(routes))
        } else {
            let cfg = match crate::onboarding::ensure_local_model(home, &env_name)
                .await
                .map_err(|error| RunError::Router(error.to_string()))?
            {
                crate::onboarding::LocalModelReadiness::Ready(cfg) => *cfg,
                crate::onboarding::LocalModelReadiness::NotConfigured => {
                    return Err(RunError::Router(format!(
                        "No local model configured for `{cli} claude --local`. Run `{cli} local onboard` to set one up, or run `{cli} claude` for cloud routing.",
                        cli = crate::CLI_BIN,
                    )));
                }
            };
            let routes = provider_routes_for_config(home, &cfg).await?;
            (cfg, routes)
        };
        let mut start_request =
            explicit_local_router_start_request(&env_name, request, Some(&cfg), home).await?;
        if let Some(path) = provider_routes_path {
            start_request.router_config_path = Some(path);
        }
        Some(start_request)
    } else {
        match local_cfg {
            Some(cfg)
                if implicit_local_engages(
                    request.routing_mode,
                    request.isolated,
                    enable_local_router,
                ) =>
            {
                if provider_from_config(&cfg).is_some() {
                    eprintln!(
                        "Warning: local routing is enabled, but the configured provider endpoint requires `{cli} claude --local`. Continuing with cloud routing.",
                        cli = crate::CLI_BIN,
                    );
                    None
                } else {
                    match resolve_engageable_local_config(home, &env_name, cfg).await {
                        Some(cfg) => {
                            let injector_port = resolve_injector_port(request.local_injector_port)?;
                            let mut start_request = crate::router::RouterStartRequest::defaults(
                                request.root_env_explicit,
                            );
                            start_request.env_name = Some(env_name.clone());
                            start_request.router_url = router_url.to_owned();
                            start_request.router_url_explicit = true;
                            start_request.injector_port = injector_port;
                            Some(crate::router::RouterStartRequest::from_local_model(
                                &cfg,
                                start_request,
                            ))
                        }
                        None => None, // warning already printed; stay cloud
                    }
                }
            }
            _ => None,
        }
    };
    let requested_local_port = local_start_request
        .as_ref()
        .map(|start_request| start_request.injector_port);

    // Correct the parse-time routing mode now that local engagement is known.
    // The parser resolves bare cloud `rayline claude` to route-all `Proxy`, but
    // when account-local routing engages without an explicit `--route` the scope
    // must fall back to the hybrid subagents-only default (main agent on cloud
    // Claude, subagents on-device) — the same default as explicit `--local`.
    // Shadow `request` with the corrected mode so every downstream consumer
    // (model default, proxy wiring, status line) sees a single coherent value.
    let local_engaged = local_start_request.is_some();
    let request = &RunRequest {
        routing_mode: effective_routing_mode(
            request.routing_mode,
            local_engaged,
            request.route_scope_explicit,
        ),
        ..request.clone()
    };

    let inherited_anthropic_model = env::var_os("ANTHROPIC_MODEL").is_some();
    let model = request
        .model
        .clone()
        .or_else(|| env::var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| default_model_for_routing_mode(request.routing_mode).to_owned());
    let set_model_env = should_set_model_env(
        request.routing_mode,
        request.model.is_some(),
        inherited_anthropic_model,
    );
    let auto_compact_window = effective_auto_compact_window(request, settings.as_ref(), &model);

    // `--isolated` (or choosing `[i]` at the conflict prompt) targets a private
    // config dir and, in proxy mode, a private proxy port. Resolve both per the
    // isolation state and inspect the matching config dir for a daemon conflict.
    let mut isolated = request.isolated;
    let mut requested_proxy_port = if is_proxy_routing_mode(request.routing_mode) {
        Some(resolve_proxy_port(isolated)?)
    } else {
        None
    };
    let mut inspect_dir = claude_config_dir(home, isolated);
    let mut daemon_request = RequestSpec {
        env_name: &env_name,
        routing_mode: request.routing_mode,
        auto_compact_window: &auto_compact_window,
        args: &request.args,
        requested_local_port,
        requested_proxy_port,
    };
    let preflight = LaunchPreflight {
        home,
        config_dir: &inspect_dir,
        request: &daemon_request,
        claude_bin: &claude_bin,
        // Do not switch an implicit account-local run into isolation from the
        // conflict prompt. Users who want that shape should request it directly
        // with `--local-router --isolated` so the isolated proxy sidecar is
        // configured deliberately.
        allow_isolated: !isolated && local_start_request.is_none(),
    }
    .resolve()?;
    let preserve_spawned_by_pid = match preflight {
        PreflightOutcome::Proceed(preflight) => preflight.preserve_spawned_by_pid,
        PreflightOutcome::SwitchToIsolated => {
            // The user chose to run isolated: re-resolve the proxy port and re-check
            // the isolated config dir, which may already host a daemon for a
            // different env/mode. No further isolated escape from here.
            isolated = true;
            requested_proxy_port = if is_proxy_routing_mode(request.routing_mode) {
                Some(resolve_proxy_port(true)?)
            } else {
                None
            };
            inspect_dir = claude_config_dir(home, true);
            daemon_request.requested_proxy_port = requested_proxy_port;
            let isolated_preflight = LaunchPreflight {
                home,
                config_dir: &inspect_dir,
                request: &daemon_request,
                claude_bin: &claude_bin,
                allow_isolated: false,
            }
            .resolve()?;
            match isolated_preflight {
                PreflightOutcome::Proceed(preflight) => preflight.preserve_spawned_by_pid,
                PreflightOutcome::SwitchToIsolated => {
                    unreachable!("isolated re-check cannot isolate")
                }
            }
        }
    };
    if request.diagnose {
        diag_print_preamble(&env_name, &router_url, request.routing_mode, home).await;
    }

    let mut command = Command::new(&claude_bin);
    add_claude_bin_dir_to_child_path(&mut command, &claude_bin);
    let args = if request.diagnose {
        inject_claude_debug(&request.args)
    } else {
        request.args.clone()
    };
    command.args(args);
    // Apply the isolated overlay (and CLAUDE_CONFIG_DIR) before proxy/statusline
    // config so those target the isolated settings.json and proxy, not the shared
    // ones under ~/.claude.
    if isolated {
        apply_isolated_config_dir(&mut command, home, &claude_bin);
    }
    match request.routing_mode {
        RoutingMode::Override => {
            configure_override_env(
                &mut command,
                local_start_request.as_ref(),
                home,
                &env_name,
                router_url.as_str(),
                &key,
                &model,
                &auto_compact_window,
            )
            .await?;
        }
        RoutingMode::Proxy | RoutingMode::ProxySubagents => {
            // A prior local session may have left the shared `rld serve` daemon
            // running with its model loaded — wasting RAM/GPU and holding the
            // shared proxy port this non-isolated cloud launch is about to
            // rebind (`configure_proxy_env` → `start_proxy_from_home`). Stop it
            // so the replacement proxy can take the port, but only when the
            // account toggle is *confirmed* off: `settings.is_some()` rules out
            // a failed `/v1/settings` fetch (and the not-engageable config
            // case), where the server gate may still be on and an existing
            // session is legitimately routing local. Gated to non-isolated
            // proxy mode — `--isolated` uses a private port and override mode
            // starts no proxy, so neither replaces the embedded proxy. The
            // helper further restricts the stop to a daemon that actually owns
            // this proxy port. Placed after the conflict check commits, so a
            // cancelled launch never tears the daemon down without a successor;
            // and we preflight the daemon binary the replacement proxy needs
            // (`resolve_rld_bin`) so a launch that could not start its own proxy
            // never performs the destructive stop and strands the session.
            let toggle_confirmed_off = settings.is_some() && !enable_local_router;
            if local_start_request.is_none()
                && !isolated
                && toggle_confirmed_off
                && crate::router::resolve_rld_bin(home).is_ok()
            {
                let proxy_port = resolve_proxy_port(false)?;
                match crate::router::stop_serve_daemon_from_home(home, proxy_port).await {
                    Ok(true) => eprintln!("Local routing is off — stopped the on-device model."),
                    Ok(false) => {}
                    // Best-effort: a failed teardown must not block the launch.
                    Err(error) => {
                        eprintln!("Warning: could not stop the on-device model: {error}.")
                    }
                }
            }
            configure_proxy_env(
                &mut command,
                request,
                local_start_request.as_ref(),
                home,
                &env_name,
                router_url.as_str(),
                &key,
                set_model_env.then_some(model.as_str()),
                &auto_compact_window,
                isolated,
            )
            .await?;
            configure_route_statusline(home, isolated, request.route_statusline_enabled);
        }
    }
    if request.diagnose {
        diag_print_postamble_for_mode(request.routing_mode, &router_url, isolated, home).await;
    }
    if let Some(caller_cwd) = env::var_os("RAYLINE_CALLER_CWD") {
        let caller_cwd = PathBuf::from(caller_cwd);
        if caller_cwd.is_dir() {
            command.current_dir(caller_cwd);
        }
    }

    crate::claude_daemon::record_rayline_claude_launch(LaunchRecord {
        home,
        pid: std::process::id(),
        request: &daemon_request,
        preserve_spawned_by_pid,
    });
    Ok(command)
}

fn router_url_for_run(
    hosted: Option<&crate::status::HostedEnvironment>,
    local_router: bool,
) -> Result<String, RunError> {
    if local_router {
        return Ok(format!(
            "http://127.0.0.1:{}",
            crate::router::DEFAULT_LOCAL_ROUTER_PORT
        ));
    }
    hosted
        .map(|hosted| hosted.router_url.clone())
        .ok_or_else(|| RunError::UnknownEnvironment("missing hosted environment".to_owned()))
}

/// Resolve the router key for `env_name`, provisioning it on demand.
///
/// Walks the onboarding ladder so a bare `claude` run is all a new user needs:
/// if no key is stored we mint one, signing in first
/// when the user is not yet authenticated (the `auth login` step). Interactive
/// sign-in only fires on a real terminal; non-interactive callers keep the
/// previous hard error instead of blocking on a browser.
async fn ensure_router_key(
    env_name: &str,
    home: &Path,
    auth_token: Option<&str>,
    root_env_explicit: bool,
) -> Result<String, RunError> {
    if let Some(token) = auth_token.filter(|token| !token.is_empty()) {
        eprintln!("Provisioning {} router key...", crate::DISPLAY_NAME);
        return crate::status::provision_router_key(env_name, home, DEFAULT_ROUTER_KEY_NAME, token)
            .await
            .map_err(|error| RunError::KeyProvision(error.to_string()));
    }

    if let Some(key) = crate::status::load_claude_key_from_home(env_name, home) {
        return Ok(key);
    }

    let Some(token) = resolve_auth_token_or_login(env_name, auth_token, root_env_explicit).await?
    else {
        return Err(RunError::MissingRouterKey(env_name.to_owned()));
    };

    eprintln!("Provisioning {} router key...", crate::DISPLAY_NAME);
    crate::status::provision_router_key(env_name, home, DEFAULT_ROUTER_KEY_NAME, &token)
        .await
        .map_err(|error| RunError::KeyProvision(error.to_string()))
}

/// Return an account bearer token for `env_name`, launching the interactive
/// sign-in flow when no usable credentials are stored. Returns `Ok(None)` when
/// no token is available and we cannot prompt (non-interactive session), so the
/// caller can fall back to the existing "not logged in" error.
async fn resolve_auth_token_or_login(
    env_name: &str,
    auth_token: Option<&str>,
    root_env_explicit: bool,
) -> Result<Option<String>, RunError> {
    let token_request = crate::status::AuthTokenRequest {
        env_name: Some(env_name.to_owned()),
        auth_token: auth_token.map(ToOwned::to_owned),
        root_env_explicit,
    };

    // Auto sign-in only makes sense when we can actually drive a browser/device
    // prompt, so require both stdin and stdout to be a terminal. Print-mode
    // pipelines (`claude -p ... | cmd`) keep the previous non-interactive error.
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();

    match crate::status::resolve_auth_token(&token_request).await {
        Ok(crate::status::AuthTokenOutcome::Available(token)) => return Ok(Some(token)),
        // Credentials are missing or stale (expired/revoked refresh token). On a
        // terminal, re-running sign-in repairs both; otherwise preserve the prior
        // behavior (missing-key error, or surfacing the refresh failure).
        Ok(crate::status::AuthTokenOutcome::NotLoggedIn) if !interactive => {
            return Ok(None);
        }
        Ok(crate::status::AuthTokenOutcome::NotLoggedIn) => {}
        Err(error) if !interactive => return Err(error.into()),
        Err(_) => {}
    }

    eprintln!("Signing in to {}...", crate::DISPLAY_NAME);
    let login_request = crate::status::AuthLoginRequest {
        env_name: Some(env_name.to_owned()),
        root_env_explicit,
        no_browser: false,
        paste: false,
    };
    let message = crate::status::auth_login(&login_request)
        .await
        .map_err(|error| RunError::Login(error.to_string()))?;
    crate::status::write_auth_message(&message)
        .map_err(|error| RunError::Login(format!("failed to write login output: {error}")))?;

    match crate::status::resolve_auth_token(&token_request).await? {
        crate::status::AuthTokenOutcome::Available(token) => Ok(Some(token)),
        crate::status::AuthTokenOutcome::NotLoggedIn => Ok(None),
    }
}

/// Start the on-device router for a local-router launch, surfacing its progress.
///
/// `run_command_from_home` otherwise discards the status string `start_from_home`
/// returns, so the terminal stays silent for the multi-second-to-minute wait
/// while the local model loads. We print a heads-up before the blocking start and
/// the router's own status afterward, so a slow launch reads as "working", not
/// "hung".
async fn start_local_router(
    home: &Path,
    start_request: &crate::router::RouterStartRequest,
) -> Result<(), RunError> {
    eprintln!(
        "Starting on-device model (first response can take a minute or two while it loads and reads your prompt).\nRouter progress: tail -f {}",
        crate::router::local_router_log_path(home).display()
    );
    if start_request.enable_proxy {
        eprintln!(
            "Proxy routing decisions: tail -f {}",
            crate::router::proxy_log_path(home, false).display()
        );
    }
    let status = crate::router::start_from_home(home, start_request)
        .await
        .map_err(|error| RunError::Router(error.to_string()))?;
    eprint!("{status}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn configure_override_env(
    command: &mut Command,
    local_start_request: Option<&crate::router::RouterStartRequest>,
    home: &Path,
    env_name: &str,
    router_url: &str,
    key: &str,
    model: &str,
    auto_compact_window: &str,
) -> Result<(), RunError> {
    if let Some(start_request) = local_start_request {
        let injector_port = start_request.injector_port;
        start_local_router(home, start_request).await?;
        command.env(
            "ANTHROPIC_BASE_URL",
            format!("http://127.0.0.1:{injector_port}"),
        );
    } else {
        command.env("ANTHROPIC_BASE_URL", router_url);
    }
    command.env("ANTHROPIC_AUTH_TOKEN", key);
    command.env("ANTHROPIC_MODEL", model);
    command.env(AUTO_COMPACT_WINDOW_ENV, auto_compact_window);
    command.env(RAYLINE_ENV_NAME_ENV, env_name);
    command.env_remove("ANTHROPIC_API_KEY");
    command.env_remove("RAYLINE_CLAUDE_ROUTING_MODE");
    command.env_remove("RAYLINE_ROUTER_URL");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn configure_proxy_env(
    command: &mut Command,
    request: &RunRequest,
    local_start_request: Option<&crate::router::RouterStartRequest>,
    home: &Path,
    env_name: &str,
    router_url: &str,
    key: &str,
    model: Option<&str>,
    auto_compact_window: &str,
    isolated: bool,
) -> Result<(), RunError> {
    let proxy_port = resolve_proxy_port(isolated)?;
    let proxy_routing_mode = proxy_routing_mode_name(request.routing_mode);
    if let Some(start_request) = local_start_request {
        if isolated {
            let mut start_request = start_request.clone();
            start_request.enable_proxy = false;
            start_local_router(home, &start_request).await?;
            eprintln!(
                "Proxy routing decisions: tail -f {}",
                crate::router::proxy_log_path(home, true).display()
            );
            let status = crate::router::start_local_proxy_from_home(
                home,
                &crate::router::LocalProxyStartRequest {
                    router_url: start_request.router_url.clone(),
                    proxy_port,
                    proxy_routing_mode: proxy_routing_mode.to_owned(),
                    router_config_path: start_request.router_config_path.clone(),
                    local_model_id: start_request.local_model_id.clone(),
                    adapter_port: start_request.adapter_port,
                    custom_mode: start_request.upstream_url.is_some(),
                    force_restart: request.diagnose,
                    diagnose: request.diagnose,
                    upstream_ca_path: request.upstream_ca_path.clone(),
                    isolated: true,
                },
            )
            .await
            .map_err(|error| RunError::Router(error.to_string()))?;
            eprint!("{status}");
        } else {
            // The engaged local daemon also binds the proxy port in proxy mode.
            // Clone the config-derived request and enable its embedded proxy.
            let mut start_request = start_request.clone();
            start_request.enable_proxy = true;
            start_request.proxy_port = proxy_port;
            start_request.proxy_routing_mode = proxy_routing_mode.to_owned();
            start_local_router(home, &start_request).await?;
        }
    } else {
        crate::router::start_proxy_from_home(
            home,
            router_url,
            key,
            proxy_port,
            proxy_routing_mode,
            request.diagnose,
            request.diagnose,
            request.upstream_ca_path.as_deref(),
            isolated,
        )
        .await
        .map_err(|error| RunError::Router(error.to_string()))?;
    }

    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    command.env("HTTPS_PROXY", &proxy_url);
    command.env("https_proxy", &proxy_url);
    command.env("NODE_EXTRA_CA_CERTS", node_ca_bundle_value(home, request));
    if env::var_os("NODE_USE_SYSTEM_CA").is_none() {
        command.env("NODE_USE_SYSTEM_CA", "1");
    }
    if let Some(model) = model {
        command.env("ANTHROPIC_MODEL", model);
    }
    command.env(AUTO_COMPACT_WINDOW_ENV, auto_compact_window);
    command.env(
        "RAYLINE_CLAUDE_ROUTING_MODE",
        routing_mode_name(request.routing_mode),
    );
    command.env(RAYLINE_ENV_NAME_ENV, env_name);
    command.env("RAYLINE_ROUTER_URL", router_url);
    let existing_no_proxy = [env::var("NO_PROXY").ok(), env::var("no_proxy").ok()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");
    let no_proxy = append_no_proxy(&existing_no_proxy, &["localhost", "127.0.0.1", "::1"]);
    command.env("NO_PROXY", &no_proxy);
    command.env("no_proxy", no_proxy);
    configure_proxy_auth_env(command, request.routing_mode);
    Ok(())
}

fn should_set_model_env(
    routing_mode: RoutingMode,
    request_model_explicit: bool,
    inherited_anthropic_model: bool,
) -> bool {
    routing_mode != RoutingMode::ProxySubagents
        || request_model_explicit
        || inherited_anthropic_model
}

fn configure_proxy_auth_env(command: &mut Command, routing_mode: RoutingMode) {
    if routing_mode == RoutingMode::ProxySubagents {
        command.env_remove("ANTHROPIC_BASE_URL");
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
        command.env_remove("ANTHROPIC_API_KEY");
    } else {
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
        command.env_remove("ANTHROPIC_BASE_URL");
        command.env_remove("ANTHROPIC_API_KEY");
    }
    command.env_remove("RAYLINE_ROUTER_API_KEY");
}

fn resolve_injector_port(explicit: Option<u16>) -> Result<u16, RunError> {
    if let Some(port) = explicit {
        return Ok(port);
    }
    match env::var("RAYLINE_INJECTOR_PORT") {
        Ok(value) if !value.is_empty() => value.parse::<u16>().map_err(|_| {
            RunError::Router(format!(
                "RAYLINE_INJECTOR_PORT must be an integer, got {value:?}"
            ))
        }),
        _ => Ok(DEFAULT_LOCAL_INJECTOR_PORT),
    }
}

fn resolve_proxy_port(isolated: bool) -> Result<u16, RunError> {
    let (env_var, default_port) = if isolated {
        ("RAYLINE_ISOLATED_PROXY_PORT", DEFAULT_ISOLATED_PROXY_PORT)
    } else {
        ("RAYLINE_PROXY_PORT", DEFAULT_PROXY_PORT)
    };
    match env::var(env_var) {
        Ok(value) if !value.is_empty() => value
            .parse::<u16>()
            .map_err(|_| RunError::Router(format!("{env_var} must be an integer, got {value:?}"))),
        _ => Ok(default_port),
    }
}

/// Resolve the auto-compact window, preferring an explicit flag/env, then the
/// pinned main-model value from the (already-fetched) router settings, then a
/// model-aware default. Pure: the single `/v1/settings` fetch happens in
/// `run_command_from_home` and is threaded in here.
fn effective_auto_compact_window(
    request: &RunRequest,
    settings: Option<&Value>,
    model: &str,
) -> String {
    if let Some(window) = request.auto_compact_window {
        return window.to_string();
    }
    if let Ok(value) = env::var(AUTO_COMPACT_WINDOW_ENV) {
        if !value.is_empty() {
            return value;
        }
    }
    if let Some(window) = settings.and_then(auto_compact_window_from_router_settings) {
        return window;
    }
    default_auto_compact_window(model).to_owned()
}

/// True when the explicit flag/env already pins the auto-compact window, so the
/// settings fetch is not needed for that purpose.
fn auto_compact_window_is_explicit(request: &RunRequest) -> bool {
    if request.auto_compact_window.is_some() {
        return true;
    }
    matches!(env::var(AUTO_COMPACT_WINDOW_ENV), Ok(value) if !value.is_empty())
}

fn default_auto_compact_window(model: &str) -> &'static str {
    if model.trim_end().ends_with("[1m]") {
        DEFAULT_AUTO_COMPACT_WINDOW_1M
    } else {
        DEFAULT_AUTO_COMPACT_WINDOW
    }
}

fn positive_int_string(value: &Value) -> Option<String> {
    if let Some(number) = value.as_u64().filter(|number| *number > 0) {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_i64().filter(|number| *number > 0) {
        return Some(number.to_string());
    }
    let number = value.as_f64()?;
    if number.is_finite() && number > 0.0 && number.fract() == 0.0 {
        return Some(format!("{number:.0}"));
    }
    None
}

fn auto_compact_window_from_router_settings(result: &Value) -> Option<String> {
    result
        .get("settings")?
        .get("rules")?
        .get("main_model")?
        .get("autoCompactWindow")
        .and_then(positive_int_string)
}

/// Read the account-level `enable_local_router` toggle from a `GET /v1/settings`
/// response. Mirrors `auto_compact_window_from_router_settings` so both values
/// come from one fetch. Defaults to `false` (stay cloud) when the field is
/// absent or not a bool — the safe default the design mandates.
fn enable_local_router_from_router_settings(result: &Value) -> bool {
    result
        .get("settings")
        .and_then(|settings| settings.get("enable_local_router"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Fetch the caller's router settings once per launch. Prefer account credentials
/// when available, but fall back to the already-provisioned router key because
/// the router accepts either bearer form. `None` on request failure or an error
/// payload, so callers fall back to safe defaults.
async fn fetch_router_settings(
    env_name: &str,
    router_base: &str,
    auth_token: Option<&str>,
    router_key: &str,
) -> Option<Value> {
    let token_request = crate::status::AuthTokenRequest {
        env_name: Some(env_name.to_owned()),
        // Honor an explicit `--auth-token` first; when account credentials are
        // absent or expired, the stored router key below can still read settings.
        auth_token: auth_token.map(ToOwned::to_owned),
        root_env_explicit: false,
    };
    let bearer_token = match crate::status::resolve_auth_token(&token_request).await {
        Ok(crate::status::AuthTokenOutcome::Available(token)) => token,
        Ok(crate::status::AuthTokenOutcome::NotLoggedIn) => router_key.to_owned(),
        Err(_) => router_key.to_owned(),
    };
    let url = format!("{router_base}/v1/settings");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?
        .get(url)
        .bearer_auth(bearer_token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    if body.get("error").is_some() {
        return None;
    }
    Some(body)
}

/// Resolve a present `local_model` config into one that can actually engage,
/// or print a warning and return `None` so the launch continues with cloud
/// routing (the plan's "warn before invoking claude" — never block, never
/// download):
/// - Custom complete → engage as-is; incomplete → warn.
/// - Recommended with a pick → engage when its GGUF is downloaded, else warn.
/// - Recommended without a pick (e.g. legacy `mode: "auto"`) → adopt the best
///   already-downloaded curated model and persist it as the pick (so the
///   other Rayline clients and `local show` reflect it); nothing downloaded or catalog
///   unreachable → warn.
async fn resolve_engageable_local_config(
    home: &Path,
    env_name: &str,
    cfg: crate::local_model::LocalModelConfig,
) -> Option<crate::local_model::LocalModelConfig> {
    let cli = crate::CLI_BIN;
    match cfg.mode {
        crate::local_model::LocalModelMode::Custom => {
            if cfg.is_engageable() {
                return Some(cfg);
            }
            eprintln!(
                "Warning: local routing is enabled, but the custom endpoint is incomplete. Continuing with cloud routing. Set it with `{cli} local custom --url <URL> --model <NAME>`."
            );
            None
        }
        crate::local_model::LocalModelMode::Recommended => {
            if cfg.has_recommended_pick() {
                if crate::router::hf_cache_has_verified_config_gguf(home, &cfg) {
                    return Some(cfg);
                }
                let model_id = cfg
                    .model_id
                    .as_deref()
                    .or(cfg.model_file.as_deref())
                    .unwrap_or("selected model");
                eprintln!(
                    "Warning: local routing is enabled, but the local model `{model_id}` is not downloaded. Continuing with cloud routing. Download it with `{cli} local download {model_id}`."
                );
                return None;
            }
            match crate::catalog::auto_select_downloaded(env_name).await {
                Some(model) => {
                    eprintln!(
                        "No local model selected — using downloaded `{id}` (saved as your selection; change with `{cli} local use <model-id>`).",
                        id = model.id,
                    );
                    match crate::local_model::set_recommended_in_home(home, &model) {
                        Ok(cfg) => Some(cfg),
                        Err(error) => {
                            // Engage anyway: the pick is valid, only the
                            // write-back failed — it will be retried next run.
                            eprintln!(
                                "Warning: could not save the local model selection: {error}."
                            );
                            Some(crate::local_model::LocalModelConfig {
                                mode: crate::local_model::LocalModelMode::Recommended,
                                provider: Some("llamacpp".to_owned()),
                                protocol: Some("anthropic_messages".to_owned()),
                                base_url: cfg.base_url,
                                model: cfg.model,
                                model_id: Some(model.id),
                                model_repo: Some(model.repo),
                                model_file: Some(model.filename),
                                model_revision: Some(model.revision),
                                model_sha256: Some(model.sha256),
                                custom_endpoints: cfg.custom_endpoints,
                            })
                        }
                    }
                }
                // Nothing downloaded — a sole saved custom endpoint is the
                // only added model, so select and use it.
                None => match cfg.custom_endpoints.as_slice() {
                    [endpoint] => {
                        eprintln!(
                            "No local model selected — using your saved custom endpoint `{model}` ({url}).",
                            model = endpoint.model,
                            url = endpoint.base_url,
                        );
                        match crate::local_model::activate_custom_endpoint_in_home(home, endpoint) {
                            Ok(cfg) => Some(cfg),
                            Err(error) => {
                                eprintln!(
                                    "Warning: could not save the local model selection: {error}. Continuing with cloud routing."
                                );
                                None
                            }
                        }
                    }
                    _ => {
                        eprintln!(
                            "Warning: local routing is enabled, but no local model is selected and none is downloaded. Continuing with cloud routing. Pick one with `{cli} local use <model-id>` (see `{cli} local models`)."
                        );
                        None
                    }
                },
            }
        }
    }
}

fn append_no_proxy(existing: &str, required: &[&str]) -> String {
    let mut values = existing
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut seen = values
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    for value in required {
        if seen.insert(value.to_ascii_lowercase()) {
            values.push((*value).to_owned());
        }
    }
    values.join(",")
}

fn node_ca_bundle_value(home: &Path, request: &RunRequest) -> String {
    let proxy_ca_path = crate::router::default_proxy_ca_cert_path(home);
    let existing_node_ca = env::var_os("NODE_EXTRA_CA_CERTS").map(PathBuf::from);
    let bundle_path = proxy_ca_path.parent().map_or_else(
        || PathBuf::from(NODE_CA_BUNDLE_FILENAME),
        |parent| parent.join(NODE_CA_BUNDLE_FILENAME),
    );
    let mut extra = Vec::new();
    if let Some(existing_node_ca) = existing_node_ca {
        if existing_node_ca != proxy_ca_path && existing_node_ca != bundle_path {
            extra.push(existing_node_ca);
        }
    }
    if let Some(upstream_ca_path) = request.upstream_ca_path.as_ref() {
        if upstream_ca_path != &proxy_ca_path && upstream_ca_path != &bundle_path {
            extra.push(upstream_ca_path.clone());
        }
    }
    if extra.is_empty() {
        return proxy_ca_path.display().to_string();
    }

    let mut sections = Vec::new();
    for source in std::iter::once(proxy_ca_path.as_path()).chain(extra.iter().map(PathBuf::as_path))
    {
        let Ok(contents) = std::fs::read_to_string(source) else {
            continue;
        };
        let contents = contents.trim();
        if !contents.is_empty() {
            sections.push(contents.to_owned());
        }
    }
    if sections.is_empty() {
        return proxy_ca_path.display().to_string();
    }
    if let Some(parent) = bundle_path.parent() {
        if std::fs::create_dir_all(parent).is_ok()
            && std::fs::write(&bundle_path, sections.join("\n") + "\n").is_ok()
        {
            return bundle_path.display().to_string();
        }
    }
    proxy_ca_path.display().to_string()
}

fn inject_claude_debug(args: &[OsString]) -> Vec<OsString> {
    if args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "--debug" || arg.starts_with("--debug="))
    }) {
        return args.to_vec();
    }
    std::iter::once(OsString::from("--debug"))
        .chain(args.iter().cloned())
        .collect()
}

pub(crate) fn claude_agent_view_disabled() -> bool {
    env::var(CLAUDE_DISABLE_AGENT_VIEW_ENV).is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        )
    })
}

/// Resolve the Claude Code config dir this run targets. In isolated mode that is
/// the brand's private dir (`~/.<brand>/cc`); otherwise it honors an externally
/// set `CLAUDE_CONFIG_DIR`, falling back to the standard `~/.claude`.
fn claude_config_dir(home: &Path, isolated: bool) -> PathBuf {
    if isolated {
        return isolated_cc_dir(home);
    }
    env::var_os(CLAUDE_CONFIG_DIR_ENV)
        .map(PathBuf::from)
        .map(|path| expand_user_path(path, Some(home)))
        .unwrap_or_else(|| home.join(".claude"))
}

/// The brand-private Claude config dir used by `--isolated` (e.g. `~/.rayline/cc`).
pub(crate) fn isolated_cc_dir(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR).join("cc")
}

/// Write-target directories Claude creates session work under. Created in the
/// shared root if missing so the symlink always points at the shared copy, even
/// on a fresh `~/.claude`: otherwise Claude would create them locally under the
/// isolated config and isolated-mode work would never show up in standard runs.
/// Daemon/runtime state (daemon.lock, daemon/, jobs/, tasks/, teams/, todos/) is
/// deliberately absent so it stays local and the supervisors never collide.
const ISOLATED_SHARED_DIRS: [&str; 4] = ["projects", "sessions", "plans", "paste-cache"];

/// Write-target files: created empty in the shared root if missing, then
/// symlinked, so isolated and standard runs append to the same file.
const ISOLATED_SHARED_FILES: [&str; 1] = ["history.jsonl"];

/// Read-only customization shared via symlink only when present in the shared
/// root. Claude does not create these, so a missing entry cannot diverge; we
/// avoid materializing empty `CLAUDE.md`/statusline/skills entries the user
/// never had.
const ISOLATED_OPTIONAL_ENTRIES: [&str; 7] = [
    "skills",
    "plugins",
    "commands",
    "agents",
    "CLAUDE.md",
    "hooks",
    "statusline-worktree.js",
];

/// Per-profile config dir files seeded as independent copies so they can
/// diverge. `.claude.json` is seeded
/// separately because it lives outside the config dir by default (see
/// [`global_claude_json_path`]).
const ISOLATED_SEED_FILES: [&str; 1] = ["settings.json"];

/// Lay down the isolated overlay and point Claude Code at it. Idempotent: safe
/// to call on every run.
fn apply_isolated_config_dir(command: &mut Command, home: &Path, claude_bin: &Path) {
    let source_dir = claude_config_dir(home, false);
    let isolated_dir = isolated_cc_dir(home);
    ensure_isolated_overlay(&source_dir, &isolated_dir);
    // `.claude.json` (global app state, project-trust, OAuth, personal MCP) lives
    // at $HOME/.claude.json by default, NOT inside ~/.claude. Keep the isolated
    // profile local by default so a `/login` done under `--isolated` survives
    // future launches and the launcher never triggers macOS keychain prompts.
    //
    // Developers who explicitly want to clone the source Claude profile can opt
    // in, but that path may prompt for keychain access on macOS.
    let source_global_config = global_claude_json_path(home);
    let isolated_global_config = isolated_dir.join(".claude.json");
    if should_sync_claude_keychain_credentials() {
        refresh_local_copy(&source_global_config, &isolated_global_config);
        mirror_claude_code_keychain_credentials(&source_dir, &isolated_dir, claude_bin);
    } else {
        seed_local_copy(&source_global_config, &isolated_global_config);
    }
    command.env(CLAUDE_CONFIG_DIR_ENV, &isolated_dir);
}

fn should_sync_claude_keychain_credentials() -> bool {
    matches!(
        env::var("RAYLINE_SYNC_CLAUDE_KEYCHAIN")
            .or_else(|_| env::var("RAYLINE_SYNC_CLAUDE_KEYCHAIN"))
            .ok()
            .as_deref(),
        Some("1" | "true" | "yes")
    )
}

/// Path of Claude Code's global `.claude.json` (app state, project-trust, OAuth,
/// personal MCP servers). It lives inside `CLAUDE_CONFIG_DIR` when that is set,
/// otherwise at `$HOME/.claude.json` (NOT inside `~/.claude`).
fn global_claude_json_path(home: &Path) -> PathBuf {
    match env::var_os(CLAUDE_CONFIG_DIR_ENV) {
        Some(dir) => expand_user_path(PathBuf::from(dir), Some(home)).join(".claude.json"),
        None => home.join(".claude.json"),
    }
}

/// Make `isolated_dir` a thin overlay on `shared_root` (the user's main Claude
/// config dir): symlink shared content back to the shared root and seed
/// settings.json / .claude.json as local copies. Best-effort throughout; a
/// failed link or copy just means that entry is not shared, never a failed
/// launch.
fn ensure_isolated_overlay(shared_root: &Path, isolated_dir: &Path) {
    if shared_root == isolated_dir {
        // The shared root already is the isolated dir (e.g. the user pointed
        // CLAUDE_CONFIG_DIR at it); there is nothing to overlay onto itself.
        return;
    }
    if fs::create_dir_all(isolated_dir).is_err() {
        return;
    }
    for dir in ISOLATED_SHARED_DIRS {
        let source = shared_root.join(dir);
        // Create the shared target first so the symlink always resolves to it,
        // even on a brand-new ~/.claude.
        let _ = fs::create_dir_all(&source);
        link_shared_entry(&source, &isolated_dir.join(dir));
    }
    for file in ISOLATED_SHARED_FILES {
        let source = shared_root.join(file);
        if !source.exists() {
            let _ = fs::write(&source, b"");
        }
        link_shared_entry(&source, &isolated_dir.join(file));
    }
    for entry in ISOLATED_OPTIONAL_ENTRIES {
        link_shared_entry(&shared_root.join(entry), &isolated_dir.join(entry));
    }
    for file in ISOLATED_SEED_FILES {
        seed_local_copy(&shared_root.join(file), &isolated_dir.join(file));
    }
}

/// Symlink `link` -> `source` when the source exists and the slot is still
/// empty. An existing file or symlink is left untouched (respect the user).
fn link_shared_entry(source: &Path, link: &Path) {
    if !source.exists() {
        return;
    }
    if fs::symlink_metadata(link).is_ok() {
        return;
    }
    let _ = make_symlink(source, link);
}

/// Copy `source` -> `dest` once, when the destination does not yet exist. These
/// files hold credentials, so keep the copy (and its parent dir) user-private.
fn seed_local_copy(source: &Path, dest: &Path) {
    if fs::symlink_metadata(dest).is_ok() {
        return;
    }
    copy_private_file(source, dest);
}

/// Copy `source` -> `dest`, replacing any existing file/symlink. Used for
/// selected-source auth state that must not remain pinned to a stale isolated
/// profile.
fn refresh_local_copy(source: &Path, dest: &Path) {
    if same_path(source, dest) {
        return;
    }
    if !source.is_file() {
        return;
    }
    if fs::symlink_metadata(dest).is_ok() && fs::remove_file(dest).is_err() {
        return;
    }
    copy_private_file(source, dest);
}

fn copy_private_file(source: &Path, dest: &Path) {
    if !source.is_file() {
        return;
    }
    if fs::copy(source, dest).is_ok() {
        set_user_private_permissions(dest);
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(target_os = "macos")]
fn mirror_claude_code_keychain_credentials(
    source_config_dir: &Path,
    isolated_dir: &Path,
    claude_bin: &Path,
) {
    if same_path(source_config_dir, isolated_dir) {
        return;
    }
    let source_service = claude_code_keychain_service(source_config_dir);
    let dest_service = claude_code_keychain_service(isolated_dir);
    if source_service == dest_service {
        return;
    }
    let Some(account) = find_keychain_account(&source_service) else {
        return;
    };
    let Some(secret) = read_keychain_secret(&source_service, &account) else {
        return;
    };
    if read_keychain_secret(&dest_service, &account).as_deref() == Some(secret.as_str()) {
        return;
    }
    write_keychain_secret(&dest_service, &account, &secret, claude_bin);
}

#[cfg(not(target_os = "macos"))]
fn mirror_claude_code_keychain_credentials(
    _source_config_dir: &Path,
    _isolated_dir: &Path,
    _claude_bin: &Path,
) {
}

#[cfg(target_os = "macos")]
fn claude_code_keychain_service(config_dir: &Path) -> String {
    let normalized = fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    let digest = Sha256::digest(normalized.to_string_lossy().as_bytes());
    format!(
        "Claude Code-credentials-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[cfg(target_os = "macos")]
fn find_keychain_account(service: &str) -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_keychain_account(std::str::from_utf8(&output.stdout).ok()?)
}

#[cfg(target_os = "macos")]
fn parse_keychain_account(output: &str) -> Option<String> {
    const PREFIX: &str = "\"acct\"<blob>=\"";
    for line in output.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix(PREFIX) else {
            continue;
        };
        let Some((account, _)) = value.split_once('"') else {
            continue;
        };
        if !account.is_empty() {
            return Some(account.to_owned());
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn read_keychain_secret(service: &str, account: &str) -> Option<String> {
    let child = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let (status, stdout) = wait_for_child_stdout_with_timeout(child, Duration::from_secs(5))
        .ok()
        .flatten()?;
    if !status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&stdout)
            .trim_end_matches(['\r', '\n'])
            .to_owned(),
    )
    .filter(|secret| !secret.is_empty())
}

#[cfg(target_os = "macos")]
fn write_keychain_secret(service: &str, account: &str, secret: &str, claude_bin: &Path) {
    let args = add_generic_password_args(service, account, claude_bin);
    let mut child = match Command::new("security")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };

    if let Some(mut stdin) = child.stdin.take() {
        // `security add-generic-password -w` prompt mode asks for the password
        // twice. Feeding it via stdin keeps OAuth material out of process args.
        let _ = writeln!(stdin, "{secret}");
        let _ = writeln!(stdin, "{secret}");
    }
    let _ = wait_for_child_with_timeout(child, Duration::from_secs(5));
}

#[cfg(target_os = "macos")]
fn add_generic_password_args(service: &str, account: &str, claude_bin: &Path) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("add-generic-password"),
        OsString::from("-U"),
        OsString::from("-s"),
        OsString::from(service),
        OsString::from("-a"),
        OsString::from(account),
    ];
    if claude_bin.is_file() {
        args.push(OsString::from("-T"));
        args.push(claude_bin.as_os_str().to_os_string());
    }
    // Keep this last: with no inline value, `security` prompts on stdin.
    args.push(OsString::from("-w"));
    args
}

#[cfg(target_os = "macos")]
fn wait_for_child_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> io::Result<()> {
    let start = Instant::now();
    loop {
        if let Some(_status) = child.try_wait()? {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn wait_for_child_stdout_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> io::Result<Option<(std::process::ExitStatus, Vec<u8>)>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_end(&mut stdout)?;
            }
            return Ok(Some((status, stdout)));
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn make_symlink(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn make_symlink(source: &Path, link: &Path) -> io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, link)
    } else {
        std::os::windows::fs::symlink_file(source, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_source: &Path, _link: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn routing_mode_name(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::Override => ROUTING_MODE_OVERRIDE,
        RoutingMode::Proxy => ROUTING_MODE_PROXY,
        RoutingMode::ProxySubagents => ROUTING_MODE_PROXY_SUBAGENTS,
    }
}

fn proxy_routing_mode_name(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::Proxy => crate::router::PROXY_ROUTING_MODE_ALL,
        RoutingMode::ProxySubagents => crate::router::PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS,
        // `Override` starts no proxy at all (it sets ANTHROPIC_BASE_URL directly),
        // so it never reaches this proxy-only translation — see the dispatch match
        // in `run` where `Override` takes the `configure_override_env` branch.
        RoutingMode::Override => unreachable!("override mode does not start a proxy"),
    }
}

async fn diag_print_preamble(
    env_name: &str,
    router_url: &str,
    routing_mode: RoutingMode,
    home: &Path,
) {
    eprintln!();
    eprintln!("================================================================");
    eprintln!("{} claude - DIAGNOSTIC MODE", crate::CLI_BIN);
    eprintln!("================================================================");

    diag_print_section("Versions");
    eprintln!("  platform     : {} {}", env::consts::OS, env::consts::ARCH);
    eprintln!("  {:<12} : {}", crate::CLI_BIN, crate::RAYLINE_VERSION);
    match find_claude_bin(home) {
        Some(claude_bin) => {
            let version = Command::new(&claude_bin)
                .arg("--version")
                .output()
                .ok()
                .map(|output| {
                    if output.stdout.is_empty() {
                        String::from_utf8_lossy(&output.stderr).trim().to_owned()
                    } else {
                        String::from_utf8_lossy(&output.stdout).trim().to_owned()
                    }
                })
                .filter(|version| !version.is_empty())
                .unwrap_or_else(|| "<probe error>".to_owned());
            eprintln!("  claude       : {version} ({})", claude_bin.display());
        }
        None => eprintln!("  claude       : NOT FOUND on PATH"),
    }
    eprintln!("  env          : {env_name}");
    eprintln!("  routing-mode : {}", routing_mode_name(routing_mode));
    eprintln!("  router-url   : {router_url}");

    diag_print_section("Relevant env vars (current shell)");
    let mut any_set = false;
    for key in DIAG_ENV_FINGERPRINT_KEYS {
        if let Ok(value) = env::var(key) {
            any_set = true;
            eprintln!("  {key:<28} = {}", diag_redact(key, &value));
        }
    }
    if !any_set {
        eprintln!("  (none of the proxy/claude env vars are set)");
    }

    diag_print_section("Stored credentials");
    let credentials = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join("credentials.json");
    let proxy_ca = crate::router::default_proxy_ca_cert_path(home);
    eprintln!(
        "  ~/.config/{}/credentials.json : {}",
        crate::CONFIG_DIR,
        if credentials.is_file() {
            "present"
        } else {
            "missing"
        }
    );
    eprintln!(
        "  proxy CA cert ({}) : {}",
        proxy_ca.display(),
        if proxy_ca.is_file() {
            "present"
        } else {
            "missing"
        }
    );

    diag_print_section("Network reachability - DIRECT (no proxy)");
    for url in diag_probe_hosts(router_url) {
        let result = diag_probe(&url, None, None).await;
        eprintln!("  HEAD {url:<48} -> {result}");
    }
}

async fn diag_print_postamble_for_mode(
    routing_mode: RoutingMode,
    router_url: &str,
    isolated: bool,
    home: &Path,
) {
    match routing_mode {
        RoutingMode::Proxy | RoutingMode::ProxySubagents => {
            let default_port = if isolated {
                DEFAULT_ISOLATED_PROXY_PORT
            } else {
                DEFAULT_PROXY_PORT
            };
            let proxy_port = resolve_proxy_port(isolated).unwrap_or(default_port);
            diag_print_postamble(proxy_port, router_url, isolated, home).await;
        }
        RoutingMode::Override => {
            diag_print_section("Logs to send back");
            eprintln!(
                "  --no-proxy / routing-mode=override: no {} proxy involved (calls go direct to ANTHROPIC_BASE_URL).",
                crate::DAEMON_BIN
            );
            eprintln!("  Capture the session with `script` so we see Claude --debug output:");
            eprintln!("    script -q ~/{}-diagnose-session.log \\", crate::CLI_BIN);
            eprintln!("      {} claude --no-proxy --diagnose", crate::CLI_BIN);
            eprintln!("================================================================");
            eprintln!();
        }
    }
}

async fn diag_print_postamble(proxy_port: u16, router_url: &str, isolated: bool, home: &Path) {
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let proxy_ca = crate::router::default_proxy_ca_cert_path(home);
    let verify_path = proxy_ca.is_file().then_some(proxy_ca.as_path());
    diag_print_section(&format!(
        "Network reachability - VIA {} proxy ({proxy_url})",
        crate::DAEMON_BIN
    ));
    for url in diag_probe_hosts(router_url) {
        if url.trim_end_matches('/') == router_url.trim_end_matches('/') {
            continue;
        }
        let result = diag_probe(&url, Some(&proxy_url), verify_path).await;
        eprintln!("  HEAD {url:<48} -> {result}");
    }
    diag_print_section("Logs to send back");
    eprintln!(
        "  proxy log : {}",
        crate::router::proxy_log_path(home, isolated).display()
    );
    eprintln!("  claude    : stderr below (--debug enabled)");
    eprintln!();
    eprintln!("  Tip: re-run with `script` to capture the full session:");
    eprintln!("    script -q ~/{}-diagnose-session.log \\", crate::CLI_BIN);
    eprintln!(
        "      {} claude --routing-mode proxy --diagnose",
        crate::CLI_BIN
    );
    eprintln!("================================================================");
    eprintln!();
}

fn diag_print_section(title: &str) {
    eprintln!();
    eprintln!(
        "-- {title} {}",
        "-".repeat(60usize.saturating_sub(title.len() + 4))
    );
}

fn diag_redact(name: &str, value: &str) -> String {
    if matches!(
        name,
        "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY" | "RAYLINE_ROUTER_API_KEY"
    ) {
        let prefix = if value.len() > 6 { &value[..6] } else { "" };
        return format!("<set, len={}, prefix={prefix:?}>", value.len());
    }
    value.to_owned()
}

async fn diag_probe(url: &str, via_proxy: Option<&str>, verify_path: Option<&Path>) -> String {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(DIAG_PROBE_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(proxy_url) = via_proxy {
        match reqwest::Proxy::all(proxy_url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(error) => return format!("ERR Proxy: {error}"),
        }
    }
    if let Some(path) = verify_path {
        match fs::read(path)
            .ok()
            .and_then(|pem| reqwest::Certificate::from_pem(&pem).ok())
        {
            Some(cert) => builder = builder.add_root_certificate(cert),
            None => return format!("ERR Certificate: could not read {}", path.display()),
        }
    }
    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => return format!("ERR Client: {error}"),
    };
    match client.head(url).send().await {
        Ok(response) => format!("HTTP {}", response.status().as_u16()),
        Err(error) => format!("ERR {}: {error}", error_kind(&error)),
    }
}

fn error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "Connect"
    } else if error.is_request() {
        "Request"
    } else if error.is_decode() {
        "Decode"
    } else {
        "Http"
    }
}

fn diag_probe_hosts(router_url: &str) -> Vec<String> {
    let mut hosts = DIAG_EXTERNAL_PROBE_HOSTS
        .iter()
        .map(|url| (*url).to_owned())
        .collect::<Vec<_>>();
    hosts.push(router_url.to_owned());
    hosts
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StatuslineInstallResult {
    settings_path: PathBuf,
    installed: bool,
    conflict: bool,
    existing_command: Option<String>,
}

fn configure_route_statusline(home: &Path, isolated: bool, enabled: bool) {
    // Isolated sessions read their own settings.json, so the route-statusline
    // entry must be written there, not into the shared ~/.claude/settings.json.
    let settings_path = if isolated {
        isolated_cc_dir(home).join("settings.json")
    } else {
        default_settings_path(Some(home))
    };
    if !enabled {
        let _ = uninstall_statusline_settings_from_path(settings_path, Some(home));
        return;
    }

    let Ok(rld_bin) = crate::router::resolve_rld_bin(home) else {
        return;
    };
    let Ok(result) = install_statusline_settings_from_path(settings_path, Some(home), &rld_bin)
    else {
        return;
    };
    if result.conflict {
        let existing = result
            .existing_command
            .as_deref()
            .unwrap_or("your custom status line");
        eprintln!(
            "{}: keeping your existing Claude Code status line ({existing}); not installing the router-transparency status line. To show the router's picked model, pipe the session JSON through `{}` statusline from your own status line, or pass --no-statusline to silence this.",
            crate::CLI_BIN,
            shell_quote(&rld_bin.display().to_string())
        );
    }
}

fn install_statusline_settings_from_path(
    settings_path: PathBuf,
    home: Option<&Path>,
    rld_bin: &Path,
) -> io::Result<StatuslineInstallResult> {
    let settings_path = expand_user_path(settings_path, home);
    let mut settings = read_json_object(&settings_path);
    if let Some(existing) = settings.get("statusLine") {
        if existing.is_object() && !is_our_statusline(existing) {
            let existing_command = existing
                .get("command")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            return Ok(StatuslineInstallResult {
                settings_path,
                installed: false,
                conflict: true,
                existing_command,
            });
        }
    }

    settings
        .as_object_mut()
        .expect("read_json_object always returns a JSON object")
        .insert(
            "statusLine".to_owned(),
            serde_json::json!({
                "type": "command",
                "command": statusline_command(rld_bin),
                "padding": 0,
            }),
        );
    write_json_pretty(&settings_path, &settings)?;
    Ok(StatuslineInstallResult {
        settings_path,
        installed: true,
        conflict: false,
        existing_command: None,
    })
}

fn uninstall_statusline_settings_from_path(
    settings_path: PathBuf,
    home: Option<&Path>,
) -> io::Result<PathBuf> {
    let settings_path = expand_user_path(settings_path, home);
    if !settings_path.exists() {
        return Ok(settings_path);
    }
    let mut settings = read_json_object(&settings_path);
    if is_our_statusline(settings.get("statusLine").unwrap_or(&Value::Null)) {
        settings
            .as_object_mut()
            .expect("read_json_object always returns a JSON object")
            .remove("statusLine");
        write_json_pretty(&settings_path, &settings)?;
    }
    Ok(settings_path)
}

fn statusline_command(rld_bin: &Path) -> String {
    let binary = rld_bin.display().to_string();
    #[cfg(target_os = "windows")]
    let binary = binary.replace('\\', "/");
    format!("{} statusline", shell_quote(&binary))
}

fn is_our_statusline(status_line: &Value) -> bool {
    let Some(command) = status_line
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
    else {
        return false;
    };
    if SHELL_COMPOSE_OPERATORS
        .iter()
        .any(|operator| command.contains(operator))
    {
        return false;
    }
    let is_rld_statusline = split_shell_words(command).is_some_and(|tokens| {
        if tokens.len() != 2 || tokens[1] != "statusline" {
            return false;
        }
        let Some(name) = Path::new(&tokens[0])
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return false;
        };
        let branded_windows_name = format!("{}.exe", crate::DAEMON_BIN);
        name == crate::DAEMON_BIN
            || name == branded_windows_name
            || name == "rld"
            || name == "rld.exe"
    });
    is_rld_statusline
        || LEGACY_STATUSLINE_MARKERS
            .iter()
            .any(|marker| command.contains(marker))
}

fn split_shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\\' if !in_single => {
                current.push(chars.next()?);
            }
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if in_single || in_double {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| resolve_executable_in_dir(&dir, name))
}

/// Resolve `name` to an executable inside `dir`, honoring Windows extensions.
///
/// On Windows an executable on PATH is stored with an extension (claude.exe
/// from the native installer, claude.cmd from npm) while callers pass the bare
/// stem, so we resolve it the way the shell does. The extension probe only runs
/// when `name` has no extension of its own, leaving explicit names unchanged;
/// non-Windows behavior is a plain existence check as before.
fn resolve_executable_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidate = dir.join(name);
    if candidate.exists() {
        return Some(candidate);
    }
    #[cfg(windows)]
    if Path::new(name).extension().is_none() {
        for ext in ["exe", "cmd", "bat", "com"] {
            let candidate = dir.join(format!("{name}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve the `claude` binary, falling back to the canonical installer
/// locations when it is absent from `PATH`.
///
/// Menu-bar launches run `claude` through a non-interactive login shell
/// (`zsh -lc`), which sources `.zprofile`/`.zlogin` but not `.zshrc`. Both the
/// native installer (`~/.local/bin`) and the legacy local installer
/// (`~/.claude/local`) wire `claude` onto `PATH` from `.zshrc`, so a PATH-only
/// lookup misses it there even though an interactive terminal finds it fine.
/// Probing the install dirs makes resolution independent of shell-init quirks.
fn find_claude_bin(home: &Path) -> Option<PathBuf> {
    find_on_path("claude").or_else(|| first_existing_file(claude_fallback_candidates(home)))
}

fn claude_fallback_candidates(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
    ]
}

fn first_existing_file(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn add_claude_bin_dir_to_child_path(command: &mut Command, claude_bin: &Path) {
    let Some(claude_dir) = claude_bin.parent() else {
        return;
    };
    let mut paths = env::var_os("PATH")
        .map(|raw| env::split_paths(&raw).collect::<Vec<_>>())
        .unwrap_or_default();
    if !paths.iter().any(|path| path == claude_dir) {
        paths.insert(0, claude_dir.to_owned());
    }
    if let Ok(joined) = env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

fn read_json_object(path: &Path) -> Value {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Value::Object(serde_json::Map::new());
    };
    serde_json::from_str::<Value>(&contents)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

fn write_json_pretty(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned()) + "\n";
    std::fs::write(path, contents)
}

fn set_user_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn default_settings_path(home: Option<&Path>) -> PathBuf {
    std::env::var_os("CLAUDE_SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_home_path(home, DEFAULT_CLAUDE_SETTINGS_SUFFIX))
}

fn default_home_path(home: Option<&Path>, suffix: &str) -> PathBuf {
    home.map_or_else(|| PathBuf::from(suffix), |home| home.join(suffix))
}

fn expand_user_path(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    if raw == "~" {
        return home.map_or(path, Path::to_path_buf);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map_or(path.clone(), |home| home.join(rest));
    }
    path
}

#[cfg(test)]
mod proxy_routing_mode_name_tests {
    use super::*;

    #[test]
    fn proxy_maps_to_all() {
        assert_eq!(
            proxy_routing_mode_name(RoutingMode::Proxy),
            crate::router::PROXY_ROUTING_MODE_ALL
        );
    }

    #[test]
    fn proxy_subagents_maps_to_selective_subagents() {
        assert_eq!(
            proxy_routing_mode_name(RoutingMode::ProxySubagents),
            crate::router::PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS
        );
    }
}

#[cfg(test)]
mod implicit_local_routing_tests {
    use super::*;

    // ── Finding 2: env (Override) is cloud-only and never engages local ──

    #[test]
    fn env_mode_never_engages_implicit_local() {
        // Even with the account toggle on and no isolation, env mode stays cloud.
        assert!(!implicit_local_engages(RoutingMode::Override, false, true));
    }

    #[test]
    fn proxy_mode_engages_implicit_local_when_toggle_on() {
        assert!(implicit_local_engages(RoutingMode::Proxy, false, true));
    }

    #[test]
    fn isolation_blocks_implicit_local() {
        assert!(!implicit_local_engages(RoutingMode::Proxy, true, true));
    }

    #[test]
    fn toggle_off_blocks_implicit_local() {
        assert!(!implicit_local_engages(RoutingMode::Proxy, false, false));
    }

    // ── Finding 1: local engagement defaults to subagents-only ──

    #[test]
    fn implicit_local_without_explicit_route_becomes_subagents() {
        // Parse time resolved cloud + route-all to Proxy; engaging local without
        // an explicit --route must mirror the explicit --local subagents default.
        assert_eq!(
            effective_routing_mode(RoutingMode::Proxy, true, false),
            RoutingMode::ProxySubagents
        );
    }

    #[test]
    fn implicit_local_with_explicit_route_all_is_respected() {
        assert_eq!(
            effective_routing_mode(RoutingMode::Proxy, true, true),
            RoutingMode::Proxy
        );
    }

    #[test]
    fn cloud_proxy_is_unchanged_without_local() {
        assert_eq!(
            effective_routing_mode(RoutingMode::Proxy, false, false),
            RoutingMode::Proxy
        );
    }

    #[test]
    fn explicit_local_subagents_is_unchanged() {
        // Explicit --local already resolved to ProxySubagents at parse time.
        assert_eq!(
            effective_routing_mode(RoutingMode::ProxySubagents, true, false),
            RoutingMode::ProxySubagents
        );
    }
}

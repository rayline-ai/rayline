use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::json;

const CODEX_SUBSCRIPTION_CONFIG_FILENAME: &str = "codex-subscription-router.json";
pub const CODEX_SUBSCRIPTION_ENDPOINT_ID: &str = "codex-subscription";
pub const CODEX_SUBSCRIPTION_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CODEX_SUBSCRIPTION_DEFAULT_MODEL: &str = "gpt-5.4";
/// The internal virtual-marker model Codex is pointed at when the user picks no
/// `--model`. Codex must send *some* `model` on every Responses request, so this
/// sentinel stands in for "no explicit model — let the router's config decide"
/// (mirrors Claude Code's `rayline-router`). The local router recognizes it as a
/// marker and applies main/subagent routing rather than treating it as a real
/// model. Users select a real model (e.g. `gpt-5.5`) via `--model` instead.
pub const CODEX_DEFAULT_SENTINEL_MODEL: &str = "rayline-local";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexAuthMode {
    Auto,
    None,
    Subscription,
}

impl CodexAuthMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "none" | "no-auth" | "local" => Some(Self::None),
            "subscription" | "codex-subscription" | "chatgpt" | "codex" => Some(Self::Subscription),
            _ => None,
        }
    }

    pub fn effective_for_run(self, config_path: Option<&PathBuf>) -> EffectiveCodexAuthMode {
        match self {
            // `auto` with a `--config`: the config drives auth. If its `main`
            // routes to the `subscription` sentinel, resolve to subscription so
            // the ChatGPT backend is materialized — otherwise the sentinel has no
            // concrete endpoint and 502-loops on a placeholder address. Any other
            // main (local/provider/cloud) needs no client auth.
            Self::Auto => match config_path {
                Some(path) if crate::router_config::config_main_is_passthrough(path) => {
                    EffectiveCodexAuthMode::Subscription
                }
                Some(_) => EffectiveCodexAuthMode::None,
                None => EffectiveCodexAuthMode::Subscription,
            },
            Self::Subscription => EffectiveCodexAuthMode::Subscription,
            Self::None => EffectiveCodexAuthMode::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveCodexAuthMode {
    None,
    Subscription,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// The `--model` the user picked, or `None` when they picked none. `None`
    /// means "route by config": the CLI stamps the internal virtual-marker
    /// sentinel ([`CODEX_DEFAULT_SENTINEL_MODEL`]) so the local router applies
    /// main/subagent routing. A `Some(real_model)` is passed through verbatim and
    /// resolved by the router's direct-model routing.
    pub model: Option<String>,
    pub config_path: Option<PathBuf>,
    pub auth_mode: CodexAuthMode,
    pub codex_args: Vec<OsString>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigureRequest {
    /// See [`RunRequest::model`]. `None` writes the virtual-marker sentinel.
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub auth_mode: CodexAuthMode,
}

pub async fn run(request: RunRequest) -> ExitCode {
    let start_request = crate::router::RouterStartCliRequest {
        api_mode: crate::router::ROUTER_API_MODE_CODEX.to_owned(),
        proxy_routing_mode: crate::router::PROXY_ROUTING_MODE_ALL.to_owned(),
        config_path: request.config_path.clone(),
        codex_auth_mode: request.auth_mode,
        root_env_explicit: request.root_env_explicit,
    };
    match crate::router::start_from_cli(&start_request).await {
        Ok(_) => {
            eprintln!(
                "Rayline Codex router ready at http://127.0.0.1:{}/v1",
                crate::router::DEFAULT_LOCAL_ROUTER_PORT
            );
        }
        Err(error) => {
            eprintln!("Error: failed to start Rayline Codex router: {error}");
            return ExitCode::from(1);
        }
    }

    let base_url = format!(
        "http://127.0.0.1:{}/v1",
        crate::router::DEFAULT_LOCAL_ROUTER_PORT
    );
    let model = request
        .model
        .as_deref()
        .unwrap_or(CODEX_DEFAULT_SENTINEL_MODEL);
    let mut command = Command::new("codex");
    command
        .arg("-c")
        .arg("model_provider=\"rayline\"")
        .arg("-c")
        .arg(format!("model={}", toml_string(model)))
        .arg("-c")
        .arg("model_providers.rayline.name=\"Rayline Local\"")
        .arg("-c")
        .arg(format!(
            "model_providers.rayline.base_url={}",
            toml_string(&base_url)
        ))
        .arg("-c")
        .arg("model_providers.rayline.wire_api=\"responses\"");
    if request
        .auth_mode
        .effective_for_run(request.config_path.as_ref())
        == EffectiveCodexAuthMode::Subscription
    {
        command
            .arg("-c")
            .arg("model_providers.rayline.requires_openai_auth=true")
            .arg("-c")
            .arg("forced_login_method=\"chatgpt\"");
        if let Some(version) = codex_cli_version_header() {
            command.arg("-c").arg(format!(
                "model_providers.rayline.http_headers.version={}",
                toml_string(&version)
            ));
        }
    }
    command.args(request.codex_args);

    exec_or_status(&mut command)
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The default Rayline OpenAI Responses base URL (the local router's `/v1`).
pub fn default_rayline_base_url() -> String {
    format!(
        "http://127.0.0.1:{}/v1",
        crate::router::DEFAULT_LOCAL_ROUTER_PORT
    )
}

/// Render the `[model_providers.rayline]` TOML block that points Codex at the
/// Rayline local router.
///
/// This is the single source of truth for the Codex-side provider config,
/// shared by `rayline codex configure` (writes it as `$CODEX_HOME/
/// rayline.config.toml`) and `rayline codex app` (writes it as the default
/// `config.toml` inside an isolated `CODEX_HOME`, since the desktop app-server
/// ignores `-c` overrides and only reads persistent config).
pub fn rayline_provider_config_toml(
    model: &str,
    base_url: &str,
    subscription_auth: bool,
) -> String {
    let http_headers = if subscription_auth {
        codex_cli_version_header()
            .map(|version| format!("http_headers = {{ version = {} }}\n", toml_string(&version)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!(
        "model = {}\nmodel_provider = \"rayline\"\n{}\
\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = {}\nwire_api = \"responses\"\n{}{}",
        toml_string(model),
        if subscription_auth {
            "forced_login_method = \"chatgpt\"\n"
        } else {
            ""
        },
        toml_string(base_url),
        if subscription_auth {
            "requires_openai_auth = true\n"
        } else {
            ""
        },
        http_headers,
    )
}

pub fn configure(request: &ConfigureRequest) -> io::Result<String> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    fs::create_dir_all(&codex_home)?;
    let profile_path = codex_home.join("rayline.config.toml");
    let base_url = request
        .base_url
        .clone()
        .unwrap_or_else(default_rayline_base_url);
    let subscription_auth =
        request.auth_mode.effective_for_run(None) == EffectiveCodexAuthMode::Subscription;
    let model = request
        .model
        .as_deref()
        .unwrap_or(CODEX_DEFAULT_SENTINEL_MODEL);
    let contents = rayline_provider_config_toml(model, &base_url, subscription_auth);
    fs::write(&profile_path, contents)?;
    Ok(format!(
        "Wrote Codex Rayline profile: {}\nStart Rayline with `rayline router start --mode codex --auth {}`, then use Codex profile `rayline`.\nBase URL: {base_url}\n",
        profile_path.display(),
        if subscription_auth {
            "subscription"
        } else {
            "none"
        }
    ))
}

pub fn write_subscription_router_config(home: &Path, subagents_local: bool) -> io::Result<PathBuf> {
    let path = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join(CODEX_SUBSCRIPTION_CONFIG_FILENAME);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&subscription_router_config_json(subagents_local))
        .map_err(io::Error::other)?;
    fs::write(&path, body)?;
    Ok(path)
}

/// The zero-config `rayline codex --auth subscription` router config.
///
/// `main` always routes to the ChatGPT subscription. When `subagents_local` is
/// true (a usable on-device model is available), `subagent` routes to the local
/// adapter — the hybrid default that mirrors `rayline claude`'s "main on cloud,
/// subagents on-device" shape. When false (no local model), subagents stay on
/// the subscription, preserving the zero-setup all-subscription behavior.
///
/// The `model_routes` sentinels stay pinned to the subscription: they resolve
/// the sentinel `--model` on **main** turns. Subagent turns skip these sentinel
/// model_routes and follow `routes.subagent` (see `select_route` in the local
/// router), which is what makes the main≠subagent split take effect.
pub fn subscription_router_config_json(subagents_local: bool) -> serde_json::Value {
    let subscription = || {
        json!({
            "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
            "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
        })
    };
    let subagent = if subagents_local {
        json!({ "endpoint": "local" })
    } else {
        subscription()
    };
    json!({
        "endpoints": [{
            "id": CODEX_SUBSCRIPTION_ENDPOINT_ID,
            "protocol": "openai_responses",
            "base_url": CODEX_SUBSCRIPTION_BASE_URL,
            "auth": "client_bearer",
            "models": [
                CODEX_SUBSCRIPTION_DEFAULT_MODEL,
                "gpt-5.4-mini",
                "gpt-5.5"
            ]
        }],
        "routes": {
            "main": subscription(),
            "subagent": subagent,
            "default": subscription(),
            "model_routes": {
                "rayline-codex": subscription(),
                "rayline-local": subscription()
            }
        }
    })
}

fn codex_cli_version_header() -> Option<String> {
    let output = Command::new("codex").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(version) = parse_codex_version_text(&stdout) {
        return Some(version);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_codex_version_text(&stderr)
}

fn parse_codex_version_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|part| {
            part.as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_digit())
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        })
        .map(ToOwned::to_owned)
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("rayline: failed to exec codex: {error}");
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::{
        CODEX_SUBSCRIPTION_DEFAULT_MODEL, CODEX_SUBSCRIPTION_ENDPOINT_ID, CodexAuthMode,
        EffectiveCodexAuthMode, parse_codex_version_text, subscription_router_config_json,
    };
    use std::path::PathBuf;

    fn write_config(name: &str, body: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rayline-codex-auth-test-{}-{name}-{unique}.json",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parses_codex_cli_version_output() {
        assert_eq!(
            parse_codex_version_text("codex-cli 0.142.0\n").as_deref(),
            Some("0.142.0")
        );
        assert_eq!(
            parse_codex_version_text("codex 1.2.3-beta.1+build\n").as_deref(),
            Some("1.2.3-beta.1+build")
        );
        assert_eq!(parse_codex_version_text("codex-cli\n"), None);
    }

    #[test]
    fn subscription_config_all_subscription_when_no_local_model() {
        // Fallback shape (no usable local model): main AND subagent → subscription.
        let cfg = subscription_router_config_json(false);
        assert_eq!(
            cfg["routes"]["main"]["endpoint"],
            CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        assert_eq!(
            cfg["routes"]["main"]["model"],
            CODEX_SUBSCRIPTION_DEFAULT_MODEL
        );
        assert_eq!(
            cfg["routes"]["subagent"]["endpoint"],
            CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        // Sentinel model_routes always pin main to the subscription.
        for m in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][m]["endpoint"],
                CODEX_SUBSCRIPTION_ENDPOINT_ID
            );
        }
    }

    #[test]
    fn subscription_config_hybrid_routes_subagents_local() {
        // Hybrid shape (usable local model): main → subscription, subagent → local.
        let cfg = subscription_router_config_json(true);
        assert_eq!(
            cfg["routes"]["main"]["endpoint"],
            CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        assert_eq!(cfg["routes"]["subagent"]["endpoint"], "local");
        // No model pinned on the local subagent route — the router fills the
        // configured local model id.
        assert!(cfg["routes"]["subagent"].get("model").is_none());
        // Sentinels still pin MAIN turns to the subscription; subagent turns skip
        // them (see the local router's select_route).
        for m in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][m]["endpoint"],
                CODEX_SUBSCRIPTION_ENDPOINT_ID
            );
        }
    }

    #[test]
    fn auto_without_config_resolves_to_subscription() {
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(None),
            EffectiveCodexAuthMode::Subscription
        );
    }

    #[test]
    fn auto_with_subscription_main_config_resolves_to_subscription() {
        // Regression: `--auth auto` + a `main: subscription` config must resolve to
        // subscription (materializing the ChatGPT backend) instead of "no auth",
        // which routed the sentinel to a dead placeholder and 502-looped.
        let path = write_config(
            "sub-main",
            r#"{"routes":{"main":{"endpoint":"subscription"},"subagent":{"endpoint":"local"}}}"#,
        );
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::Subscription
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn auto_with_absent_main_resolves_to_subscription() {
        // An absent `routes.main` is treated as the subscription passthrough.
        let path = write_config("no-main", r#"{"routes":{"subagent":{"endpoint":"local"}}}"#);
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::Subscription
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn auto_with_non_subscription_main_config_resolves_to_none() {
        // A local/provider main needs no client auth — auto stays "none".
        let path = write_config(
            "local-main",
            r#"{"endpoints":[{"id":"ollama","protocol":"openai_chat","base_url":"http://127.0.0.1:11434/v1","models":["qwen"]}],"routes":{"main":{"endpoint":"ollama","model":"qwen"}}}"#,
        );
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::None
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn explicit_modes_ignore_config() {
        let path = write_config(
            "local-main2",
            r#"{"endpoints":[{"id":"ollama","protocol":"openai_chat","base_url":"http://127.0.0.1:11434/v1","models":["qwen"]}],"routes":{"main":{"endpoint":"ollama","model":"qwen"}}}"#,
        );
        // `--auth subscription` always subscription, even against a local-main config.
        assert_eq!(
            CodexAuthMode::Subscription.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::Subscription
        );
        // `--auth none` always none, even against a subscription-main config.
        let sub = write_config(
            "sub-main2",
            r#"{"routes":{"main":{"endpoint":"subscription"}}}"#,
        );
        assert_eq!(
            CodexAuthMode::None.effective_for_run(Some(&sub)),
            EffectiveCodexAuthMode::None
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sub);
    }
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to run codex: {error}");
            ExitCode::from(127)
        }
    }
}

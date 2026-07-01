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
            Self::Auto if config_path.is_some() => EffectiveCodexAuthMode::None,
            Self::Auto | Self::Subscription => EffectiveCodexAuthMode::Subscription,
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
    pub model: String,
    pub config_path: Option<PathBuf>,
    pub auth_mode: CodexAuthMode,
    pub codex_args: Vec<OsString>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigureRequest {
    pub model: String,
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
        Ok(message) => {
            eprint!("{message}");
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
    let mut command = Command::new("codex");
    command
        .arg("-c")
        .arg("model_provider=\"rayline\"")
        .arg("-c")
        .arg(format!("model={}", toml_string(&request.model)))
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

pub fn configure(request: &ConfigureRequest) -> io::Result<String> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    fs::create_dir_all(&codex_home)?;
    let profile_path = codex_home.join("rayline.config.toml");
    let base_url = request.base_url.clone().unwrap_or_else(|| {
        format!(
            "http://127.0.0.1:{}/v1",
            crate::router::DEFAULT_LOCAL_ROUTER_PORT
        )
    });
    let subscription_auth =
        request.auth_mode.effective_for_run(None) == EffectiveCodexAuthMode::Subscription;
    let http_headers = if subscription_auth {
        codex_cli_version_header()
            .map(|version| format!("http_headers = {{ version = {} }}\n", toml_string(&version)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let contents = format!(
        "model = {}\nmodel_provider = \"rayline\"\n{}\
\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = {}\nwire_api = \"responses\"\n{}{}",
        toml_string(&request.model),
        if subscription_auth {
            "forced_login_method = \"chatgpt\"\n"
        } else {
            ""
        },
        toml_string(&base_url),
        if subscription_auth {
            "requires_openai_auth = true\n"
        } else {
            ""
        },
        http_headers,
    );
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

pub fn write_subscription_router_config(home: &Path) -> io::Result<PathBuf> {
    let path = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join(CODEX_SUBSCRIPTION_CONFIG_FILENAME);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let body =
        serde_json::to_vec_pretty(&subscription_router_config_json()).map_err(io::Error::other)?;
    fs::write(&path, body)?;
    Ok(path)
}

pub fn subscription_router_config_json() -> serde_json::Value {
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
            "main": {
                "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
                "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
            },
            "subagent": {
                "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
                "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
            },
            "default": {
                "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
                "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
            },
            "model_routes": {
                "rayline-codex": {
                    "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
                    "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
                },
                "rayline-local": {
                    "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
                    "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
                }
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
    use super::parse_codex_version_text;

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

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub mod catalog;
pub mod claude;
pub mod local_model;
pub mod router;
pub mod status;
pub mod update;

pub const CLI_BIN: &str = "rayline";
pub const DAEMON_BIN: &str = "rld";
pub const DISPLAY_NAME: &str = "Rayline";
pub const CONFIG_DIR: &str = "rayline";
pub const DOT_CONFIG_DIR: &str = ".rayline";
pub const ROUTER_STATE_DIR: &str = ".rayline/rld";
pub const ROUTER_FILE_PREFIX: &str = "rl-rld";
pub const ROUTER_PROD_URL: &str = "https://api.rayline.ai";
pub const UPDATE_BASE_URL: &str = "https://get.rayline.ai";
pub const INSTALLER_URL: &str = "https://get.rayline.ai/install.sh";
pub const UV_TOOL_NAME: &str = "rayline-cli";
pub const CLAUDE_LAUNCHES_SUFFIX: &str = ".config/rayline/claude-daemon-launches.json";
pub const RAYLINE_VERSION: &str = env!("RAYLINE_VERSION");
pub const RAYLINE_CHANNEL: &str = env!("RAYLINE_CHANNEL");

const ROOT_HELP: &str = "\
Usage: rayline [OPTIONS] COMMAND [ARGS]...

Options:
  --version    Show version
  --help       Show this message and exit

Commands:
  auth       Sign in to hosted Rayline
  status     Show current CLI auth status
  claude     Run Claude Code through Rayline routing
  router     Inspect or stop the local Rayline router runtime
  top        Show live router request metrics
  local      Configure local model routing
  update     Check for or install a rayline launcher update
";

const STATUS_HELP: &str = "\
Usage: rayline status

Show current CLI auth status.
";

const AUTH_HELP: &str = "\
Usage: rayline auth COMMAND

Commands:
  login    Sign in to Rayline
  logout   Revoke the stored Rayline session and router key
  status   Show current auth status
  token    Print a valid account bearer token
";

const AUTH_LOGIN_HELP: &str = "\
Usage: rayline auth login [OPTIONS]

Options:
  -b, --no-browser     Use device-code login instead of a local callback
  -p, --paste          Paste a browser callback URL manually
  --help               Show this message and exit
";

const AUTH_LOGOUT_HELP: &str = "\
Usage: rayline auth logout

Revoke the stored Rayline session and remove the stored router key.
";

const AUTH_STATUS_HELP: &str = "\
Usage: rayline auth status

Show current auth status.
";

const AUTH_TOKEN_HELP: &str = "\
Usage: rayline auth token

Print a valid account bearer token for scripts and integrations.
";

const CLAUDE_HELP: &str = "\
Usage: rayline claude [OPTIONS] [--] [CLAUDE_ARGS]...
       rayline claude run [OPTIONS] [--] [CLAUDE_ARGS]...

Run Claude Code through Rayline hosted routing. Use --local-router for the
local static router path.

Options:
  -m, --model <model>               Route through a specific model
  --auto-compact-window <tokens>    Override Claude auto-compact threshold
  --local-router                    Use local static routing without hosted auth
  --isolated                        Use a separate Claude config dir so this
                                    session can run beside other Claude Code
                                    background agents
  --routing-mode <override|proxy|proxy-subagents>
                                    Select routing mode (default: proxy-subagents)
  --no-proxy                        Use direct router env override
  --local-injector-port <port>      Local injector port
  --statusline/--no-statusline      Show proxy picked model in status line
  --diagnose                        Print routing diagnostics before exec
  --upstream-ca-path <path>         CA bundle for upstream proxy mode
  --router-config-path <path>       Local-router static router JSON config
  --help                            Show this message and exit
";

const ROUTER_HELP: &str = "\
Usage: rayline router COMMAND

Commands:
  status   Show router status
  logs     Print router logs
  top      Show live router request metrics
  stop     Stop router processes
";

const ROUTER_STATUS_HELP: &str = "\
Usage: rayline router status

Show local router process status.
";

const ROUTER_LOGS_HELP: &str = "\
Usage: rayline router logs [--lines <count>]

Print local router logs.
";

const ROUTER_TOP_HELP: &str = "\
Usage: rayline top [--json] [--all]
       rayline router top [--json] [--all]

Show live LLM request metrics.

Options:
  --json      Print one snapshot as JSON
  --all       Include proxied Anthropic sideband traffic
";

const ROUTER_STOP_HELP: &str = "\
Usage: rayline router stop

Stop local router processes.
";

const LOCAL_HELP: &str = "\
Usage: rayline local COMMAND

Configure local model routing for this machine.

Commands:
  models    List recommended models with download status and hardware fit
  download  Download a recommended model without selecting it
  use       Select a recommended model, downloading it first if needed
  remove    Delete a downloaded model from disk
  custom    Use a custom endpoint URL + model name
  show      Show the configured mode, endpoint, and account routing state
  test      Probe a custom endpoint for an Anthropic Messages API response
  clear     Remove the local model configuration
  on        Turn local routing on for your account
  off       Turn local routing off for your account
";

const LOCAL_MODELS_HELP: &str = "\
Usage: rayline local models [--json]

List recommended local models.
";

const LOCAL_DOWNLOAD_HELP: &str = "\
Usage: rayline local download <model-id> [--json]

Download a recommended model into the local cache without selecting it.
";

const LOCAL_USE_HELP: &str = "\
Usage: rayline local use <model-id>

Select a recommended model for the built-in llama server.
";

const LOCAL_REMOVE_HELP: &str = "\
Usage: rayline local remove <model-id>

Delete a downloaded model's file from the local cache.
";

const LOCAL_CUSTOM_HELP: &str = "\
Usage: rayline local custom [--url <URL>] [--model <NAME>]

Use a custom local inference endpoint.
";

const LOCAL_SHOW_HELP: &str = "\
Usage: rayline local show

Show the configured local model mode and account routing state.
";

const LOCAL_TEST_HELP: &str = "\
Usage: rayline local test [--url <URL>] [--model <NAME>]

Probe a custom endpoint for protocol compatibility.
";

const LOCAL_CLEAR_HELP: &str = "\
Usage: rayline local clear

Remove the local model configuration.
";

const LOCAL_ON_HELP: &str = "\
Usage: rayline local on

Turn local routing on for your account.
";

const LOCAL_OFF_HELP: &str = "\
Usage: rayline local off

Turn local routing off for your account.
";

const UPDATE_HELP: &str = "\
Usage: rayline update [OPTIONS]

Options:
  --check              Check for an update without installing
  --version <version>  Install or check a specific version
  --channel <channel>  Use prod, dev, main, or local
  --force              Install even when already current
  --dry-run            Download and verify without replacing rayline
";

pub async fn run() -> ExitCode {
    let original_argv = env::args_os().collect::<Vec<_>>();
    run_argv(&original_argv).await
}

pub async fn run_argv(original_argv: &[OsString]) -> ExitCode {
    if root_version_requested(original_argv) {
        println!("rayline {RAYLINE_VERSION}");
        return ExitCode::SUCCESS;
    }

    if let Some(help) = rayline_help_for_argv(original_argv) {
        print!("{help}");
        return ExitCode::SUCCESS;
    }

    match rayline_dispatch_for_argv(original_argv) {
        RaylineDispatch::Version => {
            println!("rayline {RAYLINE_VERSION}");
            ExitCode::SUCCESS
        }
        RaylineDispatch::Status(request) => match status::render_status(&request) {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::AuthLogin(request) => match status::auth_login(&request).await {
            Ok(message) => match status::write_auth_message(&message) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: failed to write login output: {error}");
                    ExitCode::from(1)
                }
            },
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::AuthToken(request) => {
            let env_name =
                status::resolve_env(request.env_name.as_deref(), dirs::home_dir().as_deref());
            match status::resolve_auth_token(&request).await {
                Ok(status::AuthTokenOutcome::Available(value)) => {
                    let output = terminal_output_text(&value);
                    println!("{output}");
                    ExitCode::SUCCESS
                }
                Ok(status::AuthTokenOutcome::NotLoggedIn) => {
                    eprintln!("Error: Not logged in to {env_name}. Run: rayline auth login");
                    ExitCode::from(1)
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::AuthLogout(request) => match status::logout(&request).await {
            Ok(message) => match status::write_auth_message(&message) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Error: failed to write logout output: {error}");
                    ExitCode::from(1)
                }
            },
            Err(error) => {
                eprintln!("Error: failed to update credentials: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::ClaudeRun(mut request) => {
            if !routing_mode_explicit(original_argv) {
                request.routing_mode = claude::RoutingMode::ProxySubagents;
            }
            exec_claude(request).await
        }
        RaylineDispatch::RouterStatus(request) => {
            match crate::router::render_status(&request).await {
                Ok(message) => {
                    print!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::RouterLogs(request) => match crate::router::render_logs(&request) {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::RouterTop(request) => match crate::router::render_top(&request).await {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::RouterStop(request) => match crate::router::stop(&request).await {
            Ok(message) => {
                print!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalModels { env_name, json } => {
            match catalog::models_command(env_name.as_deref(), json).await {
                Ok(message) => {
                    print!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => local_error(error, json),
            }
        }
        RaylineDispatch::LocalDownload {
            env_name,
            model_id,
            json,
        } => match catalog::download_command(env_name.as_deref(), &model_id, json).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => local_error(error, json),
        },
        RaylineDispatch::LocalUse { env_name, model_id } => {
            match catalog::use_command(env_name.as_deref(), &model_id).await {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalRemove { env_name, model_id } => {
            match catalog::remove_command(env_name.as_deref(), &model_id).await {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalCustom(request) => match local_model::set_custom(&request) {
            Ok(config) => {
                let url = config.base_url.as_deref().unwrap_or("(not set)");
                let model = config.model.as_deref().unwrap_or("(not set)");
                println!("Local model set to custom endpoint.\n  URL:    {url}\n  Model:  {model}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalShow {
            env_name,
            auth_token,
        } => {
            print!(
                "{}",
                local_model::render_show(env_name.as_deref(), auth_token.as_deref()).await
            );
            ExitCode::SUCCESS
        }
        RaylineDispatch::LocalTest(request) => match local_model::test(&request).await {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalClear => match local_model::clear() {
            Ok(true) => {
                println!("Local model configuration cleared.");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("No local model configuration was set.");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::LocalOn {
            env_name,
            auth_token,
        } => {
            match local_model::set_router_enabled(true, env_name.as_deref(), auth_token.as_deref())
                .await
            {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::LocalOff {
            env_name,
            auth_token,
        } => {
            match local_model::set_router_enabled(false, env_name.as_deref(), auth_token.as_deref())
                .await
            {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        RaylineDispatch::Update(request) => match update::run(&request).await {
            Ok(result) => {
                if result.stderr {
                    eprint!("{}", result.message);
                } else {
                    print!("{}", result.message);
                }
                ExitCode::from(result.exit_code)
            }
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::from(1)
            }
        },
        RaylineDispatch::Unavailable => unavailable(original_argv),
    }
}

fn local_error(error: String, json: bool) -> ExitCode {
    if json {
        catalog::emit_error_json(&error);
    } else {
        eprintln!("Error: {error}");
    }
    ExitCode::from(1)
}

fn terminal_output_text(value: &str) -> String {
    value.chars().collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RaylineDispatch {
    Version,
    Status(status::StatusRequest),
    AuthLogin(status::AuthLoginRequest),
    AuthToken(status::AuthTokenRequest),
    AuthLogout(status::AuthLogoutRequest),
    ClaudeRun(claude::RunRequest),
    RouterStatus(router::RouterStatusRequest),
    RouterLogs(router::RouterLogsRequest),
    RouterTop(router::RouterTopRequest),
    RouterStop(router::RouterStopRequest),
    LocalModels {
        env_name: Option<String>,
        json: bool,
    },
    LocalDownload {
        env_name: Option<String>,
        model_id: String,
        json: bool,
    },
    LocalUse {
        env_name: Option<String>,
        model_id: String,
    },
    LocalRemove {
        env_name: Option<String>,
        model_id: String,
    },
    LocalCustom(local_model::LocalCustomRequest),
    LocalShow {
        env_name: Option<String>,
        auth_token: Option<String>,
    },
    LocalTest(local_model::LocalTestRequest),
    LocalClear,
    LocalOn {
        env_name: Option<String>,
        auth_token: Option<String>,
    },
    LocalOff {
        env_name: Option<String>,
        auth_token: Option<String>,
    },
    Update(update::UpdateRequest),
    Unavailable,
}

pub fn rayline_dispatch_for_argv(original_argv: &[OsString]) -> RaylineDispatch {
    if root_version_requested(original_argv) {
        return RaylineDispatch::Version;
    }

    let mut root_env = None;
    let mut root_auth_token = None;
    let mut root_env_explicit = false;
    let mut args = original_argv.iter().skip(1).peekable();

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return RaylineDispatch::Unavailable;
        };

        if arg == "--" {
            return RaylineDispatch::Unavailable;
        }
        if let Some((name, value)) = arg.split_once('=') {
            match name {
                "--env" => {
                    if !status::is_valid_root_env(value) {
                        return RaylineDispatch::Unavailable;
                    }
                    root_env = Some(value.to_owned());
                    root_env_explicit = true;
                    continue;
                }
                "--auth-token" => {
                    root_auth_token = Some(value.to_owned());
                    continue;
                }
                _ => {}
            }
        }
        if is_value_option(arg) {
            if arg.contains('=') {
                continue;
            }
            let Some(value) = args.next() else {
                return RaylineDispatch::Unavailable;
            };
            if arg == "--env" {
                let Some(value) = value.to_str() else {
                    return RaylineDispatch::Unavailable;
                };
                if !status::is_valid_root_env(value) {
                    return RaylineDispatch::Unavailable;
                }
                root_env = Some(value.to_owned());
                root_env_explicit = true;
            } else if arg == "--auth-token" {
                let Some(value) = value.to_str() else {
                    return RaylineDispatch::Unavailable;
                };
                root_auth_token = Some(value.to_owned());
            }
            continue;
        }
        if arg == "--version" || arg == "--help" {
            continue;
        }
        if arg.starts_with('-') {
            return RaylineDispatch::Unavailable;
        }

        return match arg {
            "auth" => parse_auth_dispatch(args, root_env, root_auth_token, root_env_explicit)
                .unwrap_or(RaylineDispatch::Unavailable),
            "claude" => parse_claude_request(args, root_env, root_auth_token, root_env_explicit)
                .map(RaylineDispatch::ClaudeRun)
                .unwrap_or(RaylineDispatch::Unavailable),
            "local" => parse_local_dispatch(args, root_env, root_auth_token)
                .unwrap_or(RaylineDispatch::Unavailable),
            "router" => parse_router_dispatch(args, root_env_explicit)
                .unwrap_or(RaylineDispatch::Unavailable),
            "status" => parse_status_request(args, root_env, root_auth_token, root_env_explicit)
                .map(RaylineDispatch::Status)
                .unwrap_or(RaylineDispatch::Unavailable),
            "top" => parse_router_top_request(args, root_env_explicit)
                .map(RaylineDispatch::RouterTop)
                .unwrap_or(RaylineDispatch::Unavailable),
            "update" => parse_update_request(args)
                .map(RaylineDispatch::Update)
                .unwrap_or(RaylineDispatch::Unavailable),
            _ => RaylineDispatch::Unavailable,
        };
    }

    RaylineDispatch::Unavailable
}

fn parse_auth_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    match args.next()?.to_str()? {
        "login" => parse_auth_login_request(args, root_env, root_env_explicit)
            .map(RaylineDispatch::AuthLogin),
        "logout" => parse_auth_logout_request(args, root_env, root_env_explicit)
            .map(RaylineDispatch::AuthLogout),
        "status" => parse_status_request(args, root_env, root_auth_token, root_env_explicit)
            .map(RaylineDispatch::Status),
        "token" => parse_auth_token_request(args, root_env, root_auth_token, root_env_explicit)
            .map(RaylineDispatch::AuthToken),
        _ => None,
    }
}

fn parse_status_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<status::StatusRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let (env_name, auth_token, root_env_explicit) =
        parse_env_and_auth_options(args.by_ref(), root_env, root_auth_token, root_env_explicit)?;
    Some(status::StatusRequest {
        env_name,
        auth_token,
        root_env_explicit,
    })
}

fn parse_auth_token_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<status::AuthTokenRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let (env_name, auth_token, root_env_explicit) =
        parse_env_and_auth_options(args.by_ref(), root_env, root_auth_token, root_env_explicit)?;
    Some(status::AuthTokenRequest {
        env_name,
        auth_token,
        root_env_explicit,
    })
}

fn parse_auth_logout_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_env_explicit: bool,
) -> Option<status::AuthLogoutRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let (env_name, _, root_env_explicit) =
        parse_env_and_auth_options(args.by_ref(), root_env, None, root_env_explicit)?;
    Some(status::AuthLogoutRequest {
        env_name,
        root_env_explicit,
    })
}

fn parse_auth_login_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_env_explicit: bool,
) -> Option<status::AuthLoginRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut env_name = root_env;
    let mut root_env_explicit = root_env_explicit;
    let mut no_browser = false;
    let mut paste = false;

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            if option == "--env" {
                if !status::is_valid_root_env(value) {
                    return None;
                }
                env_name = Some(value.to_owned());
                root_env_explicit = true;
                continue;
            }
            return None;
        }
        match arg {
            "--env" => {
                let value = args.next()?.to_str()?;
                if !status::is_valid_root_env(value) {
                    return None;
                }
                env_name = Some(value.to_owned());
                root_env_explicit = true;
            }
            "-b" | "--no-browser" => no_browser = true,
            "-p" | "--paste" => paste = true,
            _ => return None,
        }
    }

    Some(status::AuthLoginRequest {
        env_name,
        root_env_explicit,
        no_browser,
        paste,
    })
}

fn parse_env_and_auth_options<'a, I>(
    mut args: I,
    mut env_name: Option<String>,
    mut auth_token: Option<String>,
    mut root_env_explicit: bool,
) -> Option<(Option<String>, Option<String>, bool)>
where
    I: Iterator<Item = &'a OsString>,
{
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--env" => {
                    if !status::is_valid_root_env(value) {
                        return None;
                    }
                    env_name = Some(value.to_owned());
                    root_env_explicit = true;
                }
                "--auth-token" => auth_token = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--env" => {
                let value = args.next()?.to_str()?;
                if !status::is_valid_root_env(value) {
                    return None;
                }
                env_name = Some(value.to_owned());
                root_env_explicit = true;
            }
            "--auth-token" => auth_token = Some(args.next()?.to_str()?.to_owned()),
            _ => return None,
        }
    }

    Some((env_name, auth_token, root_env_explicit))
}

fn parse_claude_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
    root_env_explicit: bool,
) -> Option<crate::claude::RunRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut env_name = root_env;
    let mut model = None;
    let mut auto_compact_window = None;
    let mut local_router = false;
    let mut isolated = false;
    let mut local_injector_port = None;
    let mut routing_mode = crate::claude::RoutingMode::Proxy;
    let mut route_statusline_enabled = true;
    let mut diagnose = false;
    let mut upstream_ca_path = None;
    let mut router_config_path = None;
    let mut claude_args = Vec::new();

    if args
        .peek()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| arg == "run")
    {
        let _ = args.next();
    } else if args
        .peek()
        .and_then(|arg| arg.to_str())
        .is_some_and(is_claude_management_subcommand)
    {
        return None;
    }

    while let Some(arg) = args.next() {
        let Some(arg_str) = arg.to_str() else {
            claude_args.push(arg.clone());
            continue;
        };

        if arg_str == "--help" {
            return None;
        }
        if arg_str == "--" {
            claude_args.extend(args.cloned());
            break;
        }
        if let Some((option, value)) = arg_str.split_once('=') {
            match option {
                "--env" => {
                    env_name = Some(value.to_owned());
                    continue;
                }
                "--model" => {
                    model = Some(value.to_owned());
                    continue;
                }
                "--auto-compact-window" => {
                    auto_compact_window = Some(value.parse().ok()?);
                    continue;
                }
                "--routing-mode" => {
                    routing_mode = parse_routing_mode(value)?;
                    continue;
                }
                "--local-injector-port" => {
                    local_injector_port = Some(value.parse().ok()?);
                    continue;
                }
                "--upstream-ca-path" => {
                    upstream_ca_path = Some(PathBuf::from(value));
                    continue;
                }
                "--router-config-path" => {
                    router_config_path = Some(PathBuf::from(value));
                    continue;
                }
                _ => {}
            }
        }
        match arg_str {
            "--env" => {
                env_name = Some(args.next()?.to_str()?.to_owned());
                continue;
            }
            "--model" | "-m" => {
                model = Some(args.next()?.to_str()?.to_owned());
                continue;
            }
            "--auto-compact-window" => {
                auto_compact_window = Some(args.next()?.to_str()?.parse().ok()?);
                continue;
            }
            "--routing-mode" => {
                routing_mode = parse_routing_mode(args.next()?.to_str()?)?;
                continue;
            }
            "--local" => {
                eprintln!(
                    "Warning: `--local` has been removed and is ignored. Use `{CLI_BIN} claude --local-router` for public Rayline local routing."
                );
                continue;
            }
            "--local-router" => {
                local_router = true;
                continue;
            }
            "--isolated" => {
                isolated = true;
                continue;
            }
            "--no-proxy" => {
                routing_mode = crate::claude::RoutingMode::Override;
                continue;
            }
            "--diagnose" => {
                diagnose = true;
                continue;
            }
            "--local-injector-port" => {
                local_injector_port = Some(args.next()?.to_str()?.parse().ok()?);
                continue;
            }
            "--upstream-ca-path" => {
                upstream_ca_path = Some(PathBuf::from(args.next()?));
                continue;
            }
            "--router-config-path" => {
                router_config_path = Some(PathBuf::from(args.next()?));
                continue;
            }
            "--no-telemetry" => {
                return None;
            }
            "--statusline" => {
                route_statusline_enabled = true;
                continue;
            }
            "--no-statusline" => {
                route_statusline_enabled = false;
                continue;
            }
            _ => {}
        }

        if claude_args.is_empty() && is_claude_management_subcommand(arg_str) {
            return None;
        }
        claude_args.push(arg.clone());
    }

    Some(crate::claude::RunRequest {
        env_name,
        auth_token: root_auth_token,
        args: claude_args,
        model,
        auto_compact_window,
        local_router,
        isolated,
        local_injector_port,
        routing_mode,
        route_statusline_enabled,
        diagnose,
        upstream_ca_path,
        router_config_path,
        root_env_explicit,
    })
}

fn parse_routing_mode(value: &str) -> Option<crate::claude::RoutingMode> {
    match value {
        "override" => Some(crate::claude::RoutingMode::Override),
        "proxy" => Some(crate::claude::RoutingMode::Proxy),
        "proxy-subagents" => Some(crate::claude::RoutingMode::ProxySubagents),
        _ => None,
    }
}

fn is_claude_management_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "login" | "logout" | "key" | "setup" | "hooks" | "telemetry"
    )
}

fn parse_router_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    match args.next()?.to_str()? {
        "status" => {
            if args.next().is_some() {
                return None;
            }
            Some(RaylineDispatch::RouterStatus(
                crate::router::RouterStatusRequest { root_env_explicit },
            ))
        }
        "logs" => {
            parse_router_logs_request(args, root_env_explicit).map(RaylineDispatch::RouterLogs)
        }
        "top" => parse_router_top_request(args, root_env_explicit).map(RaylineDispatch::RouterTop),
        "stop" => {
            if args.next().is_some() {
                return None;
            }
            Some(RaylineDispatch::RouterStop(
                crate::router::RouterStopRequest { root_env_explicit },
            ))
        }
        _ => None,
    }
}

fn parse_router_logs_request<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<crate::router::RouterLogsRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut lines = 40;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;

        if arg == "--help" {
            return None;
        }
        if let Some(value) = arg.strip_prefix("-n") {
            if value.is_empty() {
                lines = args.next()?.to_str()?.parse().ok()?;
            } else {
                lines = value.parse().ok()?;
            }
            continue;
        }
        if let Some((option, value)) = arg.split_once('=') {
            if option == "--lines" {
                lines = value.parse().ok()?;
                continue;
            }
        }
        if arg == "--lines" {
            lines = args.next()?.to_str()?.parse().ok()?;
            continue;
        }

        return None;
    }

    Some(crate::router::RouterLogsRequest {
        lines,
        root_env_explicit,
    })
}

fn parse_router_top_request<'a, I>(
    args: std::iter::Peekable<I>,
    root_env_explicit: bool,
) -> Option<crate::router::RouterTopRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut json = false;
    let mut show_all = false;
    for arg in args {
        let arg = arg.to_str()?;
        match arg {
            "--json" => json = true,
            "--all" => show_all = true,
            "--help" => return None,
            _ => return None,
        }
    }

    Some(crate::router::RouterTopRequest {
        json,
        show_all,
        root_env_explicit,
    })
}

fn parse_update_request<'a, I>(mut args: std::iter::Peekable<I>) -> Option<update::UpdateRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut channel = None;
    let mut pinned_version = None;
    let mut force = false;
    let mut check_only = false;
    let mut dry_run = false;

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--channel" => channel = Some(value.to_owned()),
                "--version" => pinned_version = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--channel" => channel = Some(args.next()?.to_str()?.to_owned()),
            "--version" => pinned_version = Some(args.next()?.to_str()?.to_owned()),
            "--force" => force = true,
            "--check" => check_only = true,
            "--dry-run" => dry_run = true,
            _ => return None,
        }
    }

    Some(update::UpdateRequest {
        channel,
        pinned_version,
        force,
        check_only,
        dry_run,
    })
}

fn parse_local_dispatch<'a, I>(
    mut args: std::iter::Peekable<I>,
    root_env: Option<String>,
    root_auth_token: Option<String>,
) -> Option<RaylineDispatch>
where
    I: Iterator<Item = &'a OsString>,
{
    match args.next()?.to_str()? {
        "models" => parse_local_json_flag(args).map(|json| RaylineDispatch::LocalModels {
            env_name: root_env,
            json,
        }),
        "download" => {
            parse_local_model_id_arg(args).map(|(model_id, json)| RaylineDispatch::LocalDownload {
                env_name: root_env,
                model_id,
                json,
            })
        }
        "use" => parse_local_model_id_arg(args).and_then(|(model_id, json)| {
            (!json).then_some(RaylineDispatch::LocalUse {
                env_name: root_env,
                model_id,
            })
        }),
        "remove" => parse_local_model_id_arg(args).and_then(|(model_id, json)| {
            (!json).then_some(RaylineDispatch::LocalRemove {
                env_name: root_env,
                model_id,
            })
        }),
        "custom" => parse_local_custom_request(args).map(RaylineDispatch::LocalCustom),
        "show" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalShow {
            env_name: root_env,
            auth_token: root_auth_token,
        }),
        "test" => parse_local_test_request(args).map(RaylineDispatch::LocalTest),
        "clear" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalClear),
        "on" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalOn {
            env_name: root_env,
            auth_token: root_auth_token,
        }),
        "off" => parse_local_no_arg(args).map(|()| RaylineDispatch::LocalOff {
            env_name: root_env,
            auth_token: root_auth_token,
        }),
        _ => None,
    }
}

fn parse_local_no_arg<'a, I>(mut args: std::iter::Peekable<I>) -> Option<()>
where
    I: Iterator<Item = &'a OsString>,
{
    args.next().is_none().then_some(())
}

fn parse_local_json_flag<'a, I>(mut args: std::iter::Peekable<I>) -> Option<bool>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut json = false;
    for arg in args.by_ref() {
        match arg.to_str()? {
            "--json" => json = true,
            "--help" => return None,
            _ => return None,
        }
    }
    Some(json)
}

fn parse_local_model_id_arg<'a, I>(mut args: std::iter::Peekable<I>) -> Option<(String, bool)>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut model_id = None;
    let mut json = false;
    for arg in args.by_ref() {
        match arg.to_str()? {
            "--json" => json = true,
            "--help" => return None,
            value if !value.starts_with('-') && model_id.is_none() => {
                model_id = Some(value.to_owned());
            }
            _ => return None,
        }
    }
    Some((model_id?, json))
}

fn parse_local_custom_request<'a, I>(
    mut args: std::iter::Peekable<I>,
) -> Option<local_model::LocalCustomRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut base_url = None;
    let mut model = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--url" | "--base-url" => base_url = Some(value.to_owned()),
                "--model" => model = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--url" | "--base-url" => base_url = Some(args.next()?.to_str()?.to_owned()),
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            _ => return None,
        }
    }
    Some(local_model::LocalCustomRequest { base_url, model })
}

fn parse_local_test_request<'a, I>(
    mut args: std::iter::Peekable<I>,
) -> Option<local_model::LocalTestRequest>
where
    I: Iterator<Item = &'a OsString>,
{
    let mut base_url = None;
    let mut model = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--help" {
            return None;
        }
        if let Some((option, value)) = arg.split_once('=') {
            match option {
                "--url" | "--base-url" => base_url = Some(value.to_owned()),
                "--model" => model = Some(value.to_owned()),
                _ => return None,
            }
            continue;
        }
        match arg {
            "--url" | "--base-url" => base_url = Some(args.next()?.to_str()?.to_owned()),
            "--model" => model = Some(args.next()?.to_str()?.to_owned()),
            _ => return None,
        }
    }
    Some(local_model::LocalTestRequest { base_url, model })
}

fn root_version_requested(original_argv: &[OsString]) -> bool {
    let mut args = original_argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return false;
        };
        if arg == "--version" {
            return true;
        }
        if arg == "--" {
            return false;
        }
        if is_value_option(arg) {
            if !arg.contains('=') {
                args.next();
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return false;
    }
    false
}

async fn exec_claude(request: crate::claude::RunRequest) -> ExitCode {
    let mut command = match crate::claude::run_command(&request).await {
        Ok(command) => command,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };

    exec_or_status(&mut command)
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("rayline: failed to exec claude: {error}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to run claude: {error}");
            ExitCode::from(127)
        }
    }
}

fn unavailable(original_argv: &[OsString]) -> ExitCode {
    let command = first_command(original_argv).unwrap_or("command");
    eprintln!("rayline: `{command}` is not available in this public Rayline build");
    ExitCode::from(127)
}

fn routing_mode_explicit(original_argv: &[OsString]) -> bool {
    original_argv
        .iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| {
            arg.to_str().is_some_and(|arg| {
                arg == "--no-proxy" || arg == "--routing-mode" || arg.starts_with("--routing-mode=")
            })
        })
}

fn rayline_help_for_argv(original_argv: &[OsString]) -> Option<&'static str> {
    let command = command_path_before_help(original_argv)?;
    match command.as_slice() {
        [] => Some(ROOT_HELP),
        ["auth"] => Some(AUTH_HELP),
        ["auth", "login"] => Some(AUTH_LOGIN_HELP),
        ["auth", "logout"] => Some(AUTH_LOGOUT_HELP),
        ["auth", "status"] => Some(AUTH_STATUS_HELP),
        ["auth", "token"] => Some(AUTH_TOKEN_HELP),
        ["claude"] | ["claude", "run"] => Some(CLAUDE_HELP),
        ["local"] => Some(LOCAL_HELP),
        ["local", "models"] => Some(LOCAL_MODELS_HELP),
        ["local", "download"] => Some(LOCAL_DOWNLOAD_HELP),
        ["local", "use"] => Some(LOCAL_USE_HELP),
        ["local", "remove"] => Some(LOCAL_REMOVE_HELP),
        ["local", "custom"] => Some(LOCAL_CUSTOM_HELP),
        ["local", "show"] => Some(LOCAL_SHOW_HELP),
        ["local", "test"] => Some(LOCAL_TEST_HELP),
        ["local", "clear"] => Some(LOCAL_CLEAR_HELP),
        ["local", "on"] => Some(LOCAL_ON_HELP),
        ["local", "off"] => Some(LOCAL_OFF_HELP),
        ["router"] => Some(ROUTER_HELP),
        ["router", "status"] => Some(ROUTER_STATUS_HELP),
        ["router", "logs"] => Some(ROUTER_LOGS_HELP),
        ["router", "top"] => Some(ROUTER_TOP_HELP),
        ["router", "stop"] => Some(ROUTER_STOP_HELP),
        ["status"] => Some(STATUS_HELP),
        ["top"] => Some(ROUTER_TOP_HELP),
        ["update"] => Some(UPDATE_HELP),
        _ => None,
    }
}

fn command_path_before_help(original_argv: &[OsString]) -> Option<Vec<&str>> {
    let mut args = original_argv.iter().skip(1).peekable();
    let mut command = Vec::new();

    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--" {
            return None;
        }
        if arg == "--help" {
            return Some(command);
        }
        if arg == "--version" {
            continue;
        }
        if is_value_option(arg) {
            if !arg.contains('=') {
                args.next()?;
            }
            continue;
        }
        if let Some(rest) = arg.strip_prefix("-m") {
            if rest.is_empty() {
                args.next()?;
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        command.push(arg);
    }

    if command.is_empty() || matches!(command.as_slice(), ["auth"] | ["local"] | ["router"]) {
        Some(command)
    } else {
        None
    }
}

fn first_command(original_argv: &[OsString]) -> Option<&str> {
    let mut args = original_argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        let arg = arg.to_str()?;
        if arg == "--" {
            return args.next().and_then(|arg| arg.to_str());
        }
        if is_value_option(arg) {
            if !arg.contains('=') {
                args.next()?;
            }
            continue;
        }
        if arg == "--version" || arg == "--help" || arg.starts_with('-') {
            continue;
        }
        return Some(arg);
    }
    None
}

fn is_value_option(arg: &str) -> bool {
    matches!(
        arg.split_once('=').map_or(arg, |(name, _)| name),
        "--env"
            | "--auth-token"
            | "--model"
            | "--auto-compact-window"
            | "--routing-mode"
            | "--local-injector-port"
            | "--upstream-ca-path"
            | "--router-config-path"
            | "--lines"
            | "--channel"
            | "--url"
            | "--base-url"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn routing_mode_before_separator_is_explicit() {
        assert!(routing_mode_explicit(&argv(&[
            "rayline",
            "claude",
            "--routing-mode=proxy"
        ])));
    }

    #[test]
    fn no_proxy_before_separator_is_explicit() {
        assert!(routing_mode_explicit(&argv(&[
            "rayline",
            "claude",
            "--no-proxy",
            "--",
            "--routing-mode"
        ])));
    }

    #[test]
    fn forwarded_routing_mode_after_separator_is_not_explicit() {
        assert!(!routing_mode_explicit(&argv(&[
            "rayline",
            "claude",
            "--",
            "--routing-mode",
            "proxy"
        ])));
    }

    #[test]
    fn root_version_is_public() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "--version"])),
            RaylineDispatch::Version
        );
    }

    #[test]
    fn public_help_surface_is_native() {
        for (args, needle) in [
            (&["rayline", "--help"][..], "Commands:"),
            (&["rayline", "update", "--help"][..], "rayline update"),
            (
                &["rayline", "router", "status", "--help"][..],
                "rayline router status",
            ),
            (
                &["rayline", "router", "logs", "--help"][..],
                "rayline router logs",
            ),
            (
                &["rayline", "router", "top", "--help"][..],
                "rayline router top",
            ),
            (&["rayline", "top", "--help"][..], "rayline top"),
            (
                &["rayline", "router", "stop", "--help"][..],
                "rayline router stop",
            ),
            (&["rayline", "local", "--help"][..], "rayline local"),
        ] {
            let help = rayline_help_for_argv(&argv(args)).expect("native help");
            assert!(help.contains(needle), "{args:?} should mention {needle:?}");
            assert!(
                !help.contains("--env"),
                "{args:?} should not expose hidden env selection"
            );
        }
    }

    #[test]
    fn claude_help_documents_hosted_and_local_router_modes() {
        let help =
            rayline_help_for_argv(&argv(&["rayline", "claude", "--help"])).expect("claude help");

        assert!(help.contains("hosted routing"));
        assert!(help.contains("local static router"));
        assert!(help.contains("--local-router"));
    }

    #[test]
    fn public_parser_accepts_hosted_claude() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline", "--env", "foo", "claude", "--", "--debug",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert_eq!(request.env_name.as_deref(), Some("foo"));
        assert!(!request.local_router);
        assert_eq!(
            request
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["--debug"]
        );
    }

    #[test]
    fn public_parser_accepts_local_router_claude() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude",
            "--local-router",
            "--isolated",
            "-p",
            "hi",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert!(request.local_router);
        assert!(request.isolated);
        assert_eq!(
            request
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["-p", "hi"]
        );
    }

    #[test]
    fn public_parser_accepts_auth_status_update_and_local() {
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "login", "--no-browser"])),
            RaylineDispatch::AuthLogin(status::AuthLoginRequest {
                no_browser: true,
                ..
            })
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "logout"])),
            RaylineDispatch::AuthLogout(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "status"])),
            RaylineDispatch::Status(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "auth", "token"])),
            RaylineDispatch::AuthToken(_)
        ));
        assert!(matches!(
            rayline_dispatch_for_argv(&argv(&["rayline", "status"])),
            RaylineDispatch::Status(_)
        ));
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "update", "--check", "--dry-run"])),
            RaylineDispatch::Update(update::UpdateRequest {
                channel: None,
                pinned_version: None,
                force: false,
                check_only: true,
                dry_run: true,
            })
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "local", "models", "--json"])),
            RaylineDispatch::LocalModels {
                env_name: None,
                json: true,
            }
        );
    }

    #[test]
    fn public_parser_accepts_claude_run_alias() {
        let dispatch = rayline_dispatch_for_argv(&argv(&[
            "rayline",
            "claude",
            "run",
            "--local-router",
            "--",
            "--debug",
        ]));

        let RaylineDispatch::ClaudeRun(request) = dispatch else {
            panic!("expected ClaudeRun");
        };
        assert!(request.local_router);
        assert_eq!(
            request
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["--debug"]
        );
    }

    #[test]
    fn public_parser_accepts_router_logs_lines() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&["rayline", "router", "logs", "--lines=7"])),
            RaylineDispatch::RouterLogs(crate::router::RouterLogsRequest {
                lines: 7,
                root_env_explicit: false,
            })
        );
    }

    #[test]
    fn public_parser_accepts_router_top_json() {
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline", "--env", "foo", "router", "top", "--json"
            ])),
            RaylineDispatch::RouterTop(crate::router::RouterTopRequest {
                json: true,
                show_all: false,
                root_env_explicit: true,
            })
        );
    }

    #[test]
    fn public_parser_accepts_top_alias_json() {
        assert!(
            rayline_help_for_argv(&argv(&["rayline", "top"])).is_none(),
            "top alias should execute without requiring a subcommand"
        );
        assert_eq!(
            rayline_dispatch_for_argv(&argv(&[
                "rayline", "--env", "foo", "top", "--json", "--all"
            ])),
            RaylineDispatch::RouterTop(crate::router::RouterTopRequest {
                json: true,
                show_all: true,
                root_env_explicit: true,
            })
        );
    }

    #[test]
    fn parser_rejects_invalid_hidden_env_names() {
        for env_name in ["", "foo.bar", "foo/bar", "foo bar"] {
            assert_eq!(
                rayline_dispatch_for_argv(&argv(&["rayline", "--env", env_name, "status"])),
                RaylineDispatch::Unavailable,
                "{env_name:?} should be rejected"
            );
        }
    }

    #[test]
    fn public_parser_rejects_private_command_groups() {
        for args in [
            &["rayline", "internal", "status"][..],
            &["rayline", "install-cli"][..],
            &["rayline", "hook-send", "Stop"][..],
            &["rayline", "agent", "run"][..],
            &["rayline", "chat"][..],
            &["rayline", "claude", "login"][..],
            &["rayline", "claude", "hooks", "status"][..],
            &["rayline", "claude", "telemetry", "status"][..],
            &["rayline", "claude", "--no-telemetry"][..],
            &["rayline", "claude", "setup"][..],
            &["rayline", "claude", "run", "setup"][..],
            &["rayline", "claude", "--model", "x", "hooks", "status"][..],
            &["rayline", "router", "start"][..],
        ] {
            assert_eq!(
                rayline_dispatch_for_argv(&argv(args)),
                RaylineDispatch::Unavailable,
                "{args:?} should be unavailable"
            );
        }
    }
}

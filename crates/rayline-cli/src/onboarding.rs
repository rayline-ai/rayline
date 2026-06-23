//! First-run onboarding for `<cli> claude --local`: the wizard that configures a
//! local model, the persisted completion marker, and the conservative read-only
//! default routes.

use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::{catalog, local_model, status};

/// Onboarding-flow schema. Bump when the flow changes materially so existing
/// users are re-onboarded on their next interactive `--local` launch.
pub const ONBOARDING_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnboardingOutcome {
    LocalModel,
    CustomEndpoint,
    Skipped,
}

impl OnboardingOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalModel => "local-model",
            Self::CustomEndpoint => "custom-endpoint",
            Self::Skipped => "skipped",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "custom-endpoint" => Self::CustomEndpoint,
            "skipped" => Self::Skipped,
            // Default unknown/missing to LocalModel: any recorded completion that
            // isn't an explicit skip should not silently re-trigger the wizard.
            _ => Self::LocalModel,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnboardingMarker {
    pub schema: u32,
    pub completed_at: u64,
    pub cli_version: String,
    pub outcome: OnboardingOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnboardingDecision {
    /// An engageable local model already exists — launch with it.
    UseExisting,
    /// Prompt the user to configure a local model.
    RunWizard,
    /// Do not prompt (non-interactive, or a prior completion that shouldn't
    /// re-trigger). The caller warns and does not engage local.
    Decline,
}

/// Pure first-run decision (§5.1 of the spec). `engageable` is whether a usable
/// `local_model` exists; `interactive` is the TTY gate.
pub fn decide(
    engageable: bool,
    interactive: bool,
    marker: Option<&OnboardingMarker>,
    current_schema: u32,
) -> OnboardingDecision {
    if engageable {
        return OnboardingDecision::UseExisting;
    }
    if !interactive {
        return OnboardingDecision::Decline;
    }
    match marker {
        None => OnboardingDecision::RunWizard,
        Some(m) if m.schema < current_schema => OnboardingDecision::RunWizard,
        Some(m) if m.outcome == OnboardingOutcome::Skipped => OnboardingDecision::RunWizard,
        // Configured before, schema current, but not engageable now: don't nag.
        Some(_) => OnboardingDecision::Decline,
    }
}

/// Seconds since the Unix epoch (dep-free timestamp for the marker; for
/// diagnostics only).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn read_onboarding(home: &Path) -> Option<OnboardingMarker> {
    let entry = status::read_settings(home)?.get("onboarding")?.clone();
    if !entry.is_object() {
        return None;
    }
    Some(OnboardingMarker {
        schema: entry.get("schema").and_then(Value::as_u64).unwrap_or(0) as u32,
        completed_at: entry
            .get("completed_at")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cli_version: entry
            .get("cli_version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        outcome: OnboardingOutcome::from_str(
            entry.get("outcome").and_then(Value::as_str).unwrap_or(""),
        ),
    })
}

pub fn write_onboarding_in_home(home: &Path, marker: &OnboardingMarker) -> io::Result<()> {
    let mut settings = status::read_settings(home)
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let object = settings
        .as_object_mut()
        .expect("settings is an object by construction");
    object.insert(
        "onboarding".to_owned(),
        json!({
            "schema": marker.schema,
            "completed_at": marker.completed_at,
            "cli_version": marker.cli_version,
            "outcome": marker.outcome.as_str(),
        }),
    );
    status::write_settings(home, &settings)
}

/// Conservative read-only / exploration subagents routed on-device by default
/// under `--local` (subagents scope). Name-based: the proxy matches the
/// agent-type header. `Explore` is the only guaranteed-present Claude Code
/// agent; the others are common read-only helpers and are harmless when absent.
/// Widen with `--route all` or `--router-config-path`.
pub const LOCAL_DEFAULT_SUBAGENTS: &[&str] = &[
    "Explore",
    "codebase-locator",
    "codebase-analyzer",
    "codebase-pattern-finder",
];

/// The managed default router config: route only the read-only allowlist to the
/// local endpoint. In `proxy-subagents` mode the proxy intercepts *only* these
/// agent types, so everything else (main + other subagents) stays on cloud.
pub fn default_local_routes_json() -> Value {
    let mut subagents = serde_json::Map::new();
    for name in LOCAL_DEFAULT_SUBAGENTS {
        subagents.insert((*name).to_owned(), json!({ "endpoint": "local" }));
    }
    json!({ "routes": { "subagents": Value::Object(subagents) } })
}

/// Readiness verdict returned to the `--local` launch path.
pub enum LocalModelReadiness {
    /// A usable local model exists; proceed with local routing.
    Ready(Box<local_model::LocalModelConfig>),
    /// No usable model — either the user skipped, the session is
    /// non-interactive, or the wizard was already completed and the model
    /// disappeared. Caller should warn and stay on cloud.
    NotConfigured,
}

/// Ensure a usable local model exists for a `--local` launch, running the
/// first-run wizard when appropriate. Returns `NotConfigured` when the user
/// skips or the session is non-interactive — the caller surfaces guidance.
pub async fn ensure_local_model(home: &Path, env_name: &str) -> io::Result<LocalModelReadiness> {
    let cfg = local_model::read_from_home(home);
    let engageable = cfg.as_ref().is_some_and(|c| c.is_engageable());
    if engageable {
        return Ok(LocalModelReadiness::Ready(Box::new(
            cfg.expect("engageable"),
        )));
    }
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let marker = read_onboarding(home);
    match decide(false, interactive, marker.as_ref(), ONBOARDING_SCHEMA) {
        OnboardingDecision::RunWizard => {
            run_wizard(home, env_name).await?;
            let cfg = local_model::read_from_home(home);
            if cfg.as_ref().is_some_and(|c| c.is_engageable()) {
                Ok(LocalModelReadiness::Ready(Box::new(
                    cfg.expect("engageable"),
                )))
            } else {
                Ok(LocalModelReadiness::NotConfigured) // user skipped
            }
        }
        OnboardingDecision::Decline | OnboardingDecision::UseExisting => {
            Ok(LocalModelReadiness::NotConfigured)
        }
    }
}

/// `<cli> local onboard [--reset]` — re-run the wizard from a terminal.
pub async fn run_onboard_command(env_name: Option<&str>, reset: bool) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "`{cli} local onboard` needs an interactive terminal. \
             Run it directly, or use `{cli} local use <model-id>` / \
             `{cli} local custom` non-interactively.",
            cli = crate::CLI_BIN,
        );
        return Ok(());
    }
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let env = status::resolve_env(env_name, Some(&home));
    if reset {
        let _ = local_model::clear(); // best-effort clean slate
    }
    run_wizard(&home, &env)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Interactive wizard: one decision with a hardware-aware default. Writes the
/// `local_model` selection (via existing setters) and the onboarding marker.
async fn run_wizard(home: &Path, env_name: &str) -> io::Result<OnboardingOutcome> {
    let cli = crate::CLI_BIN;
    let hardware = catalog::detect_hardware();
    let recommended = catalog::recommend_for_hardware(env_name, hardware).await;

    eprintln!(
        "Rayline can offload read-only exploration subagents (read / glob / grep) to a\nlocal model so the frontier model stays your manager. This is optional.\n"
    );
    if let Some(model) = recommended.as_ref() {
        eprintln!(
            "  Recommended: {id} — {size}",
            id = model.id,
            size = catalog::format_bytes(model.size_bytes),
        );
    } else {
        eprintln!("  (No recommended model fits this machine automatically.)");
    }
    eprintln!(
        "\n  [Y] Download & use the recommended model   (default)\n  [m] See all models\n  [o] Use my own server (Ollama / LM Studio / llama.cpp URL)\n  [s] Skip — stay on cloud for now\n"
    );
    eprint!("> ");
    io::stderr().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice = input.trim().to_ascii_lowercase();

    let outcome = match choice.as_str() {
        "" | "y" => match recommended {
            Some(model) => {
                catalog::download(&model, false)
                    .await
                    .map_err(io::Error::other)?;
                local_model::set_recommended(&model).map_err(io::Error::other)?;
                eprintln!("Local model set to {}.", model.id);
                OnboardingOutcome::LocalModel
            }
            None => {
                eprintln!("No recommended model available; staying on cloud.");
                OnboardingOutcome::Skipped
            }
        },
        "m" => {
            let color = io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            let listing = catalog::models_command(Some(env_name), false, color)
                .await
                .map_err(io::Error::other)?;
            eprint!("{listing}");
            eprint!("Model number (or id) — Enter to skip › ");
            io::stderr().flush().ok();
            let mut token = String::new();
            io::stdin().read_line(&mut token)?;
            let token = token.trim();
            if token.is_empty() {
                eprintln!("No model chosen; staying on cloud.");
                OnboardingOutcome::Skipped
            } else {
                catalog::use_command(Some(env_name), token)
                    .await
                    .map_err(io::Error::other)?;
                OnboardingOutcome::LocalModel
            }
        }
        "o" => {
            eprint!("Server URL (e.g. http://127.0.0.1:11434): ");
            io::stderr().flush().ok();
            let mut url = String::new();
            io::stdin().read_line(&mut url)?;
            eprint!("Model name it serves: ");
            io::stderr().flush().ok();
            let mut model = String::new();
            io::stdin().read_line(&mut model)?;
            let request = local_model::LocalCustomRequest {
                base_url: Some(url.trim().to_owned()),
                model: Some(model.trim().to_owned()),
            };
            local_model::set_custom(&request).map_err(io::Error::other)?;
            eprintln!("Custom endpoint saved. Test it with `{cli} local test`.");
            OnboardingOutcome::CustomEndpoint
        }
        _ => {
            eprintln!("Skipping — staying on cloud. Re-run later with `{cli} local onboard`.");
            OnboardingOutcome::Skipped
        }
    };

    write_onboarding_in_home(
        home,
        &OnboardingMarker {
            schema: ONBOARDING_SCHEMA,
            completed_at: now_unix(),
            cli_version: crate::RAYLINE_VERSION.to_owned(),
            outcome,
        },
    )?;
    Ok(outcome)
}

/// Write the managed default routes under the router state dir and return its
/// path. Regenerated idempotently each launch so anchor changes roll forward.
pub fn write_default_local_routes(home: &Path) -> io::Result<PathBuf> {
    let dir = home.join(crate::ROUTER_STATE_DIR);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("local-default-routes.json");
    let body = serde_json::to_vec_pretty(&default_local_routes_json()).map_err(io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn marker(schema: u32, outcome: OnboardingOutcome) -> OnboardingMarker {
        OnboardingMarker {
            schema,
            completed_at: 0,
            cli_version: "t".into(),
            outcome,
        }
    }

    #[test]
    fn decide_uses_existing_when_engageable() {
        assert_eq!(decide(true, true, None, 1), OnboardingDecision::UseExisting);
    }

    #[test]
    fn decide_declines_when_non_interactive() {
        assert_eq!(decide(false, false, None, 1), OnboardingDecision::Decline);
    }

    #[test]
    fn decide_runs_wizard_on_first_run() {
        assert_eq!(decide(false, true, None, 1), OnboardingDecision::RunWizard);
    }

    #[test]
    fn decide_runs_wizard_on_stale_schema() {
        let m = marker(0, OnboardingOutcome::LocalModel);
        assert_eq!(
            decide(false, true, Some(&m), 1),
            OnboardingDecision::RunWizard
        );
    }

    #[test]
    fn decide_re_prompts_after_skip() {
        let m = marker(1, OnboardingOutcome::Skipped);
        assert_eq!(
            decide(false, true, Some(&m), 1),
            OnboardingDecision::RunWizard
        );
    }

    #[test]
    fn decide_declines_when_configured_but_unusable() {
        // outcome recorded, schema current, but model not engageable (e.g. GGUF
        // deleted): do NOT re-prompt — warn/decline instead.
        let m = marker(1, OnboardingOutcome::LocalModel);
        assert_eq!(
            decide(false, true, Some(&m), 1),
            OnboardingDecision::Decline
        );
    }

    fn tmp_home() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rl-onboard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config").join("rayline")).unwrap();
        dir
    }

    #[test]
    fn marker_round_trip_preserves_fields_and_sibling_keys() {
        let home = tmp_home();
        // Seed an unrelated key the writer must preserve.
        crate::status::write_settings(&home, &json!({ "local_model": { "mode": "recommended" } }))
            .unwrap();

        let marker = OnboardingMarker {
            schema: ONBOARDING_SCHEMA,
            completed_at: 1_750_000_000,
            cli_version: "9.9.9".to_owned(),
            outcome: OnboardingOutcome::LocalModel,
        };
        write_onboarding_in_home(&home, &marker).unwrap();

        let read = read_onboarding(&home).unwrap();
        assert_eq!(read.schema, ONBOARDING_SCHEMA);
        assert_eq!(read.completed_at, 1_750_000_000);
        assert_eq!(read.cli_version, "9.9.9");
        assert_eq!(read.outcome, OnboardingOutcome::LocalModel);

        // Sibling key survived the merge-preserving write.
        let settings = crate::status::read_settings(&home).unwrap();
        assert_eq!(settings["local_model"]["mode"], "recommended");
    }

    #[test]
    fn read_onboarding_none_when_absent() {
        let home = tmp_home();
        assert!(read_onboarding(&home).is_none());
    }

    #[test]
    fn default_routes_json_lists_read_only_allowlist_as_local() {
        let value = default_local_routes_json();
        let subagents = value["routes"]["subagents"].as_object().unwrap();
        assert_eq!(subagents.len(), LOCAL_DEFAULT_SUBAGENTS.len());
        for name in LOCAL_DEFAULT_SUBAGENTS {
            assert_eq!(subagents[*name]["endpoint"], "local");
        }
        // Explore is the guaranteed anchor.
        assert!(subagents.contains_key("Explore"));
    }

    #[test]
    fn write_default_local_routes_is_idempotent() {
        let home = tmp_home();
        let a = write_default_local_routes(&home).unwrap();
        let b = write_default_local_routes(&home).unwrap();
        assert_eq!(a, b);
        let text = std::fs::read_to_string(&a).unwrap();
        assert!(text.contains("\"Explore\""));
        assert!(text.contains("\"endpoint\": \"local\""));
    }

    #[tokio::test]
    async fn ensure_local_model_declines_non_interactively_without_marker() {
        // Run under a temp HOME with no local_model. In CI/tests stdin is not a
        // TTY, so `decide` returns Decline → NotConfigured, no marker written.
        let home = tmp_home();
        let readiness = ensure_local_model(&home, "prod").await.unwrap();
        assert!(matches!(readiness, LocalModelReadiness::NotConfigured));
        assert!(read_onboarding(&home).is_none());
    }

    #[tokio::test]
    async fn run_onboard_command_declines_non_interactively() {
        // The test harness is non-interactive (stdin/stdout are not TTYs).
        // run_onboard_command must return Ok without writing an onboarding marker
        // or touching local_model config.
        let home = tmp_home();
        // Pre-assert: no marker exists.
        assert!(read_onboarding(&home).is_none());

        // run_onboard_command uses dirs::home_dir(), not our tmp_home, but we
        // only care that it returns Ok and does not panic or download anything.
        let result = run_onboard_command(None, false).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        // No marker should have been written to our tmp home (wizard never ran).
        assert!(read_onboarding(&home).is_none());
    }
}

//! First-run onboarding for `<cli> claude --local`: the wizard that configures a
//! local model, the persisted completion marker, and the conservative read-only
//! default routes.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::status;

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
}

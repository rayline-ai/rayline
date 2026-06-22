//! First-run onboarding for `<cli> claude --local`: the wizard that configures a
//! local model, the persisted completion marker, and the conservative read-only
//! default routes.

use std::io;
use std::path::Path;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}

//! Hermes Agent: `HERMES_HOME=<dir>` holds `config.yaml` (plus Hermes' own
//! state: sessions, memories, skills). `key_env` names the key's env var and
//! `${VAR}` is expanded in config values.
//!
//! Episode keying: Hermes sends its session id as `x-session-affinity`, which
//! the gateway keys the episode on, and `-c` / `--resume` keeps it. Upstream
//! Hermes (v2026.8.27) sends no custom headers on `anthropic_messages`; the
//! atlasfutures fork (`rayline/session-headers`, router-infra#94) adds the
//! header on both wires. The `rayline-resp` provider (`--provider
//! rayline-resp`) speaks Responses. Each entry has its own base_url because
//! Hermes looks up `extra_headers` by base_url.

use serde_json::json;

use super::{CONTEXT_WINDOW, CONV_ID_ENV, KEY_ENV, Plan, ROUTER_MODEL, RenderContext, yaml_file};

pub(crate) const CONFIG_FILE: &str = "config.yaml";
pub(crate) const MESSAGES_PROVIDER: &str = "rayline-anth";
pub(crate) const RESPONSES_PROVIDER: &str = "rayline-resp";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let models = json!({ ROUTER_MODEL: { "context_length": CONTEXT_WINDOW } });
    let mut responses = json!({
        "base_url": ctx.v1_base(),
        "key_env": KEY_ENV,
        "api_mode": "codex_responses",
        "models": models
    });
    if ctx.conversation_id.enabled(true) {
        responses["extra_headers"] = json!({ super::CONV_HEADER: format!("${{{CONV_ID_ENV}}}") });
    }
    let config = json!({
        "model": {
            "provider": format!("custom:{MESSAGES_PROVIDER}"),
            "default": ROUTER_MODEL,
            "context_length": CONTEXT_WINDOW
        },
        "providers": {
            MESSAGES_PROVIDER: {
                "base_url": ctx.messages_base(),
                "key_env": KEY_ENV,
                "api_mode": "anthropic_messages",
                "models": models
            },
            RESPONSES_PROVIDER: responses
        },
        "fallback_providers": []
    });
    Plan {
        files: vec![yaml_file(CONFIG_FILE, &config, true)],
        env_set: vec![("HERMES_HOME", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{env_value, render};
    use super::CONFIG_FILE;

    #[test]
    fn renders_messages_default_and_responses_provider() {
        let (dir, plan) = render(Harness::Hermes);
        assert_eq!(
            env_value(&plan, "HERMES_HOME"),
            Some(&dir.clone().into_os_string())
        );
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert_eq!(
            config,
            "fallback_providers: []
model:
  context_length: 200000
  default: \"rayline-router\"
  provider: \"custom:rayline-anth\"
providers:
  rayline-anth:
    api_mode: \"anthropic_messages\"
    base_url: \"https://api-dev.rayline.ai\"
    key_env: \"RAYLINE_KEY\"
    models:
      rayline-router:
        context_length: 200000
  rayline-resp:
    api_mode: \"codex_responses\"
    base_url: \"https://api-dev.rayline.ai/v1\"
    key_env: \"RAYLINE_KEY\"
    models:
      rayline-router:
        context_length: 200000
"
        );
    }
}

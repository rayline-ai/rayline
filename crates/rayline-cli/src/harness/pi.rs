//! pi: `PI_CODING_AGENT_DIR=<dir>`, which holds `models.json`, `settings.json`,
//! `auth.json` and `sessions/`. pi resolves `$VAR` in `apiKey` and header values.
//!
//! `compat.sendSessionAffinityHeaders` makes pi send its session id as
//! `x-session-affinity` on Messages (pi 0.87.1 and 1.0.0; off by default for
//! non-OpenRouter endpoints), which the gateway keys the episode on. `-c` and
//! `--resume` reuse the session id, so a resumed session keeps its episode.

use serde_json::json;

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, KEY_ENV, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext,
    json_file,
};

pub(crate) const PROVIDER_ID: &str = "rayline";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "api": "anthropic-messages",
                "apiKey": format!("${KEY_ENV}"),
                "headers": ctx.conversation_headers(&format!("${CONV_ID_ENV}")),
                "compat": { "sendSessionAffinityHeaders": true },
                "models": [{
                    "id": ROUTER_MODEL,
                    "name": format!("Rayline router ({})", ctx.env_name),
                    "reasoning": true,
                    "input": ["text", "image"],
                    "contextWindow": CONTEXT_WINDOW,
                    "maxTokens": MAX_OUTPUT_TOKENS
                }]
            }
        }
    });
    // pi writes settings.json itself (model picks, theme), so only seed it.
    let settings = json!({
        "defaultProvider": PROVIDER_ID,
        "defaultModel": ROUTER_MODEL
    });
    Plan {
        files: vec![
            json_file("models.json", &models, true),
            json_file("settings.json", &settings, false),
        ],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_URL, env_value, render};

    #[test]
    fn renders_models_and_default_settings() {
        let (dir, plan) = render(Harness::Pi);
        assert_eq!(
            env_value(&plan, "PI_CODING_AGENT_DIR"),
            Some(&dir.clone().into_os_string())
        );
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let provider = &read("models.json")["providers"]["rayline"];
        assert_eq!(provider["baseUrl"], TEST_URL);
        assert_eq!(provider["api"], "anthropic-messages");
        assert_eq!(provider["apiKey"], "$RAYLINE_KEY");
        assert_eq!(provider["compat"]["sendSessionAffinityHeaders"], true);
        assert_eq!(provider["headers"], serde_json::json!({}));
        let model = &provider["models"][0];
        assert_eq!(model["id"], "rayline-router");
        assert_eq!(model["contextWindow"], 200000);
        assert_eq!(model["maxTokens"], 32000);
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "rayline");
        assert_eq!(settings["defaultModel"], "rayline-router");
    }
}

//! opencode: `OPENCODE_CONFIG=<dir>/opencode.json`. opencode merges this file
//! over the user's global config (project configs still win). `{env:VAR}` is
//! opencode's config substitution.

use serde_json::json;

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, KEY_ENV, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext,
    conversation_headers, json_file,
};

pub(crate) const CONFIG_FILE: &str = "opencode.json";
pub(crate) const PROVIDER_ID: &str = "rayline";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let model_ref = format!("{PROVIDER_ID}/{ROUTER_MODEL}");
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model_ref,
        // The session-title side call otherwise goes to the user's global
        // `small_model` (another provider) or a guessed small model. Keep it on
        // Rayline. It still carries the conversation header and opens the
        // episode as turn 0 until the gateway branches side calls (router-infra#87).
        "small_model": model_ref,
        "provider": {
            PROVIDER_ID: {
                "name": format!("Rayline ({})", ctx.env_name),
                // Messages is the only shape that reaches VSR; opencode's default
                // for custom providers (chat completions) 404s at the gateway.
                "npm": "@ai-sdk/anthropic",
                "options": {
                    "baseURL": ctx.v1_base(),
                    "apiKey": format!("{{env:{KEY_ENV}}}"),
                    "headers": conversation_headers(&format!("{{env:{CONV_ID_ENV}}}")),
                },
                "models": {
                    ROUTER_MODEL: {
                        "name": "Rayline router",
                        "limit": { "context": CONTEXT_WINDOW, "output": MAX_OUTPUT_TOKENS },
                        "modalities": { "input": ["text", "image"], "output": ["text"] },
                        "reasoning": true,
                        "tool_call": true,
                        "attachment": true
                    }
                }
            }
        }
    });
    Plan {
        files: vec![json_file(CONFIG_FILE, &config, true)],
        env_set: vec![(
            "OPENCODE_CONFIG",
            ctx.config_dir.join(CONFIG_FILE).into_os_string(),
        )],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_URL, env_value, render};
    use super::*;

    #[test]
    fn renders_messages_provider_with_env_refs() {
        let (dir, plan) = render(Harness::OpenCode);
        let path = dir.join(CONFIG_FILE);
        assert_eq!(
            env_value(&plan, "OPENCODE_CONFIG"),
            Some(&path.clone().into_os_string())
        );
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["model"], "rayline/rayline-router");
        assert_eq!(config["small_model"], "rayline/rayline-router");
        let provider = &config["provider"]["rayline"];
        assert_eq!(provider["npm"], "@ai-sdk/anthropic");
        assert_eq!(provider["options"]["baseURL"], format!("{TEST_URL}/v1"));
        assert_eq!(provider["options"]["apiKey"], "{env:RAYLINE_KEY}");
        assert_eq!(
            provider["options"]["headers"]["x-conversation-id"],
            "{env:RAYLINE_CONV_ID}"
        );
        let model = &provider["models"]["rayline-router"];
        assert_eq!(model["limit"]["context"], 200000);
        assert_eq!(model["limit"]["output"], 32000);
        assert_eq!(model["tool_call"], true);
    }
}

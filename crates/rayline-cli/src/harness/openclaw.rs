//! OpenClaw: `OPENCLAW_CONFIG_PATH=<dir>/openclaw.json` (state stays in the
//! user's `~/.openclaw`). `${VAR}` is OpenClaw's config substitution.
//!
//! `api` must be explicit: OpenClaw's default is chat completions, which 404s
//! at the gateway. Memory search is off because its default embeddings
//! provider sends side calls to OpenAI; heartbeat and the compaction memory
//! flush are off so no background turns hit the router.

use serde_json::json;

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, KEY_ENV, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext,
    conversation_headers, json_file,
};

pub(crate) const CONFIG_FILE: &str = "openclaw.json";
pub(crate) const PROVIDER_ID: &str = "rayline";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let config = json!({
        "memory": { "search": { "provider": "none" } },
        "agents": {
            "defaults": {
                "model": { "primary": format!("{PROVIDER_ID}/{ROUTER_MODEL}") },
                "heartbeat": { "every": "0m" },
                "compaction": { "memoryFlush": { "enabled": false } }
            }
        },
        "models": {
            "mode": "merge",
            "providers": {
                PROVIDER_ID: {
                    "baseUrl": ctx.messages_base(),
                    "apiKey": format!("${{{KEY_ENV}}}"),
                    "api": "anthropic-messages",
                    "headers": conversation_headers(&format!("${{{CONV_ID_ENV}}}")),
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
        }
    });
    Plan {
        files: vec![json_file(CONFIG_FILE, &config, true)],
        env_set: vec![(
            "OPENCLAW_CONFIG_PATH",
            ctx.config_dir.join(CONFIG_FILE).into_os_string(),
        )],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_URL, env_value, render};
    use super::CONFIG_FILE;

    #[test]
    fn renders_explicit_api_and_disables_side_calls() {
        let (dir, plan) = render(Harness::OpenClaw);
        let path = dir.join(CONFIG_FILE);
        assert_eq!(
            env_value(&plan, "OPENCLAW_CONFIG_PATH"),
            Some(&path.clone().into_os_string())
        );
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["memory"]["search"]["provider"], "none");
        let defaults = &config["agents"]["defaults"];
        assert_eq!(defaults["model"]["primary"], "rayline/rayline-router");
        assert_eq!(defaults["heartbeat"]["every"], "0m");
        assert_eq!(defaults["compaction"]["memoryFlush"]["enabled"], false);
        let provider = &config["models"]["providers"]["rayline"];
        assert_eq!(config["models"]["mode"], "merge");
        assert_eq!(provider["baseUrl"], TEST_URL);
        assert_eq!(provider["api"], "anthropic-messages");
        assert_eq!(provider["apiKey"], "${RAYLINE_KEY}");
        assert_eq!(
            provider["headers"]["x-conversation-id"],
            "${RAYLINE_CONV_ID}"
        );
        assert_eq!(provider["models"][0]["contextWindow"], 200000);
        assert_eq!(provider["models"][0]["maxTokens"], 32000);
    }
}

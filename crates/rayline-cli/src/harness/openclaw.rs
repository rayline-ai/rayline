//! OpenClaw: `OPENCLAW_CONFIG_PATH=<dir>/openclaw.json` (state stays in the
//! user's `~/.openclaw`). `${VAR}` is OpenClaw's config substitution.
//!
//! `api` must be explicit: OpenClaw's default is chat completions, which 404s
//! at the gateway. Memory search is off because its default embeddings
//! provider sends side calls to OpenAI; heartbeat and the compaction memory
//! flush are off so no background turns hit the router.
//!
//! Episode keying (OpenClaw 2026.9.6; `compat` is a model field there, the
//! provider schema has none):
//! - Messages sends no session id, so the `rayline` provider always carries
//!   the per-launch `x-conversation-id` (unless
//!   `RAYLINE_HARNESS_CONV_ID_FALLBACK=0`). A custom `anthropic-messages`
//!   provider runs on OpenClaw's own fetch transport (strategy
//!   `boundary-aware:anthropic-messages`, `createAnthropicMessagesTransportStreamFn`
//!   / `createAnthropicTransportClient` in `dist/worker/worker.mjs`), which
//!   adds no `x-session-affinity`; only the Anthropic SDK path
//!   (`@openclaw/ai/dist/anthropic-DZQ_7gD1.mjs:309`) honours
//!   `compat.sendSessionAffinityHeaders`, so that flag is not set. One launch
//!   is one episode; a resume in a new launch starts a new one.
//! - The `rayline-resp` provider (model `rayline-resp/rayline-router`) speaks
//!   Responses, where `compat.supportsPromptCacheKey` sends the session as
//!   `prompt_cache_key`; without it OpenClaw sends no key on a custom
//!   endpoint. It takes the per-launch header only where the env needs it.

use serde_json::json;

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, KEY_ENV, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext,
    json_file,
};

pub(crate) const CONFIG_FILE: &str = "openclaw.json";
pub(crate) const PROVIDER_ID: &str = "rayline";
pub(crate) const RESPONSES_PROVIDER_ID: &str = "rayline-resp";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let api_key = format!("${{{KEY_ENV}}}");
    let conv_ref = format!("${{{CONV_ID_ENV}}}");
    // No native session id on Messages (see the module docs).
    let messages_headers = ctx.conversation_headers_for(&conv_ref, false);
    let responses_headers = ctx.conversation_headers(&conv_ref);
    let model = |compat: serde_json::Value| {
        json!({
            "id": ROUTER_MODEL,
            "name": format!("Rayline router ({})", ctx.env_name),
            "reasoning": true,
            "input": ["text", "image"],
            "contextWindow": CONTEXT_WINDOW,
            "maxTokens": MAX_OUTPUT_TOKENS,
            "compat": compat
        })
    };
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
                    "apiKey": api_key,
                    "api": "anthropic-messages",
                    "headers": messages_headers,
                    "models": [model(json!({}))]
                },
                RESPONSES_PROVIDER_ID: {
                    "baseUrl": ctx.v1_base(),
                    "apiKey": api_key,
                    "api": "openai-responses",
                    "headers": responses_headers,
                    "models": [model(json!({ "supportsPromptCacheKey": true }))]
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
        // dev: Messages still carries the per-launch id, Responses does not.
        assert_eq!(
            provider["headers"],
            serde_json::json!({ "x-conversation-id": "${RAYLINE_CONV_ID}" })
        );
        let model = &provider["models"][0];
        assert_eq!(model["contextWindow"], 200000);
        assert_eq!(model["maxTokens"], 32000);
        assert_eq!(model["compat"], serde_json::json!({}));
        let responses = &config["models"]["providers"]["rayline-resp"];
        assert_eq!(responses["baseUrl"], format!("{TEST_URL}/v1"));
        assert_eq!(responses["api"], "openai-responses");
        assert_eq!(responses["apiKey"], "${RAYLINE_KEY}");
        assert_eq!(responses["headers"], serde_json::json!({}));
        assert_eq!(responses["models"][0]["id"], "rayline-router");
        assert_eq!(
            responses["models"][0]["compat"]["supportsPromptCacheKey"],
            true
        );
    }

    #[test]
    fn messages_header_follows_the_override() {
        use super::super::ConversationIdFallback;
        use super::super::tests::render_with;
        let headers = |fallback| {
            let (dir, _) = render_with(Harness::OpenClaw, fallback);
            let config: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap())
                    .unwrap();
            let providers = &config["models"]["providers"];
            (
                providers["rayline"]["headers"].clone(),
                providers["rayline-resp"]["headers"].clone(),
            )
        };
        let header = serde_json::json!({ "x-conversation-id": "${RAYLINE_CONV_ID}" });
        let none = serde_json::json!({});
        assert_eq!(
            headers(ConversationIdFallback::new("prod", None)),
            (header.clone(), header.clone())
        );
        assert_eq!(
            headers(ConversationIdFallback::new("dev", Some("0"))),
            (none.clone(), none.clone())
        );
        assert_eq!(
            headers(ConversationIdFallback::new("prod", Some("0"))),
            (none.clone(), none)
        );
    }
}

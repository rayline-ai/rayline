//! omp (oh-my-pi): `PI_CODING_AGENT_DIR=<dir>` holds `models.yml` and
//! `config.yml`. A named profile (`OMP_PROFILE`/`PI_PROFILE`) makes omp ignore
//! that override, so both are removed. omp also reads Claude Code's config from
//! `CLAUDE_CONFIG_DIR` (MCP servers, skills); the smoke runs showed a
//! `CLAUDE_CONFIG_DIR` inherited from `rayline claude` pulling another config
//! into omp, so it is removed too. `apiKey` and header values name env vars.

use serde_json::json;

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, KEY_ENV, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext,
    conversation_headers, yaml_file,
};

pub(crate) const PROVIDER_ID: &str = "rayline";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "apiKey": KEY_ENV,
                "api": "anthropic-messages",
                // Without `auth: apiKey` omp poses as Claude Code (renamed tools,
                // billing system block). apiKey mode is the honest request shape.
                "auth": "apiKey",
                "headers": conversation_headers(CONV_ID_ENV),
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
    let model_ref = format!("{PROVIDER_ID}/{ROUTER_MODEL}");
    // omp writes its settings to config.yml, so only seed it.
    let config = json!({
        "modelRoles": {
            "default": model_ref,
            "smol": model_ref,
            "slow": model_ref,
            "plan": model_ref
        }
    });
    Plan {
        files: vec![
            yaml_file("models.yml", &models, true),
            yaml_file("config.yml", &config, false),
        ],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: vec!["CLAUDE_CONFIG_DIR", "OMP_PROFILE", "PI_PROFILE"],
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{env_value, render};

    #[test]
    fn renders_api_key_mode_and_clears_claude_config_dir() {
        let (dir, plan) = render(Harness::Omp);
        assert_eq!(
            env_value(&plan, "PI_CODING_AGENT_DIR"),
            Some(&dir.clone().into_os_string())
        );
        assert!(plan.env_remove.contains(&"CLAUDE_CONFIG_DIR"));
        let models = std::fs::read_to_string(dir.join("models.yml")).unwrap();
        assert_eq!(
            models,
            "providers:
  rayline:
    api: \"anthropic-messages\"
    apiKey: \"RAYLINE_KEY\"
    auth: \"apiKey\"
    baseUrl: \"https://api-dev.rayline.ai\"
    headers:
      x-conversation-id: \"RAYLINE_CONV_ID\"
    models:
      - contextWindow: 200000
        id: \"rayline-router\"
        input: [\"text\", \"image\"]
        maxTokens: 32000
        name: \"Rayline router (dev)\"
        reasoning: true
"
        );
        let config = std::fs::read_to_string(dir.join("config.yml")).unwrap();
        assert!(config.starts_with("modelRoles:\n"));
        for role in ["default", "smol", "slow", "plan"] {
            assert!(config.contains(&format!("  {role}: \"rayline/rayline-router\"\n")));
        }
    }
}

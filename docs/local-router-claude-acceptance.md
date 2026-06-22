# Local Router Claude Acceptance

This guide validates the real Claude Code path through Rayline Local's local
router and managed llama.cpp runtime.

Prerequisites:

- Claude Code is installed and already logged in.
- Rust `1.88.0` is installed when testing from source.
- `jq` is available for JSON inspection.

## Install Matching Binaries

From a Rayline Local source checkout:

```bash
cargo install --path crates/rayline-daemon --locked --root ~/.rayline --force
cargo install --path crates/rayline-cli --locked --root ~/.rayline --force
export PATH="$HOME/.rayline/bin:$PATH"

rayline --version
rld --version
```

The two versions should match. If you are testing an already installed release,
skip this step and use the installed `rayline`.

## Select a Local Model

Use a managed recommended model:

```bash
rayline local models
rayline local use qwen3.6-35b-a3b-q4km
rayline local show
```

First use may download the model. First launch may take a few minutes while
llama.cpp loads it.

## Configure Explore Routing

Create a config that routes only the `Explore` subagent to the local model:

```bash
mkdir -p ~/.config/rayline

cat > ~/.config/rayline/local-router-explore-local.json <<'JSON'
{
  "routes": {
    "subagents": {
      "Explore": { "endpoint": "local" },
      "explore": { "endpoint": "local" }
    }
  }
}
JSON
```

## Clear Stale Test State

Stop any previous isolated Claude Code daemon and Rayline Local router:

```bash
CLAUDE_CONFIG_DIR="$HOME/.rayline/cc" claude daemon stop --any || true
rayline router stop || true
```

## Run Claude Through Rayline Local

```bash
rayline claude \
  --local \
  --isolated \
  --route subagents \
  --router-config-path ~/.config/rayline/local-router-explore-local.json \
  -- \
  -p 'Use the Explore subagent for this task. Ask the Explore subagent to reply exactly: RAYLINE_EXPLORE_ACCEPTANCE_OK. After the Explore subagent returns, reply exactly: RAYLINE_EXPLORE_ACCEPTANCE_OK.'
```

Expected stdout:

```text
RAYLINE_EXPLORE_ACCEPTANCE_OK
```

## Verify Routing

Check the router:

```bash
rayline router status
rayline router top --json \
  | jq '.active[]?, .recent[]? | select(((.agent_type // "") | ascii_downcase) == "explore" and .target == "local")'
```

A passing row shows `agent_type` as `Explore`, `target` as `local`, and a local
selected model.

Check logs:

```bash
tail -n 160 ~/.rayline/rld/rl-rld.log \
  | grep -i 'policy=subagent:Explore'

tail -n 160 ~/.rayline/rld/cc/rl-rld-proxy.log \
  | grep -i 'selective_subagent_header'
```

The proxy log should include:

```text
target=Router reason=selective_subagent_header
```

## Troubleshooting

If Claude reports a non-Rayline Local daemon conflict, stop only the isolated
daemon and retry:

```bash
CLAUDE_CONFIG_DIR="$HOME/.rayline/cc" claude daemon stop --any || true
```

If `Explore` does not route locally, confirm the command used
`--route subagents` (the default for `--local`) and `--router-config-path`.
Main-thread Claude traffic should remain passthrough in this mode.

If the model is slow or missing, watch:

```bash
tail -f ~/.rayline/rld/rl-rld.log
tail -f ~/.rayline/rld/llama-server.log
```

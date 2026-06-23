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

---

## `--local` Onboarding Acceptance

These steps require a real machine with a TTY (no headless runner). Run them after
the automated gate (`fmt` / `clippy` / `test`) is green.

### Prerequisites

```bash
# Confirm the binary is fresh (should match current branch build)
rayline --version
```

### Step 2: First-run onboarding (interactive wizard)

```bash
# Reset to a clean slate
rayline local clear
# Also remove the "onboarding" marker so the first-run wizard re-triggers.
# (portable; macOS has no `sponge`)
tmp=$(mktemp) && jq 'del(.onboarding)' ~/.config/rayline/settings.json > "$tmp" && mv "$tmp" ~/.config/rayline/settings.json

rayline claude --local
# (Tip: `rayline local onboard` re-runs the wizard directly, no settings edit needed.)
```

**Expected:**
- The wizard prints the recommendation headline + 4 choices: `[Y] download & use recommended (default) / [m] see all models / [o] use my own server (Ollama / LM Studio / llama.cpp) / [s] skip — stay on cloud`
- Choosing `[Y]` downloads and selects the recommended model
- `cat ~/.config/rayline/settings.json | jq .local_model` shows a `local_model` block (endpoint, model_id)
- `cat ~/.config/rayline/settings.json | jq .onboarding` shows `schema: 1`, `outcome: "local-model"`, a `completed_at` timestamp, and `cli_version`
- After the wizard, Claude routes only `Explore`/`codebase-*` subagents locally; main thread and other agents stay cloud
- Verify: `rayline router top` shows `policy=subagent:Explore target=local` rows; main rows stay `target=cloud`
- Or: `grep 'policy=subagent:Explore' ~/.rayline/rld/rl-rld.log`

### Step 3: Re-run + `--reset`

```bash
rayline local onboard            # wizard re-runs; keeps local_model unless changed
rayline local onboard --reset    # clears local_model first, then wizard
```

**Expected:**
- After `rayline local onboard`: `.onboarding.completed_at` in settings.json is updated to a newer timestamp
- After `rayline local onboard --reset`: `.local_model` is cleared before the wizard starts, then re-filled on `[Y]`

### Step 4: Non-interactive fallback

```bash
rayline local clear
echo "" | rayline claude --local -p 'hello'
```

**Expected:**
- No wizard prompt appears
- A clear "No local model configured…" (or similar) message is printed
- Non-zero exit code (`echo $?` should be non-zero)
- `cat ~/.config/rayline/settings.json | jq .onboarding` is `null` (no marker written)

### Step 5: Skipped → re-prompt

```bash
rayline local clear
rayline claude --local        # when prompted, choose [s] (skip) → outcome "skipped"
rayline claude --local        # MUST re-prompt even though a skip marker was recorded
```

**Expected:**
- First run writes `.onboarding.outcome = "skipped"` to settings.json
- Second run (with explicit `--local`) shows the wizard again — skip is not sticky for explicit `--local`

### Step 6: `--route all` widens past the allowlist

```bash
rayline local clear
rayline local use <some-model>   # configure a model without going through onboarding
rayline claude --local --route all
```

**Expected:**
- All subagents (not just the read-only allowlist) route locally
- Main thread stays cloud
- The materialized `local-default-routes.json` (written at launch under `~/.rayline/rld/`) is NOT applied — `--route all` (Proxy mode) bypasses it
- Verify via `rayline router top`: subagent rows show `target=local`, main rows show `target=cloud`

### Step 7: Local provider endpoint

With Ollama running and at least one model installed:

```bash
ollama serve
ollama list
rayline claude --local --local-provider ollama --model <ollama-tag> -p 'Use the Explore subagent to list the top-level files in this repo.'
```

**Expected:**
- `rayline local show` reports `Local model: Ollama`, the provider URL, the selected model, and `Protocol: openai_chat`
- `cat ~/.config/rayline/settings.json | jq .local_model` includes `"mode": "custom"`, `"provider": "ollama"`, `"protocol": "openai_chat"`, root `"base_url": "http://localhost:11434"`, and the selected model
- `~/.rayline/rld/provider-routes.json` contains an `openai_chat` endpoint named `ollama` plus `routes.subagents`, `routes.subagent`, and `routes.model_routes` entries pointing to that endpoint
- `~/.rayline/rld/rl-rld.log` shows custom upstream mode and does **not** show GGUF download or bundled `llama-server spawned`
- `rayline router top` shows Explore/subagent rows targeting the provider endpoint; main rows stay cloud

Stopped-provider failure:

```bash
rayline router stop
# stop ollama / LM Studio
rayline claude --local --local-provider ollama --model <ollama-tag> -p 'hello'
```

**Expected:**
- The command exits non-zero
- The error names the provider URL and prints the start hint (`ollama serve`)
- It does not silently fall back to cloud routing

Auto-discovery picker:

```bash
ollama serve
rayline local models
rayline local use <provider-row-number>
rayline claude --local -p 'hello'
```

**Expected:**
- `rayline local models` lists detected provider models before built-in models
- Selecting a provider row configures it immediately (no GGUF download)
- A later bare `rayline claude --local` reuses the persisted provider config and regenerates provider routes

---

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

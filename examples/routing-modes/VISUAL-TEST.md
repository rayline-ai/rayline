# Routing-mode verification runbook (for an agent or a human)

This is a self-contained runbook for **you (the agent) to execute**: run each
supported `--config` routing mode end-to-end with `visual-test.sh`, **one at a
time**, observe the routing, judge it against the criteria here, and write a report.
Run modes individually — do **not** wrap them in one long script: local-main modes
hang, and a per-mode loop lets you recover and continue.

> Honest scope: the *config-driven* routing (which endpoint each class hits, and the
> pinned/local models) is deterministic and verifiable. The *hosted* parts — the
> cloud router's model pick, and the may-local redirect — are **non-deterministic
> and account-gated**, so those rows are **REVIEW** (record what happened, judge by
> the rules below), not mechanical PASS/FAIL.

---

## 1. Prerequisites (verify all; if one is missing, note it — do not work around it)

| # | Requirement | Check | If missing |
|---|---|---|---|
| 1 | `rayline`/`rld` built from the branch under test, installed, **all stale `rld` killed** | `rayline --version`; `pgrep -fl "rld serve\|rld proxy"` → none | rebuild + install (Apple Silicon: `codesign --force --sign -` after copying); `pkill -9 -f "rld serve\|rld proxy"` |
| 2 | Signed in to rayline | `rayline auth status` (or `rayline local show` works) | `rayline auth login` |
| 3 | `ANTHROPIC_API_KEY` exported | `echo $ANTHROPIC_API_KEY` | RAC/RAL/LA subagent leg will **FAIL** — mark those rows accordingly, don't hide it |
| 4 | `claude` (Claude Code) on PATH + Claude **subscription** logged in | `command -v claude` | passthrough modes (ARC/ARCL/ARL/AL main) can't run |
| 5 | ollama up with both models | `curl -s localhost:11434/api/tags` shows `qwen3.5:9b` + `qwen2.5-coder:7b` | `ollama pull qwen3.5:9b && ollama pull qwen2.5-coder:7b` |
| 6 | local model configured + ON | `rayline local show` → "Custom endpoint … qwen2.5-coder:7b" + "account: ON" | `rayline local custom --url http://127.0.0.1:11434 --model qwen2.5-coder:7b`; the script runs `rayline local on` |
| 7 | `python3` available | `python3 --version` | install python3 |
| 8 | ports 20809–20813 free | script handles via killing `rld` | — |

asciinema/tmux are **not** required (the script runs the demo headless).

---

## 2. Procedure — run each mode yourself, sequentially

Set once (the prompt forces a main turn + an `Explore` per-type subagent + a
`general-purpose` default subagent, so every routing slot is exercised):

```bash
RB="$(command -v rayline)"   # or the absolute path to the branch-under-test binary
PROMPT='Launch TWO subagents with the Task tool, in parallel: (1) an Explore subagent and (2) a general-purpose subagent. Each must reply with exactly the word PONG. Do not use any other tool first. After both return, output DONE.'
```

Then, **for each MODE in this order** — `RRC RAC RLC RRCL RRL RAL RLC-per-type ARC
ARCL AL LRC LRL LA LL RLL ARL` (skip `RACL RLCL LRCL`, see §5) — do exactly this:

```bash
# a) clean slate so `rayline top` reflects only this mode (each run restarts rld)
pkill -9 -f "rld serve|rld proxy"; sleep 1
# b) run the mode's demo headless, with a ~100s timeout (local-main modes hang — expected)
DEMO_HEADLESS=1 RAYLINE_BIN="$RB" ./examples/routing-modes/visual-test.sh "$MODE" "$PROMPT" >/tmp/v-$MODE.out 2>&1 &
P=$!; for i in $(seq 1 20); do sleep 5; kill -0 $P 2>/dev/null || break; done; kill -9 $P 2>/dev/null
# c) read what actually routed (do NOT trust the demo's own summary if it was killed):
"$RB" top --json --all     # per request: agent_type, target, selected_model, policy
grep -i "local route endpoint" ~/.rayline/rld/rl-rld.log | tail -8   # endpoint-level detail for LSR modes
grep -i "DONE" /tmp/v-$MODE.out >/dev/null && echo "run finished" || echo "run did NOT finish (expected for local-main)"
```

Judge that mode against §3 (signals) + §4 (criteria), record one line per §6, then
move to the next mode. Don't batch them; one at a time.

---

## 3. How to read `target` / `policy` (the signals)

- `target=anthropic`, `policy=selective_main_passthrough` → **Claude subscription passthrough** (main of ARC/ARCL/ARL/AL).
- `target=local` or `policy` contains `local-adapter` / `:may-local` → **served by a local model** (the may-local redirect, or the LSR sending it local).
- `target=remote` → an upstream endpoint; use **`selected_model`** to tell which:
  - `qwen3.5:9b` / `qwen2.5-coder:7b` → **local (ollama)** routed by the LSR.
  - `GLM-5.2`, `deepseek/deepseek-v4-pro` → **cloud, pinned by the JSON** (LSR rewrote the model).
  - `claude-*`, `deepseek/deepseek-v4-flash`, etc. with no pin → **cloud RCR's own pick** (non-deterministic).
  - `claude-sonnet-4-6` on a **subagent** in RAC/RAL → the **anthropic API-key endpoint** (distinguish from an RCR pick by the mode's intent).

---

## 4. Per-mode criteria

`R/A/L` = main/subagent provider; "cloud(pick)" = RCR-chosen model (any cloud model, REVIEW).
"DONE finished" in the report means the run completed; local-main runs often won't.

| Mode | Expected main → | Expected subagents → | PASS when | Class |
|---|---|---|---|---|
| **RRC** | cloud (RCR pick) | cloud (RCR pick); Explore may→local (local ON) | main `target=remote`; subagents remote, OR Explore local (may-local) | REVIEW (cloud pick + may-local) |
| **RAC** | cloud (RCR pick) | anthropic (API key) | main remote; subagent `claude-sonnet-4-6` via anthropic | FAIL if `ANTHROPIC_API_KEY` unset |
| **RLC** | cloud (RCR pick) | **ollama** `qwen2.5-coder:7b` | main remote; subagents `selected_model=qwen2.5-coder:7b` | PASS (deterministic subagent) |
| **RRCL** § | cloud (RCR pick) | cloud + may-local | same as RRC; behaviorally ≡ RRC (may-local account-gated) | REVIEW |
| **RRL** | cloud **pinned `GLM-5.2`** | cloud **pinned `deepseek/deepseek-v4-pro`** | main `selected_model=GLM-5.2`; subagents `deepseek/deepseek-v4-pro` | **PASS (deterministic, exact models)** |
| **RAL** | cloud **pinned `GLM-5.2`** | anthropic (API key) | main `GLM-5.2`; subagent anthropic `claude-sonnet-4-6` | main PASS; subagent FAIL if no key |
| **RLC-per-type** | cloud (RCR pick) | `Explore`→ollama `qwen2.5-coder:7b`, `Plan`→ollama `qwen3.5:9b`, other→cloud | Explore `qwen2.5-coder:7b`; general-purpose remote (cloud) | **PASS (deterministic per-type)** |
| **ARC** | **subscription** (passthrough) | cloud (RCR pick) | main `target=anthropic`/`selective_main_passthrough`; subagents remote | REVIEW (cloud pick) |
| **ARCL** § | subscription | cloud + may-local | main passthrough; subagents remote or Explore→local | REVIEW |
| **AL** | subscription | **ollama** `qwen…` | main passthrough; subagents `qwen*` (local) | **PASS** (passthrough + local) |
| **LRC** ‡ | **ollama** `qwen3.5:9b` | cloud (if spawned) | main `selected_model=qwen3.5:9b` | REVIEW (local main usually spawns no subagents) |
| **LRL** ‡ | ollama `qwen3.5:9b` | cloud pinned `deepseek/deepseek-v4-pro` (if spawned) | main `qwen3.5:9b` | REVIEW (local main) |
| **LA** ‡ | ollama `qwen3.5:9b` | anthropic (API key, if spawned) | main `qwen3.5:9b` | REVIEW (local main + needs key) |
| **LL** ‡ | ollama `qwen3.5:9b` | ollama (if spawned) | main `qwen3.5:9b` | REVIEW (local main) |
| **RLL** | cloud **pinned `GLM-5.2`** | **ollama** `qwen2.5-coder:7b` | main `GLM-5.2`; subagents `qwen2.5-coder:7b` | **PASS (deterministic)** |
| **ARL** | subscription | cloud **pinned `deepseek/deepseek-v4-pro`** | main passthrough; subagents `deepseek/deepseek-v4-pro` | **PASS (deterministic subagent)** |

**§ may-local (RRCL/ARCL):** the redirect is the hosted RCR's **account-gated**
decision; with local ON it usually sends `Explore` to local, but it's discretionary.
Because the implicit account-local path also advertises, RRC/ARC behave the same —
so RRCL/ARCL are **not** distinguishable from RRC/ARC by routing alone. Mark REVIEW.

**‡ local-main (LRC/LRL/LA/LL):** the main runs on a small local model that usually
**cannot drive Claude Code's Task tool**, so subagents typically **do not spawn**.
Verify the **main** routes to ollama; treat absent subagents as the known limitation,
**not** a failure.

---

## 5. NOT SUPPORTED (do not run; mark as not supported)

| Mode | Why |
|---|---|
| **RACL** | `--local-model=on` is on the **main** (agent) class; the main never goes local (may-local is subagents-only) → the `CL` flag is **inert**; ≡ RAC. |
| **RLCL** | same as RACL on the main class → inert; ≡ RLC. |
| **LRCL** | subagent may-local would need the LSR to defer to the RCR (advertise + follow the 307); not implemented. The naive on-device shortcut collapsed to LL and was reverted. |

---

## 6. What to report

For each of the 16 supported modes, output one line:

`MODE — <PASS|REVIEW|FAIL> — main: <target/model>, subagents: <observed> — <note>`

- **PASS**: deterministic expectation met (RRL, RLL, ARL, RLC, RLC-per-type, AL).
- **REVIEW**: hosted/non-deterministic (cloud pick, may-local) or local-main — state what was observed and whether it's consistent with the expected intent.
- **FAIL**: a deterministic expectation was wrong, OR an env prereq blocked it (say which, e.g. "no ANTHROPIC_API_KEY").

Then list RACL/RLCL/LRCL as **NOT SUPPORTED**. Finish with a one-line summary
(e.g. "6 PASS, 8 REVIEW, 2 FAIL-due-to-missing-key, 3 not supported") and call out
any genuine routing bug (a deterministic row that didn't match).

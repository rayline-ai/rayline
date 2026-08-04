use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, WorkerManifest};

const CACHE_RETURN_WARM_SECONDS: f64 = 150.0;
const CACHE_RETURN_COLD_SECONDS: f64 = 300.0;
const CACHE_RETURN_WARM_HIT_RATIO: f64 = 0.8;
const CACHE_RETURN_COLD_HIT_RATIO: f64 = 0.2;

#[derive(Clone, Debug)]
pub struct WorkerWarmth {
    pub last_used: Instant,
    pub last_input_tokens: usize,
}

#[derive(Clone, Debug, Default)]
pub struct EpisodeState {
    pub previous_arm: Option<usize>,
    pub turn_index: u64,
    pub warmth: Vec<Option<WorkerWarmth>>,
}

impl EpisodeState {
    pub fn new(worker_count: usize) -> Self {
        Self {
            previous_arm: None,
            turn_index: 0,
            warmth: vec![None; worker_count],
        }
    }

    pub fn commit(&mut self, arm: usize, input_tokens: usize, now: Instant) {
        if self.warmth.len() <= arm {
            self.warmth.resize(arm + 1, None);
        }
        self.previous_arm = Some(arm);
        self.turn_index = self.turn_index.saturating_add(1);
        self.warmth[arm] = Some(WorkerWarmth {
            last_used: now,
            last_input_tokens: input_tokens,
        });
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub selected_arm: usize,
    pub selected_worker: String,
    pub raw_scores: Vec<f32>,
    pub adjusted_scores: Vec<f32>,
    pub switch_cost_usd: Vec<f64>,
    pub cache_miss_tokens: Vec<usize>,
    pub stayed: bool,
    pub cold_switch_upgrade_exemptions: Vec<bool>,
    pub stay_upgrade_exempted: bool,
}

pub fn select(
    manifest: &Manifest,
    raw_scores: Vec<f32>,
    state: &EpisodeState,
    input_tokens: usize,
    now: Instant,
) -> Result<Decision> {
    if raw_scores.len() != manifest.workers.len() || raw_scores.is_empty() {
        return Err(anyhow!("C82 score count does not match worker count"));
    }
    if raw_scores.iter().any(|value| !value.is_finite()) {
        return Err(anyhow!("C82 scores contain a non-finite value"));
    }
    let mut adjusted_scores = raw_scores.clone();
    let mut switch_cost_usd = vec![0.0; raw_scores.len()];
    let mut cache_miss_tokens = vec![0; raw_scores.len()];
    let mut cold_switch_upgrade_exemptions = vec![false; raw_scores.len()];
    if let Some(previous) = state.previous_arm {
        let previous_rate = manifest.workers[previous].estimated_input_cost_per_token;
        for (index, worker) in manifest.workers.iter().enumerate() {
            if index == previous {
                continue;
            }
            let exempt = manifest.policy.cold_switch_upgrade_exempt
                && worker.estimated_input_cost_per_token > previous_rate;
            cold_switch_upgrade_exemptions[index] = exempt;
            if exempt {
                continue;
            }
            let (miss_tokens, cost) = switch_cost(worker, state, index, input_tokens, now);
            cache_miss_tokens[index] = miss_tokens;
            switch_cost_usd[index] = cost;
            adjusted_scores[index] -=
                (manifest.policy.cold_switch_margin_per_usd as f64 * cost) as f32;
        }
    }

    let tentative =
        argmax_first(&adjusted_scores).ok_or_else(|| anyhow!("C82 policy has no candidate arm"))?;
    let mut selected = tentative;
    let mut stayed = false;
    let mut stay_upgrade_exempted = false;
    if let Some(previous) = state.previous_arm
        && tentative != previous
    {
        stay_upgrade_exempted = manifest.policy.stay_margin_upgrade_exempt
            && manifest.workers[tentative].estimated_input_cost_per_token
                > manifest.workers[previous].estimated_input_cost_per_token;
        if !stay_upgrade_exempted
            && adjusted_scores[tentative] - adjusted_scores[previous]
                <= manifest.policy.previous_worker_stay_margin
        {
            selected = previous;
            stayed = true;
        }
    }

    Ok(Decision {
        selected_arm: selected,
        selected_worker: manifest.workers[selected].id.clone(),
        raw_scores,
        adjusted_scores,
        switch_cost_usd,
        cache_miss_tokens,
        stayed,
        cold_switch_upgrade_exemptions,
        stay_upgrade_exempted,
    })
}

fn switch_cost(
    worker: &WorkerManifest,
    state: &EpisodeState,
    arm: usize,
    input_tokens: usize,
    now: Instant,
) -> (usize, f64) {
    let warmth = state.warmth.get(arm).and_then(Option::as_ref);
    let cacheable_prefix = warmth
        .map(|value| value.last_input_tokens.min(input_tokens))
        .unwrap_or(0);
    let uncached_suffix = input_tokens.saturating_sub(cacheable_prefix);
    let seconds = warmth.map(|value| {
        now.checked_duration_since(value.last_used)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64()
    });
    let miss_ratio = 1.0 - expected_cache_hit_ratio(seconds);
    let decayed = (cacheable_prefix as f64 * miss_ratio).round() as usize;
    let miss_tokens = uncached_suffix.saturating_add(decayed);
    let discount = (worker.estimated_input_cost_per_token
        - worker.estimated_cache_read_cost_per_token)
        .max(0.0);
    (miss_tokens, miss_tokens as f64 * discount)
}

fn expected_cache_hit_ratio(seconds: Option<f64>) -> f64 {
    let Some(seconds) = seconds else {
        return 0.0;
    };
    if seconds <= CACHE_RETURN_WARM_SECONDS {
        return CACHE_RETURN_WARM_HIT_RATIO;
    }
    if seconds >= CACHE_RETURN_COLD_SECONDS {
        return CACHE_RETURN_COLD_HIT_RATIO;
    }
    let fraction = (seconds - CACHE_RETURN_WARM_SECONDS)
        / (CACHE_RETURN_COLD_SECONDS - CACHE_RETURN_WARM_SECONDS);
    CACHE_RETURN_WARM_HIT_RATIO
        + fraction * (CACHE_RETURN_COLD_HIT_RATIO - CACHE_RETURN_WARM_HIT_RATIO)
}

pub fn argmax_first(values: &[f32]) -> Option<usize> {
    values
        .iter()
        .enumerate()
        .fold(None, |best, (index, value)| match best {
            Some((_, best_value)) if !value.total_cmp(best_value).is_gt() => best,
            _ => Some((index, value)),
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn worker(input: f64, cache_read: f64) -> WorkerManifest {
        WorkerManifest {
            id: "worker".to_owned(),
            model: "provider/model".to_owned(),
            api_key_env: "OPENROUTER_API_KEY".to_owned(),
            estimated_input_cost_per_token: input,
            estimated_cache_read_cost_per_token: cache_read,
            estimated_cache_write_cost_per_token: input,
            estimated_output_cost_per_token: 0.0,
            openrouter_provider_slug: "provider".to_owned(),
            openrouter_provider_name: "Provider".to_owned(),
            openrouter_provider_order: vec!["provider".to_owned()],
            openrouter_allow_fallbacks: false,
            openrouter_require_parameters: true,
            thinking_mode: "off".to_owned(),
            reasoning_budget_tokens: 0,
            minimum_completion_tokens: 0,
            max_completion_tokens: None,
            temperature: None,
            supports_output_effort: false,
            extra_body: json!({}),
            openrouter_max_retries: 3,
            openrouter_retry_base_seconds: 2.0,
            openrouter_retry_cap_seconds: 30.0,
            attempt_deadline_seconds: None,
        }
    }

    #[test]
    fn episode_state_starts_null_and_advances_only_on_commit() {
        let now = Instant::now();
        let mut state = EpisodeState::new(7);
        assert_eq!(state.previous_arm, None);
        assert_eq!(state.turn_index, 0);
        state.commit(4, 123, now);
        assert_eq!(state.previous_arm, Some(4));
        assert_eq!(state.turn_index, 1);
        assert_eq!(state.warmth[4].as_ref().unwrap().last_input_tokens, 123);
    }

    #[test]
    fn cache_hit_decay_and_switch_cost_match_contract() {
        assert_eq!(expected_cache_hit_ratio(None), 0.0);
        assert_eq!(expected_cache_hit_ratio(Some(0.0)), 0.8);
        assert_eq!(expected_cache_hit_ratio(Some(150.0)), 0.8);
        assert_eq!(expected_cache_hit_ratio(Some(225.0)), 0.5);
        assert_eq!(expected_cache_hit_ratio(Some(300.0)), 0.2);
        let now = Instant::now();
        let mut state = EpisodeState::new(1);
        state.warmth[0] = Some(WorkerWarmth {
            last_used: now,
            last_input_tokens: 1_000,
        });
        let (miss, cost) = switch_cost(&worker(0.000_01, 0.000_001), &state, 0, 1_200, now);
        // 200 uncached suffix + 20% miss on the 1,000-token warm prefix.
        assert_eq!(miss, 400);
        assert!((cost - 0.0036).abs() < 1e-12);
    }

    #[test]
    fn argmax_is_stable_first_and_rejects_empty() {
        assert_eq!(argmax_first(&[]), None);
        assert_eq!(argmax_first(&[1.0, 2.0, 2.0]), Some(1));
    }
}

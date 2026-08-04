use std::path::PathBuf;

use anyhow::{Context as _, Result};
use rayline_mtrouter::{C82Router, EpisodeState, HistoryTurn, NativeEncoderOptions};

fn configured_paths() -> Option<(PathBuf, PathBuf, PathBuf, String)> {
    Some((
        std::env::var_os("RAYLINE_C82_RUNTIME_DIR")?.into(),
        std::env::var_os("RAYLINE_C82_NATIVE_BINARY")?.into(),
        std::env::var_os("RAYLINE_C82_NATIVE_GGUF")?.into(),
        std::env::var("RAYLINE_C82_NATIVE_DEVICE").unwrap_or_else(|_| "auto".to_owned()),
    ))
}

fn turn(role: &str, text: impl Into<String>) -> HistoryTurn {
    HistoryTurn {
        role: role.to_owned(),
        text: text.into(),
    }
}

#[tokio::test]
async fn native_runtime_cache_contract() -> Result<()> {
    let Some((runtime, binary, gguf, device)) = configured_paths() else {
        return Ok(());
    };
    let mut options = NativeEncoderOptions::c82(binary, gguf, device);
    if std::env::var_os("RAYLINE_C82_NATIVE_LONG_SINGLE_SESSION").is_some() {
        options.max_sessions = 1;
    }
    if let Some(value) = std::env::var_os("RAYLINE_C82_NATIVE_PHYSICAL_BATCH") {
        options.physical_batch_tokens = value
            .to_string_lossy()
            .parse()
            .context("parse RAYLINE_C82_NATIVE_PHYSICAL_BATCH")?;
    }
    let router = C82Router::load_native(runtime, options).await?;
    let health = router.health().await?;
    anyhow::ensure!(health.backend == "libllama");
    if std::env::var_os("RAYLINE_C82_NATIVE_LONG_ONLY").is_some() {
        return run_truncation_contract(&router).await;
    }
    let parity = router.verify_encoder_golden().await?;
    anyhow::ensure!(
        parity.selection_parity == 1.0 && parity.incremental_clean_embedding_max_abs == Some(0.0)
    );

    let long_text = (0..1_200)
        .map(|index| format!("item-{index:05}"))
        .collect::<Vec<_>>()
        .join(" ");
    let prefill = vec![
        turn("user", "Analyze this deterministic synthetic sequence."),
        turn("user", long_text.clone()),
    ];
    let delta = vec![
        prefill[0].clone(),
        prefill[1].clone(),
        turn("assistant", "The sequence is deterministic."),
        turn("user", "State the final item identifier only."),
    ];
    let state = EpisodeState::new(7);
    anyhow::ensure!(
        router
            .route("cache-contract", &prefill, &state)
            .await?
            .telemetry
            .encode_mode
            == "prefill"
    );
    anyhow::ensure!(
        router
            .route("cache-contract", &delta, &state)
            .await?
            .telemetry
            .encode_mode
            == "delta"
    );
    anyhow::ensure!(
        router
            .route("cache-contract", &delta, &state)
            .await?
            .telemetry
            .encode_mode
            == "cached"
    );
    let rebuilt = vec![
        turn("user", "Use a mismatched deterministic prefix."),
        turn("user", long_text),
    ];
    anyhow::ensure!(
        router
            .route("cache-contract", &rebuilt, &state)
            .await?
            .telemetry
            .encode_mode
            == "rebuild"
    );

    for index in 0..5 {
        let episode = format!("eviction-{index}");
        let route = router.route(&episode, &prefill, &state).await?;
        anyhow::ensure!(route.telemetry.encode_mode == "prefill");
    }
    anyhow::ensure!(router.health().await?.kv_evictions > 0);

    if std::env::var_os("RAYLINE_C82_NATIVE_LONG_TEST").is_some() {
        run_truncation_contract(&router).await?;
    }
    Ok(())
}

async fn run_truncation_contract(router: &C82Router) -> Result<()> {
    let text = " token".repeat(270_000);
    let route = router
        .route(
            "truncation-contract",
            &[turn("user", text)],
            &EpisodeState::new(7),
        )
        .await
        .context("run 262k native truncation contract")?;
    anyhow::ensure!(route.telemetry.truncated_tokens > 0);
    anyhow::ensure!(route.telemetry.encode_mode == "full_truncation_fallback");
    Ok(())
}

#[tokio::test]
async fn native_runtime_fails_closed_on_budget_and_wrong_device() -> Result<()> {
    if std::env::var_os("RAYLINE_C82_NATIVE_FAILURE_TEST").is_none() {
        return Ok(());
    }
    let Some((runtime, binary, gguf, device)) = configured_paths() else {
        return Ok(());
    };
    let mut budgeted = NativeEncoderOptions::c82(binary.clone(), gguf.clone(), device.clone());
    budgeted.memory_budget_gib = Some(0.5);
    let router = C82Router::load_native(&runtime, budgeted).await?;
    anyhow::ensure!(router.health().await.is_err());
    drop(router);

    let unavailable = if device == "cuda" { "metal" } else { "cuda" };
    let router = C82Router::load_native(
        runtime,
        NativeEncoderOptions::c82(binary, gguf, unavailable.to_owned()),
    )
    .await?;
    anyhow::ensure!(router.health().await.is_err());
    Ok(())
}

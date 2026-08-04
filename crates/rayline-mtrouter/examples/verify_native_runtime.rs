use std::path::PathBuf;

use anyhow::{Context as _, Result};
use rayline_mtrouter::{C82Router, NativeEncoderOptions};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let runtime =
        PathBuf::from(args.next().context(
            "usage: verify_native_runtime RUNTIME_DIR BINARY GGUF [auto|metal|cuda|cpu]",
        )?);
    let binary = PathBuf::from(args.next().context("native binary is required")?);
    let model = PathBuf::from(args.next().context("native GGUF is required")?);
    let device = args
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auto".to_owned());
    if args.next().is_some() {
        anyhow::bail!("unexpected extra argument");
    }

    let router =
        C82Router::load_native(&runtime, NativeEncoderOptions::c82(binary, model, device)).await?;
    let output = serde_json::json!({
        "health": router.health().await?,
        "head": router.verify_head_golden()?,
        "encoder": router.verify_encoder_golden().await?,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

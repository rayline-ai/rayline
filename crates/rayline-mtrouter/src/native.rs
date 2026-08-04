use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Clone, Debug)]
pub struct NativeEncoderOptions {
    pub binary_path: PathBuf,
    pub model_path: PathBuf,
    pub device: String,
    pub memory_budget_gib: Option<f64>,
    pub max_tokens: usize,
    pub checkpoint_tokens: usize,
    pub physical_batch_tokens: usize,
    pub session_budget_tokens: usize,
    pub process_budget_tokens: usize,
    pub max_sessions: usize,
    pub idle_ttl_seconds: f64,
}

impl NativeEncoderOptions {
    pub fn c82(binary_path: PathBuf, model_path: PathBuf, device: String) -> Self {
        Self {
            binary_path,
            model_path,
            device,
            memory_budget_gib: None,
            max_tokens: 262_144,
            checkpoint_tokens: 8_192,
            physical_batch_tokens: 512,
            session_budget_tokens: 300_000,
            process_budget_tokens: 600_000,
            max_sessions: 2,
            idle_ttl_seconds: 900.0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct NativeEncoderClient {
    requests: mpsc::Sender<NativeRequest>,
}

struct NativeRequest {
    payload: Value,
    response: oneshot::Sender<Result<Value, String>>,
}

impl NativeEncoderClient {
    pub(crate) async fn spawn(options: NativeEncoderOptions) -> Result<Self> {
        if !options.binary_path.is_file() {
            return Err(anyhow!(
                "C82 native encoder binary is missing: {}",
                options.binary_path.display()
            ));
        }
        if !options.model_path.is_file() {
            return Err(anyhow!(
                "C82 native encoder GGUF is missing: {}",
                options.model_path.display()
            ));
        }
        if !matches!(options.device.as_str(), "auto" | "metal" | "cuda" | "cpu") {
            return Err(anyhow!("invalid C82 native device {}", options.device));
        }

        let mut command = Command::new(&options.binary_path);
        command
            .arg("--model")
            .arg(&options.model_path)
            .args(["--device", &options.device])
            .args(["--max-tokens", &options.max_tokens.to_string()])
            .args([
                "--checkpoint-tokens",
                &options.checkpoint_tokens.to_string(),
            ])
            .args([
                "--physical-batch-tokens",
                &options.physical_batch_tokens.to_string(),
            ])
            .args([
                "--session-budget-tokens",
                &options.session_budget_tokens.to_string(),
            ])
            .args([
                "--process-budget-tokens",
                &options.process_budget_tokens.to_string(),
            ])
            .args(["--max-sessions", &options.max_sessions.to_string()])
            .args(["--idle-ttl-seconds", &options.idle_ttl_seconds.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(memory_budget_gib) = options.memory_budget_gib {
            command.args(["--memory-budget-gib", &memory_budget_gib.to_string()]);
        }

        let mut child = command.spawn().with_context(|| {
            format!("start C82 native encoder {}", options.binary_path.display())
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("C82 native encoder stdin was not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("C82 native encoder stdout was not captured"))?;
        let mut stdout = BufReader::new(stdout);
        let (requests, mut receiver) = mpsc::channel::<NativeRequest>(32);

        tokio::spawn(async move {
            let mut line = String::new();
            while let Some(request) = receiver.recv().await {
                let result = async {
                    if let Some(status) = child
                        .try_wait()
                        .map_err(|error| format!("poll native encoder: {error}"))?
                    {
                        return Err(format!(
                            "C82 native encoder exited unexpectedly with {status}"
                        ));
                    }
                    let mut serialized = serde_json::to_vec(&request.payload)
                        .map_err(|error| format!("serialize native encoder request: {error}"))?;
                    serialized.push(b'\n');
                    stdin
                        .write_all(&serialized)
                        .await
                        .map_err(|error| format!("write native encoder request: {error}"))?;
                    stdin
                        .flush()
                        .await
                        .map_err(|error| format!("flush native encoder request: {error}"))?;
                    line.clear();
                    let count = stdout
                        .read_line(&mut line)
                        .await
                        .map_err(|error| format!("read native encoder response: {error}"))?;
                    if count == 0 {
                        return Err("C82 native encoder closed its response stream".to_owned());
                    }
                    let envelope: Value = serde_json::from_str(&line)
                        .map_err(|error| format!("parse native encoder response: {error}"))?;
                    if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
                        return Err(envelope
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("native encoder returned an unknown error")
                            .to_owned());
                    }
                    envelope
                        .get("result")
                        .cloned()
                        .ok_or_else(|| "native encoder response has no result".to_owned())
                }
                .await;
                let _ = request.response.send(result);
            }
            let shutdown = serde_json::json!({"op": "shutdown"});
            if let Ok(mut serialized) = serde_json::to_vec(&shutdown) {
                serialized.push(b'\n');
                let _ = stdin.write_all(&serialized).await;
                let _ = stdin.flush().await;
            }
            if tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        });

        Ok(Self { requests })
    }

    pub(crate) async fn call(&self, payload: Value) -> Result<Value> {
        let (response, receiver) = oneshot::channel();
        self.requests
            .send(NativeRequest { payload, response })
            .await
            .map_err(|_| anyhow!("C82 native encoder process is unavailable"))?;
        let result = tokio::time::timeout(REQUEST_TIMEOUT, receiver)
            .await
            .map_err(|_| anyhow!("C82 native encoder request timed out"))?
            .map_err(|_| anyhow!("C82 native encoder response channel closed"))?;
        result.map_err(anyhow::Error::msg)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should follow the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rayline-mtrouter-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn write_executable(path: &std::path::Path, body: &str) {
        fs::write(path, body).expect("write fake native helper");
        let mut permissions = fs::metadata(path)
            .expect("read fake native helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make fake native helper executable");
    }

    #[tokio::test]
    async fn helper_exit_fails_closed() {
        let directory = temp_dir("helper-exit");
        fs::create_dir_all(&directory).expect("create fake native runtime");
        let binary = directory.join("encoder");
        let model = directory.join("model.gguf");
        write_executable(&binary, "#!/bin/sh\nexit 17\n");
        fs::write(&model, b"fixture").expect("write fake model");

        let client =
            NativeEncoderClient::spawn(NativeEncoderOptions::c82(binary, model, "cpu".to_owned()))
                .await
                .expect("spawn fake native helper");
        let error = client
            .call(json!({"op":"health"}))
            .await
            .expect_err("dead helper must fail closed");
        assert!(error.to_string().contains("native encoder"));
        fs::remove_dir_all(directory).expect("remove fake native runtime");
    }

    #[tokio::test]
    async fn malformed_helper_response_fails_closed() {
        let directory = temp_dir("malformed-response");
        fs::create_dir_all(&directory).expect("create fake native runtime");
        let binary = directory.join("encoder");
        let model = directory.join("model.gguf");
        write_executable(
            &binary,
            "#!/bin/sh\nIFS= read -r request\nprintf 'not-json\\n'\n",
        );
        fs::write(&model, b"fixture").expect("write fake model");

        let client =
            NativeEncoderClient::spawn(NativeEncoderOptions::c82(binary, model, "cpu".to_owned()))
                .await
                .expect("spawn fake native helper");
        let error = client
            .call(json!({"op":"health"}))
            .await
            .expect_err("malformed helper response must fail closed");
        assert!(error.to_string().contains("parse native encoder response"));
        fs::remove_dir_all(directory).expect("remove fake native runtime");
    }
}

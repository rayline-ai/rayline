//! Hermetic end-to-end tests proving the documented provider-endpoint configs
//! work through the full HTTP path, using an in-process mock upstream instead of
//! a real provider. These run in CI with no API keys and loopback-only traffic,
//! and — because we control the upstream byte-for-byte — they assert the
//! streaming behavior deterministically (which the live provider tests cannot,
//! since real providers control their own chunk granularity).

use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use rayline_local_router::{LocalRouterOptions, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Accept one connection, drain the request, and reply with a fixed raw HTTP/1.1
/// response. `connection: close` lets the client read a body of unknown length
/// (SSE) until EOF. The caller passes a pre-bound listener so the port is known
/// before the router is configured.
async fn serve_once(listener: TcpListener, response: String) {
    if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 16384];
        // A single read drains the (small) request line, headers, and body for a
        // loopback request; we don't need the contents, only to free the socket.
        let _ = sock.read(&mut buf).await;
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

fn http_sse(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n{body}"
    )
}

fn write_config(name: &str, config: &Value) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "rayline-mock-{}-{}-{}.json",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
    path
}

async fn start_router(port: u16, config_path: PathBuf) {
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config_path),
        ..LocalRouterOptions::default()
    };
    tokio::spawn(async move {
        let _ = serve(opts).await;
    });
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("router on port {port} did not become healthy");
}

async fn collect_sse(resp: reqwest::Response) -> Vec<Value> {
    let mut buffer = String::new();
    let mut events = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
        while let Some(idx) = buffer.find('\n') {
            let mut line = buffer[..idx].to_owned();
            buffer.drain(..idx + 1);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(p) = line.strip_prefix("data:") {
                let p = p.trim();
                if !p.is_empty() && p != "[DONE]" {
                    if let Ok(v) = serde_json::from_str::<Value>(p) {
                        events.push(v);
                    }
                }
            }
        }
    }
    events
}

/// Documented config: an arbitrary local OpenAI-compatible endpoint (e.g. a
/// llama.cpp server) with NO `api_key_env`. Proves the openai_chat → Anthropic
/// SSE translation end-to-end over real HTTP, against an arbitrary base_url,
/// with the no-auth path, deterministically (exactly one text_delta per
/// upstream content fragment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_streaming_translation_through_mock_no_auth() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    tokio::spawn(serve_once(upstream, http_sse(sse)));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "local-openai",
            "protocol": "openai_chat",
            "base_url": format!("http://127.0.0.1:{up_port}/v1"),
            "models": ["local-model-x"]
            // No api_key_env: the documented "omit it if the server needs no auth" path.
        }],
        "routes": {
            "main": {"endpoint": "local-openai", "model": "local-model-x"},
            "default": {"endpoint": "local-openai", "model": "local-model-x"}
        }
    });
    let path = write_config("openai-stream", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({
            "model": "rayline-router",
            "stream": true,
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(ct.starts_with("text/event-stream"), "content-type={ct:?}");

    let events = collect_sse(resp).await;
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    let text_deltas = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .count();
    let text: String = events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    assert_eq!(text_deltas, 2, "one text_delta per upstream fragment");
    assert_eq!(text, "Hello world");
    assert_eq!(types.first(), Some(&"message_start"));
    assert_eq!(types.last(), Some(&"message_stop"));
    let message_delta = events
        .iter()
        .find(|e| e["type"] == "message_delta")
        .expect("message_delta");
    assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(message_delta["usage"]["output_tokens"], 2);

    let _ = std::fs::remove_file(path);
    println!("PASS mock openai_chat streaming (no auth): deltas={text_deltas} text={text:?}");
}

/// Documented config: an `anthropic_messages` endpoint. Proves the router
/// forwards native Anthropic SSE through VERBATIM — preserving every upstream
/// content_block_delta (no coalescing). This is the deterministic counterpart to
/// the live OpenRouter test, whose delta count is provider-controlled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_messages_passthrough_preserves_deltas_through_mock() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    tokio::spawn(serve_once(upstream, http_sse(sse)));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "anthropic-compatible",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["claude-mock"]
        }],
        "routes": {
            "main": {"endpoint": "anthropic-compatible", "model": "claude-mock"},
            "default": {"endpoint": "anthropic-compatible", "model": "claude-mock"}
        }
    });
    let path = write_config("anthropic-passthrough", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-router",
            "stream": true,
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);

    let events = collect_sse(resp).await;
    let text_deltas = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .count();
    let text: String = events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    // Passthrough must preserve BOTH upstream deltas (not coalesce them).
    assert_eq!(
        text_deltas, 2,
        "passthrough must preserve every upstream delta"
    );
    assert_eq!(text, "Hi there");
    assert!(events.iter().any(|e| e["type"] == "message_stop"));

    let _ = std::fs::remove_file(path);
    println!("PASS mock anthropic_messages passthrough: deltas={text_deltas} text={text:?}");
}

/// A minimal Anthropic SSE stream whose single text delta is `text`, so a test can
/// tell which upstream answered by reading the response text.
fn anthropic_sse(text: &str) -> String {
    let body = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"x\",\"content\":[],\"stop_reason\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n\
event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n\
event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    http_sse(&body)
}

fn sse_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect()
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// End-to-end (headless / agent path): a single `--config` with distinct `main`
/// and `subagent` routes must send the MAIN request to one upstream and a SUBAGENT
/// request to ANOTHER, over real HTTP. This is the SDK-through-the-router proof for
/// `rayline router start --config` — main vs subagent is classified per request by
/// the agent headers, exactly as the proxy sets them at runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_routes_main_and_subagent_to_distinct_endpoints() {
    let main_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let main_port = main_up.local_addr().unwrap().port();
    let sub_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sub_port = sub_up.local_addr().unwrap().port();
    tokio::spawn(serve_once(main_up, anthropic_sse("MAIN-OK")));
    tokio::spawn(serve_once(sub_up, anthropic_sse("SUB-OK")));

    let port = free_port();
    let config = json!({
        "endpoints": [
            {"id": "main-ep", "protocol": "anthropic_messages",
             "base_url": format!("http://127.0.0.1:{main_port}"), "models": ["model-main"]},
            {"id": "sub-ep", "protocol": "anthropic_messages",
             "base_url": format!("http://127.0.0.1:{sub_port}"), "models": ["model-sub"]}
        ],
        "routes": {
            "main": {"endpoint": "main-ep", "model": "model-main"},
            "subagent": {"endpoint": "sub-ep", "model": "model-sub"}
        }
    });
    let path = write_config("config-both", &config);
    start_router(port, path.clone()).await;
    let client = reqwest::Client::new();

    // Main turn: no agent headers → routes.main → main-ep.
    let main_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-router", "stream": true, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("main request");
    assert_eq!(main_resp.status(), 200);
    assert_eq!(header(&main_resp, "x-rayline-task-class"), Some("main"));
    assert_eq!(
        header(&main_resp, "x-rayline-selected-model"),
        Some("model-main")
    );
    assert_eq!(sse_text(&collect_sse(main_resp).await), "MAIN-OK");

    // Subagent turn: agent-id + agent-type headers → routes.subagent → sub-ep.
    let sub_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-agent-id", "abc123")
        .header("x-rayline-claude-code-agent-type", "reviewer")
        .json(&json!({
            "model": "rayline-router", "stream": true, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("subagent request");
    assert_eq!(sub_resp.status(), 200);
    assert_eq!(header(&sub_resp, "x-rayline-task-class"), Some("subagent"));
    assert_eq!(
        header(&sub_resp, "x-rayline-selected-model"),
        Some("model-sub")
    );
    assert_eq!(sse_text(&collect_sse(sub_resp).await), "SUB-OK");

    let _ = std::fs::remove_file(path);
    println!("PASS config drives main→main-ep, subagent→sub-ep over HTTP");
}

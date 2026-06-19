//! Integration test: boot rayline-injector pointed at a fake cloud router and
//! verify the three x-rayline-local-* headers are injected, the inbound
//! Authorization is forwarded, and a 307 from the cloud passes through
//! unchanged (no auto-follow).

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;

fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[derive(Default, Clone)]
struct Captured {
    headers: Arc<Mutex<Vec<(String, String)>>>,
}

async fn fake_router(port: u16, captured: Captured) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let captured = captured.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let captured = captured.clone();
                async move {
                    let mut h = captured.headers.lock().unwrap();
                    h.clear();
                    for (k, v) in req.headers().iter() {
                        h.push((k.as_str().to_string(), v.to_str().unwrap_or("").to_string()));
                    }
                    // Respond with 307 so we can assert the injector
                    // forwards it through unchanged.
                    let resp: Response<Full<Bytes>> = Response::builder()
                        .status(StatusCode::TEMPORARY_REDIRECT)
                        .header(
                            "location",
                            "http://127.0.0.1:20808/api/v1/messages?usage_doc_id=fake",
                        )
                        .body(Full::new(Bytes::from_static(b"")))
                        .unwrap();
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

#[tokio::test]
async fn injector_adds_headers_and_passes_307() {
    let router_port = free_port();
    let injector_port = free_port();
    let captured = Captured::default();
    tokio::spawn(fake_router(router_port, captured.clone()));
    tokio::spawn(rayline_injector::serve(rayline_injector::InjectorOptions {
        port: injector_port,
        router_url: format!("http://127.0.0.1:{}", router_port),
        local_model_id: "qwen3.6-35b-a3b-q4-k-m".into(),
        auth_cache: None,
        local_available: None,
        custom_mode: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Client must NOT auto-follow so we can observe the 307.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/messages", injector_port))
        .header("authorization", "Bearer rayline-test-router-key")
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 307);
    assert!(
        resp.headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("http://127.0.0.1:20808/")
    );

    let seen = captured.headers.lock().unwrap().clone();
    let find = |name: &str| -> Option<String> {
        seen.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(find("x-rayline-local-available"), Some("true".into()));
    assert_eq!(find("x-rayline-local-hint"), Some("1".into()));
    assert_eq!(
        find("x-rayline-local-model-id"),
        Some("qwen3.6-35b-a3b-q4-k-m".into())
    );
    // Default (bundled) mode must NOT advertise the custom-endpoint flag.
    assert!(find("x-rayline-local-custom").is_none());
    assert_eq!(
        find("authorization"),
        Some("Bearer rayline-test-router-key".into())
    );
    // Hop-by-hop must NOT have been forwarded.
    assert!(find("transfer-encoding").is_none());
}

#[tokio::test]
async fn injector_custom_mode_emits_custom_header_and_omits_hint() {
    let router_port = free_port();
    let injector_port = free_port();
    let captured = Captured::default();
    tokio::spawn(fake_router(router_port, captured.clone()));
    tokio::spawn(rayline_injector::serve(rayline_injector::InjectorOptions {
        port: injector_port,
        router_url: format!("http://127.0.0.1:{}", router_port),
        local_model_id: "google/gemma-4-e4b".into(),
        auth_cache: None,
        local_available: None,
        custom_mode: true,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let _ = client
        .post(format!("http://127.0.0.1:{}/v1/messages", injector_port))
        .header("authorization", "Bearer rayline-test-router-key")
        .body("{}")
        .send()
        .await
        .unwrap();

    let seen = captured.headers.lock().unwrap().clone();
    let find = |name: &str| -> Option<String> {
        seen.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    // Custom mode: advertise available + the real model id + the custom flag,
    // and DO NOT force the hint (router delegates only exploration subagents).
    assert_eq!(find("x-rayline-local-available"), Some("true".into()));
    assert_eq!(find("x-rayline-local-custom"), Some("true".into()));
    assert_eq!(
        find("x-rayline-local-model-id"),
        Some("google/gemma-4-e4b".into())
    );
    assert!(find("x-rayline-local-hint").is_none());
}

#[tokio::test]
async fn injector_advertises_local_unavailable_when_unhealthy() {
    let router_port = free_port();
    let injector_port = free_port();
    let captured = Captured::default();
    tokio::spawn(fake_router(router_port, captured.clone()));
    // Watchdog has marked the local model unhealthy.
    tokio::spawn(rayline_injector::serve(rayline_injector::InjectorOptions {
        port: injector_port,
        router_url: format!("http://127.0.0.1:{}", router_port),
        local_model_id: "qwen3.6-35b-a3b-q4-k-m".into(),
        auth_cache: None,
        local_available: Some(Arc::new(AtomicBool::new(false))),
        custom_mode: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let _ = client
        .post(format!("http://127.0.0.1:{}/v1/messages", injector_port))
        .header("authorization", "Bearer rayline-test-router-key")
        // Stale client-supplied local headers must NOT survive: the injector is
        // authoritative and must override them to reflect the unhealthy model.
        .header("x-rayline-local-available", "true")
        .header("x-rayline-local-hint", "1")
        .header("x-rayline-local-model-id", "stale-client-rayline-model")
        .body("{}")
        .send()
        .await
        .unwrap();

    let seen = captured.headers.lock().unwrap().clone();
    let find = |name: &str| -> Option<String> {
        seen.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let count = |name: &str| -> usize {
        seen.iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .count()
    };
    // Local advertised unavailable → router serves from cloud, not 307. The
    // client's stale `true`/hint/model-id were dropped, leaving only our single
    // `false`.
    assert_eq!(find("x-rayline-local-available"), Some("false".into()));
    assert_eq!(count("x-rayline-local-available"), 1);
    assert!(find("x-rayline-local-hint").is_none());
    assert!(find("x-rayline-local-model-id").is_none());
}

#[tokio::test]
async fn injector_healthz() {
    let port = free_port();
    tokio::spawn(rayline_injector::serve(rayline_injector::InjectorOptions {
        port,
        router_url: "http://127.0.0.1:1".into(),
        local_model_id: "qwen3.6-35b-a3b-q4-k-m".into(),
        auth_cache: None,
        local_available: None,
        custom_mode: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = reqwest::get(format!("http://127.0.0.1:{}/healthz", port))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
}

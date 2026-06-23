//! Thin reverse proxy that injects `x-rayline-local-*` headers and forwards
//! to the Rayline router. Sits in front of Claude Code so Claude
//! Code itself can stay unmodified.
//!
//! Forwards the cloud router's 307 unchanged so the *client* follows it to
//! 127.0.0.1:20808 (the adapter). The injector never auto-follows.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use rayline_authcache::{
    AuthCache, MAX_AUTH_CACHE_ENTRIES, evict_auth_cache_overflow, new_auth_cache,
    stash_auth_headers,
};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub const DEFAULT_PORT: u16 = 20809;

#[derive(Clone)]
pub struct InjectorOptions {
    pub port: u16,
    /// Router base URL.
    pub router_url: String,
    /// Model id to advertise via `x-rayline-local-model-id`.
    pub local_model_id: String,
    /// Optional shared cache so the adapter can recover auth for usage callbacks.
    pub auth_cache: Option<AuthCache>,
    /// Shared health flag for the local model, flipped by the daemon
    /// watchdog. When present and `false`, advertise
    /// `x-rayline-local-available: false` so the router serves the turn
    /// instead of 307ing to a dead local adapter. `None` = always available.
    pub local_available: Option<Arc<AtomicBool>>,
    /// Custom user endpoint: advertise `x-rayline-local-custom` and suppress
    /// the forced hint so the router only delegates exploration subagents.
    pub custom_mode: bool,
}

#[derive(Clone)]
struct AppState {
    opts: Arc<InjectorOptions>,
    http: reqwest::Client,
}

pub async fn serve(opts: InjectorOptions) -> Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), opts.port);
    let listener = TcpListener::bind(addr).await?;
    info!(
        "injector listening on 127.0.0.1:{} → {} (local-model-id={})",
        opts.port, opts.router_url, opts.local_model_id
    );
    let state = AppState {
        opts: Arc::new(opts),
        // No redirect-following: 307 from cloud router must reach the client untouched.
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
    };
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(state, req).await) }
            });
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                warn!("connection error: {e}");
            }
        });
    }
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

async fn handle(state: AppState, req: Request<Incoming>) -> Response<BoxBody> {
    if req.method() == Method::GET && req.uri().path() == "/healthz" {
        let body = serde_json::to_vec(&json!({
            "ok": true,
            "injector_port": state.opts.port,
            "router_url": state.opts.router_url,
            "local_model_id": state.opts.local_model_id,
        }))
        .unwrap_or_else(|_| b"{}".to_vec());
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(full_body(body))
            .unwrap();
    }
    match forward(state, req).await {
        Ok(r) => r,
        Err(e) => {
            warn!("forward error: {e}");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(full_body(format!("injector forward error: {e}")))
                .unwrap()
        }
    }
}

async fn forward(state: AppState, req: Request<Incoming>) -> Result<Response<BoxBody>> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!(
        "{}{}",
        state.opts.router_url.trim_end_matches('/'),
        path_and_query
    );

    let bytes = body.collect().await?.to_bytes();

    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;
    let mut outbound = state.http.request(method, &url).body(bytes.to_vec());

    // Capture user's auth headers so we can stash them for the adapter's
    // usage callback if the cloud router redirects to a local origin.
    let mut inbound_auth: HashMap<String, String> = HashMap::new();
    if let Some(v) = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        inbound_auth.insert("Authorization".to_string(), v.to_string());
    }
    if let Some(v) = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        inbound_auth.insert("x-api-key".to_string(), v.to_string());
    }

    // Forward inbound headers (drop hop-by-hop, host, and any client-supplied
    // local-routing headers — the injector is the sole authority on those, set
    // authoritatively below from the watchdog's health flag, so a stale client
    // value can't force local routing to a down model).
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name)
            || name == hyper::header::HOST
            || name
                .as_str()
                .to_ascii_lowercase()
                .starts_with("x-rayline-local-")
        {
            continue;
        }
        outbound = outbound.header(name.as_str(), value.as_bytes());
    }

    // Inject Rayline local-routing headers, gated on the local model's health.
    // When the watchdog marks it unhealthy, advertise it unavailable so the
    // cloud router serves this turn rather than 307ing to a dead adapter (which
    // would make Claude Code retry-loop). `None` flag = always available.
    let local_up = state
        .opts
        .local_available
        .as_ref()
        .is_none_or(|h| h.load(Ordering::Relaxed));
    if local_up {
        outbound = outbound
            .header("x-rayline-local-available", "true")
            .header("x-rayline-local-model-id", &state.opts.local_model_id);
        if state.opts.custom_mode {
            // Custom user endpoint: trust the opt-in (router bypasses the
            // model-id allowlist) but do NOT force the hint, so the router
            // only delegates exploration subagents (Path A).
            outbound = outbound.header("x-rayline-local-custom", "true");
        } else {
            outbound = outbound.header("x-rayline-local-hint", "1");
        }
    } else {
        outbound = outbound.header("x-rayline-local-available", "false");
    }

    let resp = outbound.send().await?;
    let status = resp.status();

    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let routed =
        if status == reqwest::StatusCode::TEMPORARY_REDIRECT && location.contains("127.0.0.1") {
            "local"
        } else if status.is_redirection() {
            "redirect"
        } else {
            "cloud"
        };
    info!(
        "{} {} → {} status={} routed={}",
        parts.method.as_str(),
        path_and_query,
        state.opts.router_url,
        status.as_u16(),
        routed,
    );

    // If the cloud router 307s to a local origin, the client (Claude Code) will
    // strip Authorization on the cross-origin hop. Stash the auth by usage_doc_id
    // so the adapter can re-attach it when firing /v1/usage/update.
    if status == reqwest::StatusCode::TEMPORARY_REDIRECT && !inbound_auth.is_empty() {
        if let (Some(cache), Some(loc)) = (
            state.opts.auth_cache.as_ref(),
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
        ) {
            if let Some(doc_id) = extract_usage_doc_id(loc) {
                stash_auth_headers(cache, doc_id, inbound_auth);
            }
        }
    }

    let mut headers_out = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        if is_hop_by_hop_str(k.as_str()) {
            continue;
        }
        let name = match HeaderName::from_bytes(k.as_str().as_bytes()) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let value = match HeaderValue::from_bytes(v.as_bytes()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        headers_out.append(name, value);
    }

    // Stream the response body back so SSE / large payloads don't get buffered.
    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();

    tokio::spawn(async move {
        use futures::StreamExt;
        let mut s = resp.bytes_stream();
        while let Some(chunk) = s.next().await {
            match chunk {
                Ok(b) => {
                    if tx.send(Ok(Frame::data(b))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                    break;
                }
            }
        }
    });

    let mut builder = Response::builder().status(status.as_u16());
    if let Some(b) = builder.headers_mut() {
        *b = headers_out;
    }
    Ok(builder.body(body_out).unwrap())
}

/// Pull `usage_doc_id` out of a Location URL's query string.
fn extract_usage_doc_id(location: &str) -> Option<String> {
    let q = location.split_once('?')?.1;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        if k == "usage_doc_id" && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    is_hop_by_hop_str(name.as_str())
}

fn is_hop_by_hop_str(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_usage_doc_id_basic() {
        assert_eq!(
            extract_usage_doc_id("http://127.0.0.1:20808/api/v1/messages?usage_doc_id=rt_abc"),
            Some("rt_abc".to_string())
        );
        assert_eq!(
            extract_usage_doc_id(
                "http://127.0.0.1:20808/api/v1/messages?x=1&usage_doc_id=rt_ml_def&y=2"
            ),
            Some("rt_ml_def".to_string())
        );
        assert_eq!(
            extract_usage_doc_id("http://127.0.0.1:20808/api/v1/messages"),
            None
        );
    }

    #[test]
    fn hop_by_hop_classification() {
        assert!(is_hop_by_hop_str("Connection"));
        assert!(is_hop_by_hop_str("transfer-encoding"));
        assert!(!is_hop_by_hop_str("authorization"));
        assert!(!is_hop_by_hop_str("x-rayline-local-hint"));
    }
}

//! Phase 1 routing: prefix-based reverse proxy.
//!
//! `/crm/*path` → configured CRM backend, `/hrm/*path` → HRM backend.
//! Routing lives in core (ADR-0002) so integration tests can drive the
//! engine directly against in-process mock backends.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::circuit::{Admit, Breakers, State as BreakerState};
use crate::config::Route;
use crate::ratelimit::{limit_key, Decision, RateLimiter};

/// Shared proxy state.
#[derive(Debug, Clone)]
pub struct ProxyState {
    /// prefix → backend base URL (no trailing slash).
    backends: BTreeMap<String, String>,
    client: reqwest::Client,
    /// Optional Phase 2 auth gate: when set, the proxied fallback verifies
    /// a bearer JWT before forwarding. `/healthz` is never gated.
    auth: Option<crate::middleware_auth::AuthState>,
    /// Optional Phase 3 rate limiter: fixed-window Redis counters,
    /// fail-open. Applied after auth so verified subjects key the limit.
    limiter: Option<RateLimiter>,
    /// Per-backend circuit breakers (Phase 4). Transport failures open a
    /// backend's breaker; open breakers fail fast with 503.
    breakers: Option<Arc<Breakers>>,
    /// Request timeout for backend calls, from `[breaker].timeout_secs`.
    backend_timeout: std::time::Duration,
    /// Prometheus registry (Phase 5). Counts/latencies/rejections per
    /// route prefix; labels come from the validated table only.
    metrics: Option<crate::metrics::Metrics>,
}

impl ProxyState {
    pub fn new(routes: &[Route]) -> Result<Self, reqwest::Error> {
        let mut backends = BTreeMap::new();
        for route in routes {
            backends.insert(route.prefix.clone(), route.backend.clone());
        }
        // Phase 4: the backend timeout is a breaker input — a timed-out
        // request counts as a transport failure and can open the breaker.
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            backends,
            client,
            auth: None,
            limiter: None,
            breakers: None,
            backend_timeout: std::time::Duration::from_secs(30),
            metrics: None,
        })
    }

    /// Enable the fail-closed bearer gate on proxied routes (Phase 2).
    pub fn with_auth(mut self, auth: crate::middleware_auth::AuthState) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Enable the fail-open rate limiter on proxied routes (Phase 3).
    pub fn with_limiter(mut self, limiter: RateLimiter) -> Self {
        self.limiter = Some(limiter);
        self
    }

    /// Enable per-backend circuit breakers (Phase 4) and set the backend
    /// request timeout. `timeout_secs` comes from the same `[breaker]`
    /// config: a timed-out call is a transport failure for the breaker.
    pub fn with_breakers(mut self, breakers: Breakers, timeout_secs: u64) -> Self {
        self.breakers = Some(Arc::new(breakers));
        self.backend_timeout = std::time::Duration::from_secs(timeout_secs);
        self
    }

    /// Enable the Prometheus registry (Phase 5).
    pub fn with_metrics(mut self, metrics: crate::metrics::Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn backend_for(&self, prefix: &str) -> Option<&str> {
        self.backends.get(prefix).map(String::as_str)
    }
}

/// Build the Phase 1 application: health + one catch-all proxy fallback.
///
/// A single fallback parses `/{prefix}/{rest}` against the route table. This
/// avoids axum Path-extraction edge cases on wildcard routes and supports any
/// configured prefix without re-registering routes.
pub fn build_router(state: ProxyState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .fallback(proxy_fallback)
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Prometheus scrape endpoint, in the text exposition format.
/// Unauthenticated by design (see the doc on `/healthz`): orchestrators and
/// scrapers must not need a token. Bind the port to an internal interface
/// or wall it off at the ingress if that's wrong for your network.
async fn metrics_endpoint(State(state): State<ProxyState>) -> Response {
    // Breaker gauges are published at scrape time from breaker state —
    // the breaker stays the single source of truth.
    if let (Some(metrics), Some(breakers)) = (&state.metrics, &state.breakers) {
        for (prefix, breaker_state) in breakers.states() {
            metrics.set_breaker_state(&prefix, breaker_state);
        }
    }
    let body = state.metrics.map(|m| m.render()).unwrap_or_default();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// Readiness: real component health, not "process is alive". A gateway is
/// ready when every backend answers (open breakers count as not-ready —
/// they are open because the backend proved unreachable) and Redis answers
/// PING (enforcement is fail-open, but readiness must not lie about it).
/// 200 with per-component details when ready, 503 with the same body when
/// not — orchestrators key on status, humans read the body.
async fn readyz(State(state): State<ProxyState>) -> Response {
    let mut components = BTreeMap::new();
    let mut ready = true;

    if let Some(breakers) = &state.breakers {
        for (prefix, breaker_state) in breakers.states() {
            let ok = breaker_state != BreakerState::Open;
            ready &= ok;
            components.insert(format!("backend:{prefix}"), ok);
        }
    }
    if let Some(limiter) = &state.limiter {
        let ok = limiter.ping().await;
        ready &= ok;
        components.insert("redis".to_string(), ok);
    }

    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(components)).into_response()
}

/// Catch-all: parse `/{prefix}[/{rest}]` and proxy to the matching backend.
/// Middleware order on this path: auth (fail-closed) → rate limit
/// (fail-open) → circuit breaker → forward. Unauthenticated requests exit
/// at 401 without consuming rate-limit budget; over-limit requests exit at
/// 429 without touching the backend; an open breaker exits at 503 without
/// waiting on a dead backend.
async fn proxy_fallback(State(state): State<ProxyState>, request: Request) -> Response {
    let subject = if let Some(auth) = &state.auth {
        match crate::middleware_auth::check_bearer(auth, &request) {
            Ok(subject) => {
                // ADR-0001's audit shape: the per-request log line carries
                // the verified subject, not just the route and status.
                crate::middleware::record_subject(&subject);
                Some(subject)
            }
            Err(response) => return *response,
        }
    } else {
        None
    };
    if let Some(limiter) = &state.limiter {
        let key = limit_key(subject.as_deref(), "unkeyed");
        if let Decision::Limit(retry_after) = limiter.decide(&key).await {
            if let Some(metrics) = &state.metrics {
                // The prefix isn't parsed yet at this point; derive it from
                // the path the same way the fallback does, so the rejection
                // lands on the route's own counters.
                let prefix = request
                    .uri()
                    .path()
                    .trim_start_matches('/')
                    .split_once('/')
                    .map(|(p, _)| p)
                    .unwrap_or("");
                metrics.inc_rate_limited(prefix);
            }
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", retry_after.to_string())],
                "rate limit exceeded",
            )
                .into_response();
        }
        // Decision::Allow and Decision::Open both proceed.
    }
    let full_path = request.uri().path().to_string();
    let trimmed = full_path.trim_start_matches('/');
    let (prefix, rest) = match trimmed.split_once('/') {
        Some((p, r)) => (p, format!("/{r}")),
        None => (trimmed, "/".to_string()),
    };
    if prefix.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    forward(&state, prefix, &rest, request).await
}

async fn forward(state: &ProxyState, prefix: &str, suffix: &str, request: Request) -> Response {
    let Some(backend) = state.backend_for(prefix) else {
        return (
            StatusCode::NOT_FOUND,
            format!("no route for prefix {prefix:?}"),
        )
            .into_response();
    };

    // Circuit breaker, per backend: ShortCircuit fails fast with 503 so a
    // dead backend costs callers a millisecond, not a connection timeout.
    let breaker = state.breakers.as_ref().and_then(|b| b.get(prefix));
    if let Some(breaker) = breaker {
        if breaker.try_admit() == Admit::ShortCircuit {
            tracing::warn!(prefix = %prefix, "circuit open: failing fast");
            if let Some(metrics) = &state.metrics {
                metrics.inc_circuit_open(prefix);
            }
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "5".to_string())],
                format!("backend {prefix:?} unavailable (circuit open)"),
            )
                .into_response();
        }
    }

    let (parts, body) = request.into_parts();
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!("{backend}{suffix}{query}");

    // Preserve the request method and headers; reqwest re-sets Host to the
    // backend authority, which is what a reverse proxy is expected to do.
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut out = state.client.request(method, &target);
    for (name, value) in &parts.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out = out.header(name.as_str(), v);
        }
    }
    out = out.timeout(state.backend_timeout);

    let body_bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(prefix = %prefix, %err, "failed to buffer request body");
            return (StatusCode::BAD_REQUEST, "failed to read request body").into_response();
        }
    };

    // Metrics: one timer covers the whole proxied attempt — transport
    // failures, breaker short-circuits and backend responses all land in
    // the same per-prefix latency family they belong to.
    let timer = state.metrics.as_ref().map(|m| m.start_request(prefix));
    let started = std::time::Instant::now();
    let response = match out.body(body_bytes).send().await {
        Ok(response) => response,
        // Transport failure: connect refused, DNS, timeout — the "backend is
        // broken or unreachable" family. This is what trips the breaker.
        Err(err) => {
            tracing::warn!(prefix = %prefix, target = %target, %err, "backend request failed");
            if let Some(breaker) = breaker {
                breaker.record(false);
            }
            if let Some(timer) = timer {
                timer.finish(Some("5xx"));
            }
            return (
                StatusCode::BAD_GATEWAY,
                format!("backend {prefix:?} unreachable"),
            )
                .into_response();
        }
    };
    // Backend answered over the wire: any HTTP status — including 500 —
    // means the process is up and serving; only transport failures count
    // toward opening the breaker.
    if let Some(breaker) = breaker {
        breaker.record(true);
    }
    let latency_ms = started.elapsed().as_millis() as u64;

    let status = response.status();
    let axum_status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(axum_status);
    for (name, value) in response.headers() {
        if is_hop_by_hop_name(name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    let payload = response.bytes().await.unwrap_or_default();
    if let Some(timer) = timer {
        timer.finish(Some(status_class(axum_status)));
    }
    tracing::info!(
        prefix = %prefix,
        target = %target,
        status = %axum_status,
        latency_ms,
        "proxied request"
    );
    builder.body(Body::from(payload)).unwrap_or_else(|err| {
        tracing::error!(%err, "failed to build upstream response");
        (StatusCode::BAD_GATEWAY, "response build failed").into_response()
    })
}

/// Status *class* for metric labels: `2xx`..`5xx`, not the code — keeps
/// label cardinality fixed regardless of how many codes backends invent.
fn status_class(status: StatusCode) -> String {
    format!("{}xx", status.as_u16() / 100)
}

/// RFC 7230 hop-by-hop headers must not be forwarded by a proxy.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    is_hop_by_hop_name(name.as_str())
}

fn is_hop_by_hop_name(name: &str) -> bool {
    matches!(
        name,
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
    use crate::middleware::request_id_and_trace;
    use axum::http::Request as AxumRequest;
    use axum::middleware::from_fn;
    use serde_json::Value as Json;
    use tower::ServiceExt;

    /// In-process mock backend standing in for CRM/HRM (no fixed ports).
    async fn spawn_mock_backend() -> String {
        let app = Router::new().fallback(|request: Request| async move {
            let path = request.uri().path().to_string();
            let query = request.uri().query().unwrap_or("").to_string();
            let method = request.method().to_string();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "method": method,
                    "path": path,
                    "query": query,
                })),
            )
                .into_response()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let routes = vec![Route {
            prefix: "crm".into(),
            backend: "http://127.0.0.1:1".into(), // never contacted here
        }];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state);
        let response = app
            .oneshot(AxumRequest::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn routes_round_trip_to_backend_with_path_and_query() {
        let backend_url = spawn_mock_backend().await;
        let routes = vec![
            Route {
                prefix: "crm".into(),
                backend: backend_url.clone(),
            },
            Route {
                prefix: "hrm".into(),
                backend: backend_url.clone(),
            },
        ];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state).layer(from_fn(request_id_and_trace));

        // CRM with path + query
        let response = app
            .clone()
            .oneshot(
                AxumRequest::get("/crm/customers/42?verbose=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-request-id"));
        let body: Json = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["path"], "/customers/42");
        assert_eq!(body["query"], "verbose=1");

        // HRM root
        let response = app
            .oneshot(AxumRequest::get("/hrm").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Json = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["path"], "/");
    }

    #[tokio::test]
    async fn unknown_prefix_is_404() {
        let routes = vec![Route {
            prefix: "crm".into(),
            backend: "http://127.0.0.1:1".into(),
        }];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state);
        let response = app
            .oneshot(AxumRequest::get("/nope/thing").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unreachable_backend_is_502() {
        // Port 1 is reserved and refuses connections.
        let routes = vec![Route {
            prefix: "crm".into(),
            backend: "http://127.0.0.1:1".into(),
        }];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state);
        let response = app
            .oneshot(
                AxumRequest::get("/crm/anything")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn post_method_is_forwarded() {
        let backend_url = spawn_mock_backend().await;
        let routes = vec![Route {
            prefix: "crm".into(),
            backend: backend_url,
        }];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state);
        let response = app
            .oneshot(
                AxumRequest::post("/crm/customers")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"ada"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Json = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["method"], "POST");
        assert_eq!(body["path"], "/customers");
    }
}

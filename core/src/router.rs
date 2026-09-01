//! Phase 1 routing: prefix-based reverse proxy.
//!
//! `/crm/*path` → configured CRM backend, `/hrm/*path` → HRM backend.
//! Routing lives in core (ADR-0002) so integration tests can drive the
//! engine directly against in-process mock backends.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::config::Route;

/// Shared proxy state.
#[derive(Debug, Clone)]
pub struct ProxyState {
    /// prefix → backend base URL (no trailing slash).
    backends: BTreeMap<String, String>,
    client: reqwest::Client,
}

impl ProxyState {
    pub fn new(routes: &[Route]) -> Result<Self, reqwest::Error> {
        let mut backends = BTreeMap::new();
        for route in routes {
            backends.insert(route.prefix.clone(), route.backend.clone());
        }
        // Phase 1 keeps a single shared client; per-route timeouts move into
        // the circuit-breaker wrapper in Phase 4.
        let client = reqwest::Client::builder().build()?;
        Ok(Self { backends, client })
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
        .fallback(proxy_fallback)
        .with_state(state)
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Catch-all: parse `/{prefix}[/{rest}]` and proxy to the matching backend.
async fn proxy_fallback(
    State(state): State<ProxyState>,
    request: Request,
) -> Response {
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

    let body_bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(prefix = %prefix, %err, "failed to buffer request body");
            return (StatusCode::BAD_REQUEST, "failed to read request body")
                .into_response();
        }
    };

    let started = std::time::Instant::now();
    let response = match out.body(body_bytes).send().await {
        Ok(response) => response,
        // Phase 1 surfaces backend failures as 502. Circuit breaking and
        // graceful degradation arrive in Phase 4.
        Err(err) => {
            tracing::warn!(prefix = %prefix, target = %target, %err, "backend request failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("backend {prefix:?} unreachable"),
            )
                .into_response();
        }
    };
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
            Route { prefix: "crm".into(), backend: backend_url.clone() },
            Route { prefix: "hrm".into(), backend: backend_url.clone() },
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
            &axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap(),
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
            &axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["path"], "/");
    }

    #[tokio::test]
    async fn unknown_prefix_is_404() {
        let routes = vec![Route { prefix: "crm".into(), backend: "http://127.0.0.1:1".into() }];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state);
        let response = app
            .oneshot(
                AxumRequest::get("/nope/thing").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unreachable_backend_is_502() {
        // Port 1 is reserved and refuses connections.
        let routes = vec![Route { prefix: "crm".into(), backend: "http://127.0.0.1:1".into() }];
        let state = ProxyState::new(&routes).expect("state");
        let app = build_router(state);
        let response = app
            .oneshot(
                AxumRequest::get("/crm/anything").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn post_method_is_forwarded() {
        let backend_url = spawn_mock_backend().await;
        let routes = vec![Route { prefix: "crm".into(), backend: backend_url }];
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
            &axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["method"], "POST");
        assert_eq!(body["path"], "/customers");
    }
}

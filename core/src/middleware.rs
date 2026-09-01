//! Phase 1 middleware: per-request identifiers + structured tracing.
//!
//! Every request gets a UUID (`x-request-id`), propagated to the response and
//! to a `tracing` span that also records route, status and latency. This is
//! the exact shape the audit-ledger project will ingest later.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

/// Header the gateway uses to expose/accept request ids.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Assign a request id, wrap the request in a tracing span, and record
/// status + latency on the way out.
pub async fn request_id_and_trace(request: Request, next: Next) -> Response {
    let incoming = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .filter(|value| !value.is_empty() && value.len() <= 128);
    let request_id = incoming.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let method = request.method().clone();
    let path = request
        .uri()
        .path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| request.uri().path().to_string());

    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        method = %method,
        path = %path,
    );
    let _guard = span.enter();

    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    let latency_ms = started.elapsed().as_millis() as u64;

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    tracing::info!(status = %response.status(), latency_ms, "request complete");
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn ok_handler() -> StatusCode {
        StatusCode::OK
    }

    #[tokio::test]
    async fn assigns_request_id_when_absent() {
        let app = Router::new()
            .route("/", get(ok_handler))
            .layer(from_fn(request_id_and_trace));
        let response = app
            .oneshot(
                axum::http::Request::get("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(id.len(), 36); // UUID
    }

    #[tokio::test]
    async fn preserves_incoming_request_id() {
        let app = Router::new()
            .route("/", get(ok_handler))
            .layer(from_fn(request_id_and_trace));
        let response = app
            .oneshot(
                axum::http::Request::get("/")
                    .header(REQUEST_ID_HEADER, "my-trace-42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(REQUEST_ID_HEADER).unwrap(),
            "my-trace-42"
        );
    }
}

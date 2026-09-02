//! Phase 1 middleware: per-request identifiers + structured tracing.
//!
//! Every request gets a UUID (`x-request-id`), propagated to the response and
//! to a `tracing` span that also records route, status and latency — and the
//! authenticated subject once auth has verified one
//! (`record_subject`), so each log line carries the full audit shape:
//! request id, route, status, latency, subject.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use tracing::{field::Empty, Instrument};

/// Header the gateway uses to expose/accept request ids.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Field on the request span that carries the authenticated subject. Empty
/// until (and unless) auth verifies a token; handlers call [`record_subject`]
/// to fill it in. Kept `pub` so middleware and handlers cannot drift apart
/// on the field name.
pub const SUBJECT_FIELD: &str = "subject";

/// Attach the verified subject to the current request span, so it appears on
/// the request's log line(s). A no-op outside a request span (e.g. a handler
/// reached without the tracing middleware).
pub fn record_subject(subject: &str) {
    tracing::Span::current().record(SUBJECT_FIELD, subject);
}

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
        // Declared literally, not as `SUBJECT_FIELD = Empty`: the macro
        // stringifies identifiers, which would declare a field *named*
        // "SUBJECT_FIELD" and `record_subject` would silently no-op.
        subject = Empty,
    );

    let started = std::time::Instant::now();
    let mut response = async { next.run(request).await }
        .instrument(span.clone())
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    tracing::info!(parent: &span, status = %response.status(), latency_ms, "request complete");
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
            .oneshot(axum::http::Request::get("/").body(Body::empty()).unwrap())
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

    #[test]
    fn subject_field_name_is_the_documented_constant() {
        // The field name is part of the audit-log contract (consumers key on
        // it), so pin it.
        assert_eq!(SUBJECT_FIELD, "subject");
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

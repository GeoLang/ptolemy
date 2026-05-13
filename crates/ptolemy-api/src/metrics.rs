// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Prometheus metrics middleware and endpoint.

use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use metrics::{counter, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use std::time::Instant;

static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Initialize the Prometheus metrics recorder. Safe to call multiple times (idempotent).
pub fn init_metrics() -> PrometheusHandle {
    PROMETHEUS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install Prometheus recorder")
        })
        .clone()
}

/// Middleware that records request duration and status code.
pub async fn metrics_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let start = Instant::now();

    let response = next.run(request).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let labels = [
        ("method", method.to_string()),
        ("path", normalize_path(&path)),
        ("status", status),
    ];

    counter!("http_requests_total", &labels).increment(1);
    histogram!("http_request_duration_seconds", &labels).record(duration);

    response
}

/// Record a domain-specific event (commit, merge, branch creation, etc.)
pub fn record_domain_event(event: &str) {
    counter!("ptolemy_events_total", "event" => event.to_owned()).increment(1);
}

/// Expose /metrics endpoint that returns Prometheus text format.
pub fn metrics_routes<S: Clone + Send + Sync + 'static>(handle: PrometheusHandle) -> Router<S> {
    Router::new().route("/metrics", get(move || metrics_handler(handle.clone())))
}

async fn metrics_handler(handle: PrometheusHandle) -> impl IntoResponse {
    let output = handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        output,
    )
}

/// Normalize paths to collapse UUIDs into `{id}` for sensible cardinality.
fn normalize_path(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .iter()
        .map(|s| {
            if s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4 {
                "{id}"
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

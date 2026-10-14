//! The Prometheus `/metrics` endpoint, and the middleware recording request
//! latency and concurrency.
//!
//! The series names, label names and label order are a stable contract shared
//! with homelab-go. A dashboard or alert built against them keeps working;
//! renaming one yields an empty panel rather than an error, which is a silent
//! failure. So `status` is a string label holding the numeric code and is not
//! named `code`, the labels stay in method/route/status order, and the buckets
//! stay `prometheus.DefBuckets` — which `prometheus::DEFAULT_BUCKETS` is,
//! value for value.
//!
//! The Go runtime's `go_*` series have no direct equivalent. The nearest are
//! the tokio runtime's `tokio_*` (tasks for goroutines, worker busy time for
//! the scheduler) and, with jemalloc, `jemalloc_*` for the `go_memstats_*`
//! heap figures; both are refreshed at each scrape. The `process_*` series are
//! kept: the process collector reads the same /proc fields client_golang's
//! does.

use std::sync::LazyLock;
use std::time::Instant;

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use prometheus::{Encoder, HistogramOpts, HistogramVec, IntGauge, Opts, TextEncoder};

use crate::http::Route;

static DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    let h = HistogramVec::new(
        HistogramOpts::new(
            "http_request_duration_seconds",
            "HTTP request latency in seconds.",
        )
        .buckets(prometheus::DEFAULT_BUCKETS.to_vec()),
        &["method", "route", "status"],
    )
    .expect("a valid histogram definition");
    prometheus::register(Box::new(h.clone())).expect("registered once");
    h
});

static IN_FLIGHT: LazyLock<IntGauge> = LazyLock::new(|| {
    let g = IntGauge::with_opts(Opts::new(
        "http_requests_in_flight",
        "HTTP requests currently being served.",
    ))
    .expect("a valid gauge definition");
    prometheus::register(Box::new(g.clone())).expect("registered once");
    g
});

/// Registers both series now rather than on the first request, so the gauge
/// reads 0 from the first scrape as client_golang's does.
pub(crate) fn register() {
    LazyLock::force(&DURATION);
    LazyLock::force(&IN_FLIGHT);
    crate::runtime::register();
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    crate::alloc::register();
}

/// Labels each request by its ServeMux-style pattern rather than its raw path
/// — one series per route, not one per row ever created. Anything no route
/// matched is `unmatched`.
pub(crate) async fn middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = Route::of(&req);

    IN_FLIGHT.inc();
    // A guard rather than a decrement after the await: a client that hangs up
    // drops this future part-way, and the gauge must still come back down.
    let _in_flight = Decrement;
    let start = Instant::now();
    let res = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status = res.status();
    let route = route.settle(status).pattern(&method);
    DURATION
        .with_label_values(&[
            method.as_str(),
            route.as_deref().unwrap_or("unmatched"),
            status.as_str(),
        ])
        .observe(elapsed);
    res
}

struct Decrement;

impl Drop for Decrement {
    fn drop(&mut self) {
        IN_FLIGHT.dec();
    }
}

/// Serves the default registry: these series plus the process collector.
pub async fn handler() -> Response {
    crate::runtime::refresh();
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    crate::alloc::refresh();
    let mut buf = Vec::new();
    let encoder = TextEncoder::new();
    if let Err(err) = encoder.encode(&prometheus::gather(), &mut buf) {
        tracing::error!(error = %err, "encode metrics");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    (
        [(header::CONTENT_TYPE, encoder.format_type().to_string())],
        buf,
    )
        .into_response()
}

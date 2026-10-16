//! The Prometheus `/metrics` endpoint, and the middleware recording request
//! latency and concurrency.
//!
//! The series names, label names and label order are a contract with the
//! homelab dashboards and alerts, which read them by name: renaming one gives
//! an empty panel rather than an error. `status` is a string label holding the
//! numeric code, the labels are method/route/status in that order, and the
//! buckets are `prometheus::DEFAULT_BUCKETS`.
//!
//! Alongside them, refreshed at each scrape: the tokio runtime's `tokio_*`,
//! jemalloc's `jemalloc_*` when it is the allocator, and the process
//! collector's `process_*`.

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use prometheus::{Encoder, HistogramOpts, HistogramVec, IntGauge, Opts, TextEncoder};

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

/// Registers the series now rather than on the first request, so the gauge
/// reads 0 from the first scrape.
pub(crate) fn register() {
    LazyLock::force(&DURATION);
    LazyLock::force(&IN_FLIGHT);
    crate::runtime::register();
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    crate::alloc::register();
}

/// Routes the middleware leaves out, by label ("GET /live").
#[derive(Clone)]
pub(crate) struct Labels {
    excluded: Arc<[String]>,
}

impl Labels {
    pub(crate) fn new(excluded: &[&str]) -> Self {
        Labels {
            excluded: excluded.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Records each request's latency under the route it matched, and counts it
/// in flight while it runs.
pub(crate) async fn middleware(State(labels): State<Labels>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = crate::http::route(&req);
    // Decided before the gauge is touched, so an excluded route never moves it.
    if route.as_ref().is_some_and(|r| labels.excluded.contains(r)) {
        return next.run(req).await;
    }

    IN_FLIGHT.inc();
    // A guard rather than a decrement after the await: a client that hangs up
    // drops this future part-way, and the gauge must still come back down.
    let _in_flight = Decrement;
    let start = Instant::now();
    let res = next.run(req).await;
    DURATION
        .with_label_values(&[
            method.as_str(),
            route.as_deref().unwrap_or("unmatched"),
            res.status().as_str(),
        ])
        .observe(start.elapsed().as_secs_f64());
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

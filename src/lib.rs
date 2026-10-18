//! Observability and serving for the homelab's axum services.
//!
//! The homelab's Grafana dashboards and alerts read these services' metric
//! series, log fields and span names by name, and a renamed one is an empty
//! panel rather than an error — so every app emits them through here.
//!
//! | | |
//! |---|---|
//! | [`access`] | the Cloudflare Access identity header, and the hostname's allowlist |
//! | [`logging`] | JSON lines on stdout, with the active span's trace ids |
//! | [`trace`] | OTLP/gRPC span export through tracing-opentelemetry, and a server span per request |
//! | [`metrics`] | `http_request_duration_seconds` and `http_requests_in_flight`, labelled by route |
//! | [`http`] | wires the two middlewares onto a router, and turns a panic into a 500 |
//! | [`serve`] | an HTTP/1 server with timeouts, and shutdown on SIGINT/SIGTERM |
//! | [`etag`] | strong ETags over embedded assets, so a repeat request costs a 304 |
//! | [`profiling`] | a pprof heap profile on its own listener, for Alloy |
//!
//! With the default `jemalloc` feature, on Linux, linking this crate also makes
//! jemalloc the process allocator: that is what the heap profile and the
//! `jemalloc_*` series read. The `tokio_*` runtime series need nothing extra.

/// Starts logging, and exporting spans to `otel_endpoint` — host:port of
/// Alloy's OTLP/gRPC receiver — as `service`. First thing in `main`: until it
/// runs, a log line goes nowhere. An empty endpoint logs without exporting,
/// for local dev. On an error logging has still started, so the app can log
/// why it is giving up.
pub fn init(service: &'static str, otel_endpoint: &str) -> anyhow::Result<trace::Tracing> {
    trace::init(service, otel_endpoint)
}

pub mod access;
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
mod alloc;
pub mod etag;
pub mod http;
pub mod logging;
pub mod metrics;
pub mod profiling;
mod runtime;
pub mod serve;
pub mod trace;

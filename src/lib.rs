//! Observability and serving for the homelab's axum services.
//!
//! The Rust counterpart of homelab-go, and held to the same contract: an app
//! moved from one to the other must emit the same metric series, log lines and
//! spans, because the Grafana dashboards and alerts read those by name and a
//! renamed one is an empty panel rather than an error.
//!
//! | | |
//! |---|---|
//! | [`flags`] | Go `flag`-style arguments, so a Deployment's `args` carry over unchanged |
//! | [`logging`] | slog-shaped JSON on stdout, with the active span's trace ids |
//! | [`trace`] | OTLP/gRPC span export and an otelhttp-shaped server span per request |
//! | [`metrics`] | `http_request_duration_seconds` and `http_requests_in_flight`, labelled by route |
//! | [`http`] | wires the two middlewares onto a router, with net/http's 404 and 405 |
//! | [`serve`] | an HTTP/1 server with timeouts, and shutdown on SIGINT/SIGTERM |
//! | [`etag`] | strong ETags over embedded assets, so a repeat request costs a 304 |
//! | [`profiling`] | pprof CPU and heap profiles on their own listener, for Alloy |
//!
//! With the default `jemalloc` feature, on Linux, linking this crate also makes
//! jemalloc the process allocator: that is what the heap profile and the
//! `jemalloc_*` series read. The `tokio_*` runtime series need nothing extra.

#[cfg(all(feature = "jemalloc", target_os = "linux"))]
mod alloc;
pub mod etag;
pub mod flags;
pub mod http;
pub mod logging;
pub mod metrics;
pub mod profiling;
mod runtime;
pub mod serve;
pub mod trace;

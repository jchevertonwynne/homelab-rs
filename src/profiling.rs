//! A CPU profile in pprof's format, on a dedicated listener, for Alloy's
//! pyroscope.scrape — which is what the dashboard's flame graph reads.
//!
//! Never wire this onto the app's main router. `/debug/pprof/profile` lets any
//! caller burn CPU sampling the process — fine inside the cluster, not fine on
//! a port a tunnel serves to the internet. A listener that is never added to a
//! Service cannot leak by accident.
//!
//! The CPU profile is pprof-rs sampling at 100 Hz, as Go's is. With jemalloc
//! (the default feature, Linux only) the heap profile is served too, at both
//! `/debug/pprof/heap` and `/debug/pprof/allocs`: it holds only the live
//! sampled allocations (`inuse_space`), not Go's cumulative alloc counts, so
//! it answers "what is holding memory now" and not "what churns". Go's
//! goroutine, mutex and block profiles have no Rust equivalent, and Alloy's
//! requests for those get a 404 here; the `tokio_*` series are the nearest
//! signal.
//!
//! The sampler buffers stacks in a temporary file, so it needs a writable
//! temp directory — which a scratch image with a read-only root does not have.
//! Mount an emptyDir at /tmp. Without one every profile request fails, and
//! the profiler's own error ("create profiler error") does not say why, so
//! [`spawn`] checks at startup and logs the reason.

use std::time::Duration;

use axum::Router;
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use pprof::protos::Message as _;
use serde::Deserialize;

/// Serves the profile on `addr`. A bind failure is logged, not fatal —
/// diagnostics are not worth taking the app down for.
pub fn spawn(addr: String) {
    if let Err(err) = temp_dir_writable() {
        tracing::error!(addr, error = err, "profiling server");
    }
    tokio::spawn(async move {
        let target = if addr.starts_with(':') {
            format!("0.0.0.0{addr}")
        } else {
            addr.clone()
        };
        let listener = match tokio::net::TcpListener::bind(&target).await {
            Ok(l) => l,
            Err(err) => {
                tracing::error!(addr, error = %err, "profiling server");
                return;
            }
        };
        if let Err(err) = axum::serve(listener, router()).await {
            tracing::error!(addr, error = %err, "profiling server");
        }
    });
}

fn temp_dir_writable() -> Result<(), String> {
    let dir = std::env::temp_dir();
    let probe = dir.join(format!(".profiling-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| {
        format!(
            "CPU profiling needs a writable temp directory; {}: {e}",
            dir.display()
        )
    })?;
    let _ = std::fs::remove_file(probe);
    Ok(())
}

pub fn router() -> Router {
    let router = Router::new()
        .route("/debug/pprof/", get(index))
        .route("/debug/pprof/profile", get(profile));
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    let router = router
        .route("/debug/pprof/heap", get(heap))
        .route("/debug/pprof/allocs", get(heap));
    router
}

async fn index() -> &'static str {
    if cfg!(all(feature = "jemalloc", target_os = "linux")) {
        "profile: CPU profile in pprof format; ?seconds=N, default 30\n\
         heap, allocs: live sampled allocations (inuse_space) in pprof format\n"
    } else {
        "profile: CPU profile in pprof format; ?seconds=N, default 30\n"
    }
}

/// The heap profile as it stands: a snapshot, so unlike the CPU profile there
/// is no `seconds` to wait out.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
async fn heap() -> Response {
    let result = tokio::task::spawn_blocking(crate::alloc::heap_profile)
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match result {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"heap\""),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            body,
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Could not dump heap profile: {err}\n"),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct Params {
    seconds: Option<String>,
}

/// `pprof.Profile`: sample for `seconds` (30 if absent or not a positive
/// integer), then answer with the profile.
async fn profile(Query(params): Query<Params>) -> Response {
    let seconds = params
        .seconds
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(30);
    // The sampler and the report are synchronous, and the guard is not Send,
    // so the whole thing runs on a blocking thread. The sampler is
    // process-wide: a second request while one runs fails to start, as Go's
    // does.
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .build()
            .map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_secs(seconds));
        let report = guard.report().build().map_err(|e| e.to_string())?;
        let profile = report.pprof().map_err(|e| e.to_string())?;
        let mut body = Vec::new();
        profile.encode(&mut body).map_err(|e| e.to_string())?;
        Ok(body)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    match result {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"profile\"",
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            body,
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Could not enable CPU profiling: {err}\n"),
        )
            .into_response(),
    }
}

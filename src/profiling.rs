//! A heap profile in pprof's format, on a dedicated listener, for Alloy's
//! pyroscope.scrape.
//!
//! Never wire this onto the app's main router: a profile dumps process state
//! — fine inside the cluster, not fine on a port a tunnel serves to the
//! internet. A listener that is never added to a Service cannot leak by
//! accident.
//!
//! With jemalloc (the default feature, Linux only) the heap profile is served
//! at both `/debug/pprof/heap` and `/debug/pprof/allocs`. It holds only the
//! live sampled allocations (`inuse_space`), not cumulative allocation counts,
//! so it answers "what is holding memory now" and not "what churns".
//!
//! **There is no CPU profile.** pprof-rs served one, and cost more than these
//! apps were worth profiling for: it preallocates a sample table of 16–22 MB
//! that it rebuilds after every profile and keeps, and with Alloy profiling
//! back to back the churn left jemalloc holding several times that — an app
//! that runs in 8 MB settled at 50–80 MB. Alloy's requests for the CPU profile,
//! and for the goroutine, mutex and block profiles Go programs serve, get a
//! 404 here — though Alloy is configured to ask a pod annotated
//! `pyroscope.io/profiles: "memory"` for its heap alone.
//!
//! The heap dump goes through a temporary file, so it needs a writable temp
//! directory — which a scratch image with a read-only root does not have.
//! Mount an emptyDir at /tmp. [`spawn`] checks at startup and says so if not.

use axum::Router;
use axum::routing::get;

/// Serves the profiles on `addr`. A bind failure is logged, not fatal —
/// diagnostics are not worth taking the app down for.
pub fn spawn(addr: std::net::SocketAddr) {
    if let Err(err) = temp_dir_writable() {
        tracing::error!(%addr, error = err, "profiling server");
    }
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(err) => {
                tracing::error!(%addr, error = %err, "profiling server");
                return;
            }
        };
        if let Err(err) = axum::serve(listener, router()).await {
            tracing::error!(%addr, error = %err, "profiling server");
        }
    });
}

fn temp_dir_writable() -> Result<(), String> {
    let dir = std::env::temp_dir();
    let probe = dir.join(format!(".profiling-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| {
        format!(
            "heap profiling needs a writable temp directory; {}: {e}",
            dir.display()
        )
    })?;
    let _ = std::fs::remove_file(probe);
    Ok(())
}

pub fn router() -> Router {
    let router = Router::new().route("/debug/pprof/", get(index));
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    let router = router
        .route("/debug/pprof/heap", get(heap))
        .route("/debug/pprof/allocs", get(heap));
    router
}

async fn index() -> &'static str {
    if cfg!(all(feature = "jemalloc", target_os = "linux")) {
        "heap, allocs: live sampled allocations (inuse_space) in pprof format\n"
    } else {
        "no profiles: the heap profile needs jemalloc on Linux\n"
    }
}

/// The heap profile as it stands: a snapshot, nothing to wait out.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
async fn heap() -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

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

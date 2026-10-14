//! Serves a router with the timeouts every app on the cluster needs, and shuts
//! it down on SIGTERM.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use axum::Router;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

/// Without it one client can hold a connection open indefinitely by dribbling
/// out headers, and enough of those exhaust the server without ever completing
/// a request.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest a request may take end to end before its connection is closed
/// — net/http's WriteTimeout, which also has the connection dropped rather
/// than answered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Kubernetes sends SIGTERM and waits terminationGracePeriodSeconds (30) before
/// SIGKILL; this is what keeps the drain inside that window.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What to do between the signal and the drain. For anything holding a
/// response open that the drain would otherwise wait out — an SSE hub, whose
/// streams only end when it closes.
pub type PreShutdown = Box<dyn FnOnce() + Send>;

/// Serves `router` on `addr` (Go's spelling: `:8092` is every interface) until
/// SIGINT or SIGTERM, then drains in-flight requests.
///
/// An error means the listener could not be bound or the drain overran, both
/// worth exiting non-zero over: Kubernetes should restart a pod that cannot
/// serve. Anything to flush on the way out goes after this returns.
pub async fn run(addr: &str, router: Router) -> Result<()> {
    run_with(addr, router, None).await
}

pub async fn run_with(addr: &str, router: Router, pre_shutdown: Option<PreShutdown>) -> Result<()> {
    let listener = bind(addr).await?;
    tracing::info!(addr, "listening");

    let graceful = GracefulShutdown::new();
    let mut stop = std::pin::pin!(stopped());
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let router = router.clone();
                    let service = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
                        req.extensions_mut().insert(ConnectInfo(peer));
                        let router = router.clone();
                        async move {
                            match tokio::time::timeout(REQUEST_TIMEOUT, router.oneshot(req)).await {
                                Ok(Ok(res)) => Ok(res),
                                Ok(Err(never)) => match never {},
                                // Returning an error is what makes hyper close
                                // the connection instead of answering.
                                Err(_) => Err(TimedOut),
                            }
                        }
                    });
                    let conn = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .serve_connection(TokioIo::new(stream), service);
                    let conn = graceful.watch(conn);
                    // A connection that errors (a reset, a header timeout) is
                    // the client's business; net/http logs nothing for it either.
                    tokio::spawn(async move { let _ = conn.await; });
                }
                // EMFILE and friends: net/http backs off and retries rather
                // than giving up on the listener, and so does this.
                Err(err) => {
                    tracing::error!(error = %err, "accept");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = &mut stop => break,
        }
    }
    drop(listener);

    tracing::info!("shutting down");
    if let Some(f) = pre_shutdown {
        f();
    }
    tokio::time::timeout(SHUTDOWN_TIMEOUT, graceful.shutdown())
        .await
        .map_err(|_| anyhow!("shutdown: context deadline exceeded"))
}

/// `:8092` means every IPv4 interface here. Go's means every interface, but a
/// pod's network namespace may have IPv6 disabled, and binding `[::]` there
/// fails where `0.0.0.0` cannot.
async fn bind(addr: &str) -> Result<TcpListener> {
    let target = if addr.starts_with(':') {
        format!("0.0.0.0{addr}")
    } else {
        addr.to_string()
    };
    TcpListener::bind(&target)
        .await
        .with_context(|| format!("listen and serve: listen tcp {addr}"))
}

/// Done when the cluster, or a Ctrl-C, asks the server to stop.
async fn stopped() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        // Without the handler the server still runs; it is only stopped less gently.
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// The error that closes a connection whose request overran REQUEST_TIMEOUT.
#[derive(Debug)]
struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request timed out")
    }
}

impl std::error::Error for TimedOut {}

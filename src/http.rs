//! Wires the request middleware onto a router.
//!
//! Every request is labelled by the route it matched — `"GET /entries/{id}"`,
//! the method and axum's matched path — never its raw path, so a metric has
//! one series per route rather than one per row ever created, and a span is
//! named for what it did. A request no route matched is `unmatched`.
//!
//! A handler that panics is a 500, counted under its route and logged inside
//! its span, rather than a connection reset that no metric or trace records.

use std::any::Any;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use tower_http::catch_panic::CatchPanicLayer;

/// Puts the metrics middleware and a server span around every route of
/// `router`, its fallback included.
///
/// Call it after every route is added: axum applies a layer only to the routes
/// that exist when it is applied.
pub fn instrument(router: Router) -> Router {
    crate::metrics::register();
    router
        // Innermost, so the 500 it makes of a panic is what the metrics and
        // the span see.
        .layer(CatchPanicLayer::custom(panicked))
        .layer(from_fn_with_state(
            crate::metrics::Labels::new(&[]),
            crate::metrics::middleware,
        ))
        .layer(from_fn(crate::trace::middleware))
}

/// For an app that serves most of its routes behind a wrapper — an
/// authentication layer — with a few unwrapped ones beside them (`/healthz`,
/// `/metrics`).
///
/// `outer` holds the unwrapped routes and `app` everything else; `app` is
/// served for any request `outer` has no route for. Bring `app` with its
/// wrapper already applied, and its fallback added before the wrapper, so the
/// wrapper covers unknown paths too.
///
/// A request is labelled by the route it matched in whichever router served
/// it, and the label is fixed before `app`'s wrapper runs, so a request the
/// wrapper refuses is still counted under the route it was for. Routes in
/// `excluded` (`"GET /live"`) are kept out of the metrics entirely: a stream
/// open for hours would make the latency histogram and the in-flight gauge
/// lie for as long as it lasted.
pub fn instrument_layered(outer: Router, app: Router, excluded: &[&str]) -> Router {
    crate::metrics::register();
    let labels = crate::metrics::Labels::new(excluded);
    // Outermost on app, so both run before app's own wrapper; the panic
    // catcher inside them, as in instrument.
    let app = app
        .layer(CatchPanicLayer::custom(panicked))
        .layer(from_fn_with_state(
            labels.clone(),
            crate::metrics::middleware,
        ))
        .layer(from_fn(record_route));
    outer
        // route_layer: outer's own routes only. A request handed to app is
        // counted there, once.
        .route_layer(CatchPanicLayer::custom(panicked))
        .route_layer(from_fn_with_state(labels, crate::metrics::middleware))
        .fallback_service(app)
        .layer(from_fn(crate::trace::middleware))
}

/// Where a nested router writes the route it matched, for the span started
/// outside it: the outer router only knows the request fell through to its
/// fallback.
#[derive(Clone, Default)]
pub(crate) struct RouteSlot(pub Arc<Mutex<Option<String>>>);

impl RouteSlot {
    pub(crate) fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

async fn record_route(req: Request, next: Next) -> Response {
    if let (Some(slot), Some(route)) = (req.extensions().get::<RouteSlot>(), route(&req)) {
        *slot.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(route);
    }
    next.run(req).await
}

/// The route a request matched, as its label: the method and the matched path.
pub(crate) fn route(req: &Request) -> Option<String> {
    req.extensions()
        .get::<MatchedPath>()
        .map(|p| format!("{} {}", req.method(), p.as_str()))
}

/// The response for a handler that panicked. The message goes to the log, not
/// the client; Rust's panic hook has already written where it happened to
/// stderr.
fn panicked(panic: Box<dyn Any + Send + 'static>) -> Response {
    let msg = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic payload".to_string());
    tracing::error!(panic = msg, "handler panicked");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

/// A plain-text response for a status and a message, `nosniff` so it is never
/// read as anything else.
pub fn error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        msg.to_string(),
    )
        .into_response()
}

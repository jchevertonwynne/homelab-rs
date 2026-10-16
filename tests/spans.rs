//! Request spans are named for the route that matched, in a plain router and
//! in a layered one — where the match happens in the router nested inside.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::routing::get;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tower::ServiceExt as _;

#[tokio::test]
async fn spans_are_named_for_their_route() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());

    let plain =
        homelab::http::instrument(Router::new().route("/entries/{id}", get(|| async { "e" })));
    let layered = homelab::http::instrument_layered(
        Router::new().route("/healthz", get(|| async { "ok" })),
        Router::new().route("/collections/{collection}", get(|| async { "c" })),
        &[],
    );
    for (router, path) in [
        (&plain, "/entries/7"),
        (&plain, "/nowhere"),
        (&layered, "/healthz"),
        (&layered, "/collections/3"),
        (&layered, "/nowhere"),
    ] {
        router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
    }
    let _ = provider.force_flush();
    let mut names: Vec<String> = exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .map(|s| s.name.into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "GET",
            "GET",
            "GET /collections/{collection}",
            "GET /entries/{id}",
            "GET /healthz"
        ]
    );
}

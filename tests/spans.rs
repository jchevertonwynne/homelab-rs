//! What Tempo and Loki are queried by, through the whole pipeline — the log
//! subscriber, tracing-opentelemetry and an exporter: request spans named for
//! the route that matched, in a plain router and in a layered one; an app's
//! own `#[instrument]` spans beneath them; error status for a 5xx and for an
//! `err`; a panic as a counted 500; and the ids on log lines.
//!
//! One test, because the subscriber and the tracer provider are global to the
//! process.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::get;
use http_body_util::BodyExt as _;
use opentelemetry::trace::{SpanId, SpanKind, Status, TraceId};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tower::ServiceExt as _;

const PARENT_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_SPAN: &str = "00f067aa0ba902b7";

#[tracing::instrument(name = "db.query", skip_all, fields(db.system = "sqlite"), err)]
async fn query(fail: bool) -> Result<u32, String> {
    if fail {
        Err("disk I/O error".into())
    } else {
        Ok(7)
    }
}

async fn entry() -> String {
    tracing::info!("in handler");
    query(false).await.unwrap().to_string()
}

async fn failing() -> StatusCode {
    match query(true).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Marks its span an error without logging, as an app does when its caller
/// logs the error itself.
#[tracing::instrument(name = "notes.save", skip_all)]
async fn save() -> StatusCode {
    use homelab::trace::OpenTelemetrySpanExt as _;
    tracing::Span::current().set_status(homelab::trace::Status::error("disk full"));
    StatusCode::OK
}

async fn boom() -> &'static str {
    panic!("kaboom")
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
    span.attributes
        .iter()
        .find(|kv: &&KeyValue| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name == name).collect()
}

#[tokio::test]
async fn spans_and_log_lines_keep_their_shape() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let (_tracing, lines) = homelab::trace::init_capturing(provider.clone());

    let plain = homelab::http::instrument(
        Router::new()
            .route("/entries/{id}", get(entry))
            .route("/fail", get(failing))
            .route("/boom", get(boom))
            .route("/save", get(save)),
    );
    let layered = homelab::http::instrument_layered(
        Router::new().route("/healthz", get(|| async { "ok" })),
        Router::new().route("/collections/{collection}", get(|| async { "c" })),
        &[],
    );

    tracing::info!("outside any request");
    let traced = Request::builder()
        .uri("/entries/7")
        .header("traceparent", format!("00-{PARENT_TRACE}-{PARENT_SPAN}-01"))
        .body(Body::empty())
        .unwrap();
    let res = plain.clone().oneshot(traced).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut statuses = Vec::new();
    for (router, path) in [
        (&plain, "/nowhere"),
        (&plain, "/fail"),
        (&plain, "/boom"),
        (&plain, "/save"),
        (&layered, "/healthz"),
        (&layered, "/collections/3"),
        (&layered, "/nowhere"),
    ] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        statuses.push(router.clone().oneshot(req).await.unwrap().status());
    }
    assert_eq!(statuses[1], StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        statuses[2],
        StatusCode::INTERNAL_SERVER_ERROR,
        "a panic is a 500"
    );

    let _ = provider.force_flush();
    let spans = exporter.get_finished_spans().unwrap();
    let mut names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "GET",
            "GET",
            "GET /boom",
            "GET /collections/{collection}",
            "GET /entries/{id}",
            "GET /fail",
            "GET /healthz",
            "GET /save",
            "db.query",
            "db.query",
            "notes.save",
        ]
    );

    // The request span: a server span continuing the incoming trace, with the
    // semantic-convention attributes.
    let server = named(&spans, "GET /entries/{id}")[0];
    assert_eq!(server.span_kind, SpanKind::Server);
    assert_eq!(
        server.span_context.trace_id(),
        TraceId::from_hex(PARENT_TRACE).unwrap()
    );
    assert_eq!(
        server.parent_span_id,
        SpanId::from_hex(PARENT_SPAN).unwrap()
    );
    assert_eq!(
        attr(server, "http.route"),
        Some(&Value::from("/entries/{id}"))
    );
    assert_eq!(
        attr(server, "http.request.method"),
        Some(&Value::from("GET"))
    );
    assert_eq!(attr(server, "url.path"), Some(&Value::from("/entries/7")));
    assert_eq!(
        attr(server, "http.response.status_code"),
        Some(&Value::I64(200))
    );
    assert_eq!(server.status, Status::Unset);

    // The app's own span, beneath it.
    let ok_query = named(&spans, "db.query")
        .into_iter()
        .find(|s| s.status == Status::Unset)
        .expect("the successful query's span");
    assert_eq!(ok_query.parent_span_id, server.span_context.span_id());
    assert_eq!(
        ok_query.span_context.trace_id(),
        server.span_context.trace_id()
    );
    assert_eq!(attr(ok_query, "db.system"), Some(&Value::from("sqlite")));

    // Errors: an `err` marks its span, a 5xx marks the request's.
    let failed_query = named(&spans, "db.query")
        .into_iter()
        .find(|s| matches!(s.status, Status::Error { .. }))
        .expect("the failed query's span is an error");
    let fail = named(&spans, "GET /fail")[0];
    assert_eq!(failed_query.parent_span_id, fail.span_context.span_id());
    assert!(matches!(fail.status, Status::Error { .. }));
    assert!(matches!(
        named(&spans, "GET /boom")[0].status,
        Status::Error { .. }
    ));
    assert_eq!(named(&spans, "GET /healthz")[0].status, Status::Unset);
    // Marked by hand: an error span, but a 200 request, and nothing logged.
    assert!(matches!(
        named(&spans, "notes.save")[0].status,
        Status::Error { .. }
    ));
    assert_eq!(named(&spans, "GET /save")[0].status, Status::Unset);
    assert_eq!(attr(server, "code.file.path"), None, "no source location");

    // The panic is counted under its route.
    let metrics = homelab::metrics::handler()
        .await
        .into_body()
        .collect()
        .await
        .unwrap();
    let metrics = String::from_utf8(metrics.to_bytes().to_vec()).unwrap();
    assert!(
        metrics.contains(
            r#"http_request_duration_seconds_count{method="GET",route="GET /boom",status="500"} 1"#
        ),
        "{metrics}"
    );

    // Log lines: the ids of the span they were logged in, and none outside.
    let lines = lines.lock().unwrap().clone();
    let line = |msg: &str| {
        lines
            .iter()
            .find(|l| l.contains(&format!(r#""msg":"{msg}""#)))
            .unwrap_or_else(|| panic!("no {msg:?} line in {lines:#?}"))
            .clone()
    };
    assert!(!line("outside any request").contains("trace_id"));
    let in_handler = line("in handler");
    assert!(
        in_handler.ends_with(&format!(
            r#""trace_id":"{PARENT_TRACE}","span_id":"{}"}}"#,
            server.span_context.span_id()
        )),
        "{in_handler}"
    );
    let query_error = line("db.query");
    assert!(query_error.contains(r#""level":"ERROR""#), "{query_error}");
    assert!(
        query_error.contains(r#""error":"disk I/O error""#),
        "{query_error}"
    );
    assert!(
        query_error.contains(&format!(
            r#""span_id":"{}""#,
            failed_query.span_context.span_id()
        )),
        "{query_error}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("notes.save")),
        "{lines:#?}"
    );
    let panicked = line("handler panicked");
    assert!(panicked.contains(r#""panic":"kaboom""#), "{panicked}");
    assert!(
        panicked.contains(&format!(
            r#""trace_id":"{}""#,
            named(&spans, "GET /boom")[0].span_context.trace_id()
        )),
        "{panicked}"
    );
}

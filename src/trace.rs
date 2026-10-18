//! Spans exported over OTLP/gRPC to Alloy's in-cluster receiver, which
//! forwards them to Grafana Cloud Tempo. Always sampled — Alloy is the only hop
//! and the traffic here is nowhere near worth trimming.
//!
//! The request span follows OpenTelemetry's HTTP semantic conventions: a
//! server span named for the route it matched (`"GET /entries/{id}"`), or the
//! method alone when nothing matched, an error only for a 5xx, and the
//! standard request and response attributes. A W3C `traceparent` on the
//! request continues its trace rather than starting a new one.
//!
//! Every span is a `tracing` span, exported through tracing-opentelemetry: an
//! app's own are `#[tracing::instrument]` on the functions worth seeing —
//! `#[instrument(name = "db.query", skip_all, fields(db.system = "sqlite"),
//! err)]` — and the dashboards read them by that name. `err` marks the span an
//! error and logs the error once, at ERROR, named for the span.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Method, Version, header};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::global;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use tracing::Instrument as _;
use tracing::field::Empty;

use crate::logging::Sink;

/// For an app to mark its own span an error without logging, when the caller
/// already logs it: `Span::current().set_status(Status::error(..))`.
/// Re-exported so an app's versions cannot drift from this crate's.
pub use opentelemetry::trace::Status;
pub use tracing_opentelemetry::OpenTelemetrySpanExt;

/// How many finished spans wait for export at most. The SDK's default is
/// 2048, and its queue holds a slot for each from the start — about 770 kB,
/// the largest single thing in these apps' heaps. A batch goes every five
/// seconds and these apps finish a few spans a second, so 256 is still far
/// more than a batch ever holds; past it, spans are dropped and the SDK
/// says so at WARN.
const QUEUE: usize = 256;

/// What [`crate::init`] started. Shut it down after the server stops, or the
/// last batch of spans is dropped.
pub struct Tracing {
    provider: Option<SdkTracerProvider>,
}

/// See [`crate::init`].
pub(crate) fn init(service: &'static str, endpoint: &str) -> Result<Tracing> {
    if endpoint.is_empty() {
        return Ok(start(None, Sink::Stdout));
    }
    match provider(service, endpoint) {
        Ok(provider) => Ok(start(Some(provider), Sink::Stdout)),
        Err(err) => {
            // Logging still starts, so the app can say why it is exiting.
            crate::logging::install(Sink::Stdout, None);
            Err(err)
        }
    }
}

/// Exports to Alloy's OTLP/gRPC receiver at `endpoint`, plaintext because it
/// never leaves the cluster network.
fn provider(service: &'static str, endpoint: &str) -> Result<SdkTracerProvider> {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(format!("http://{endpoint}"))
        .build()
        .context("create otlp exporter")?;
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(QUEUE)
        .with_max_export_batch_size(QUEUE)
        .build();
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch)
        .build();
    Ok(SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(Resource::builder().with_service_name(service).build())
        .build())
}

fn start(provider: Option<SdkTracerProvider>, sink: Sink) -> Tracing {
    global::set_text_map_propagator(TraceContextPropagator::new());
    if let Some(provider) = &provider {
        global::set_tracer_provider(provider.clone());
    }
    let tracer = provider.as_ref().map(|p| p.tracer("homelab"));
    crate::logging::install(sink, tracer);
    Tracing { provider }
}

/// [`crate::init`] for a test: spans go to `provider`, and the log lines are
/// kept and returned rather than written.
#[doc(hidden)]
pub fn init_capturing(provider: SdkTracerProvider) -> (Tracing, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::default();
    let tracing = start(Some(provider), Sink::Capture(Arc::clone(&lines)));
    (tracing, lines)
}

impl Tracing {
    /// Sends what is still batched. Its error has nowhere useful to go — the
    /// process is exiting — so it is logged and otherwise ignored.
    pub async fn shutdown(self) {
        let Some(provider) = self.provider else {
            return;
        };
        // Off the runtime's own threads: the flush waits on them.
        let sent = tokio::task::spawn_blocking(move || provider.shutdown()).await;
        if let Ok(Err(err)) = sent {
            tracing::error!(error = %err, "shutdown tracing");
        }
    }
}

/// A server span around every request, probes and scrapes included, named
/// for the route it matched — `"GET /entries/{id}"` — or for its method alone
/// when none did.
pub(crate) async fn middleware(mut req: Request, next: Next) -> Response {
    let parent = global::get_text_map_propagator(|p| p.extract(&Headers(req.headers())));
    let method = req.method().clone();
    // The outer router's own match, if it has one; a router nested in its
    // fallback fills in the slot instead (see http::instrument_layered).
    let matched = crate::http::route(&req);
    let slot = crate::http::RouteSlot::default();
    req.extensions_mut().insert(slot.clone());

    // Every attribute is declared here, because a tracing span's fields are
    // fixed when it is created; the ones a request lacks stay empty and are
    // not exported.
    let span = tracing::info_span!(
        target: "homelab::http",
        "request",
        otel.name = span_name(&method, None),
        otel.kind = "server",
        otel.status_code = Empty,
        server.address = Empty,
        server.port = Empty,
        http.request.method = Empty,
        http.request.method_original = Empty,
        url.scheme = "http",
        url.path = Empty,
        network.peer.address = Empty,
        network.peer.port = Empty,
        network.protocol.version = Empty,
        user_agent.original = Empty,
        client.address = Empty,
        http.route = Empty,
        http.response.status_code = Empty,
        http.response.body.size = Empty,
    );
    // Before the span is first entered: that is when it starts, and its parent
    // is fixed from then on.
    let _ = span.set_parent(parent);
    record_request(&span, &req);

    let res = next.run(req).instrument(span.clone()).await;

    if let Some(route) = matched.or_else(|| slot.take()) {
        let path = route.split_once(' ').map_or(route.as_str(), |(_, p)| p);
        span.record("http.route", path);
        // Through the OpenTelemetry span rather than recording otel.name: the
        // span started when it was first entered, and tracing-opentelemetry
        // applies only attributes and status to a started span.
        span.context()
            .span()
            .update_name(span_name(&method, Some(path)));
    }
    if let Some(size) = res.body().size_hint().exact().filter(|&n| n > 0) {
        span.record("http.response.body.size", size as i64);
    }
    let status = res.status();
    span.record("http.response.status_code", i64::from(status.as_u16()));
    if status.is_server_error() {
        span.record("otel.status_code", "ERROR");
    }
    res
}

const KNOWN_METHODS: [Method; 9] = [
    Method::CONNECT,
    Method::DELETE,
    Method::GET,
    Method::HEAD,
    Method::OPTIONS,
    Method::PATCH,
    Method::POST,
    Method::PUT,
    Method::TRACE,
];

/// `"{method} {route}"`, or the method alone with no route. A method outside
/// the standard set is "HTTP", which keeps a scanner's invented verbs from
/// minting span names.
fn span_name(method: &Method, path: Option<&str>) -> String {
    let method = if KNOWN_METHODS.contains(method) {
        method.as_str()
    } else {
        "HTTP"
    };
    match path {
        Some(path) => format!("{method} {path}"),
        None => method.to_string(),
    }
}

/// The semantic-convention request attributes.
fn record_request(span: &tracing::Span, req: &Request) {
    let host = req
        .uri()
        .authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| header_str(req.headers(), header::HOST.as_str()))
        .unwrap_or_default();
    let (host, port) = split_host_port(&host);
    span.record("server.address", host);
    if let Some(port) = port.filter(|&p| p != 80) {
        span.record("server.port", i64::from(port));
    }

    let method = req.method();
    if KNOWN_METHODS.contains(method) {
        span.record("http.request.method", method.as_str());
    } else {
        span.record("http.request.method", "_OTHER");
        span.record("http.request.method_original", method.as_str());
    }

    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    if let Some(peer) = peer {
        span.record("network.peer.address", peer.ip().to_string());
        span.record("network.peer.port", i64::from(peer.port()));
    }
    if let Some(ua) =
        header_str(req.headers(), header::USER_AGENT.as_str()).filter(|s| !s.is_empty())
    {
        span.record("user_agent.original", ua);
    }
    // The first X-Forwarded-For entry, else the peer. Behind cloudflared that
    // is cloudflared's view of the visitor.
    let client = header_str(req.headers(), "x-forwarded-for")
        .map(|xff| xff.split(',').next().unwrap_or_default().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| peer.map(|p| p.ip().to_string()));
    if let Some(client) = client {
        span.record("client.address", client);
    }
    span.record("url.path", decoded_path(req.uri().path()));
    if let Some(version) = protocol_version(req.version()) {
        span.record("network.protocol.version", version);
    }
}

/// url.path is the decoded path.
pub(crate) fn decoded_path(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

fn protocol_version(v: Version) -> Option<&'static str> {
    Some(match v {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_11 => "1.1",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => return None,
    })
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

/// A Host header's `host[:port]` or `[v6]:port`.
fn split_host_port(hostport: &str) -> (String, Option<u16>) {
    if let Some(rest) = hostport.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
        return (host.to_string(), port);
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host.to_string(), port.parse().ok()),
        _ => (hostport.to_string(), None),
    }
}

struct Headers<'a>(&'a HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_spans_by_route_or_method() {
        assert_eq!(span_name(&Method::GET, Some("/")), "GET /");
        assert_eq!(span_name(&Method::GET, None), "GET");
        let odd = Method::from_bytes(b"PROPFIND").unwrap();
        assert_eq!(span_name(&odd, Some("/x")), "HTTP /x");
    }

    #[test]
    fn splits_hosts_as_go_does() {
        assert_eq!(
            split_host_port("echo.example:8092"),
            ("echo.example".into(), Some(8092))
        );
        assert_eq!(
            split_host_port("echo.example"),
            ("echo.example".into(), None)
        );
        assert_eq!(split_host_port("[::1]:80"), ("::1".into(), Some(80)));
    }
}

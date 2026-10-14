//! Spans exported over OTLP/gRPC to Alloy's in-cluster receiver, which
//! forwards them to Grafana Cloud Tempo. Always sampled — Alloy is the only hop
//! and the traffic here is nowhere near worth trimming.
//!
//! The request span is shaped as otelhttp shapes it, because the dashboards
//! query spans by name and `resource.service.name`: named `"GET /{$}"` once the
//! route is known and just `"GET"` when nothing matched, a server span, an
//! error only for a 5xx, with the same semantic-convention attributes. A W3C
//! `traceparent` on the request continues its trace rather than starting a new
//! one.

use std::net::SocketAddr;

use anyhow::{Context as _, Result};
use axum::body::HttpBody as _;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Method, Version, header};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::context::FutureExt as _;
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;

use crate::http::Route;

/// What [`init`] started. Shut it down after the server stops, or the last
/// batch of spans is dropped.
pub struct Tracing {
    provider: Option<SdkTracerProvider>,
}

/// Points the global tracer provider at `endpoint` (host:port of Alloy's
/// OTLP/gRPC receiver), plaintext because it never leaves the cluster network.
/// An empty endpoint leaves the no-op tracer in place, for local dev.
pub fn init(service: &'static str, endpoint: &str) -> Result<Tracing> {
    if endpoint.is_empty() {
        return Ok(Tracing { provider: None });
    }
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(format!("http://{endpoint}"))
        .build()
        .context("create otlp exporter")?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name(service).build())
        .build();
    global::set_tracer_provider(provider.clone());
    global::set_text_map_propagator(TraceContextPropagator::new());
    Ok(Tracing {
        provider: Some(provider),
    })
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

/// A server span around every request, as `otelhttp.NewHandler` gives one.
/// Probes and scrapes are included, as they are in the Go apps.
pub(crate) async fn middleware(req: Request, next: Next) -> Response {
    let parent = global::get_text_map_propagator(|p| p.extract(&Headers(req.headers())));
    let method = req.method().clone();
    let route = Route::of(&req);

    let tracer = global::tracer("homelab");
    let span = tracer
        .span_builder(span_name(&method, None))
        .with_kind(SpanKind::Server)
        .with_attributes(request_attrs(&req))
        .start_with_context(&tracer, &parent);
    let cx = parent.with_span(span);

    let res = next.run(req).with_context(cx.clone()).await;

    let span = cx.span();
    let status = res.status();
    let route = route.settle(status);
    if let Some(path) = &route.path {
        // Name only: otelhttp reads the pattern for http.route before the mux
        // has set it, so the Go spans never carried that attribute.
        span.update_name(span_name(&method, Some(path)));
    }
    if let Some(size) = res.body().size_hint().exact().filter(|&n| n > 0) {
        span.set_attribute(KeyValue::new("http.response.body.size", size as i64));
    }
    span.set_attribute(KeyValue::new(
        "http.response.status_code",
        i64::from(status.as_u16()),
    ));
    if status.is_server_error() {
        span.set_status(Status::error(""));
    }
    span.end();
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

/// otelhttp's request attributes, in its order.
fn request_attrs(req: &Request) -> Vec<KeyValue> {
    let host = req
        .uri()
        .authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| header_str(req.headers(), header::HOST.as_str()))
        .unwrap_or_default();
    let (host, port) = split_host_port(&host);

    let mut attrs = Vec::with_capacity(12);
    attrs.push(KeyValue::new("server.address", host));
    let method = req.method();
    if KNOWN_METHODS.contains(method) {
        attrs.push(KeyValue::new("http.request.method", method.to_string()));
    } else {
        attrs.push(KeyValue::new("http.request.method", "_OTHER"));
    }
    attrs.push(KeyValue::new("url.scheme", "http"));
    if let Some(port) = port.filter(|&p| p != 80) {
        attrs.push(KeyValue::new("server.port", i64::from(port)));
    }
    if !KNOWN_METHODS.contains(method) {
        attrs.push(KeyValue::new(
            "http.request.method_original",
            method.to_string(),
        ));
    }

    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    if let Some(peer) = peer {
        attrs.push(KeyValue::new("network.peer.address", peer.ip().to_string()));
        attrs.push(KeyValue::new("network.peer.port", i64::from(peer.port())));
    }
    if let Some(ua) =
        header_str(req.headers(), header::USER_AGENT.as_str()).filter(|s| !s.is_empty())
    {
        attrs.push(KeyValue::new("user_agent.original", ua));
    }
    // The first X-Forwarded-For entry, untrimmed, else the peer — otelhttp's
    // order. Behind cloudflared that is cloudflared's view of the visitor.
    let client = header_str(req.headers(), "x-forwarded-for")
        .map(|xff| xff.split(',').next().unwrap_or_default().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| peer.map(|p| p.ip().to_string()));
    if let Some(client) = client {
        attrs.push(KeyValue::new("client.address", client));
    }
    attrs.push(KeyValue::new("url.path", decoded_path(req.uri().path())));
    if let Some(version) = protocol_version(req.version()) {
        attrs.push(KeyValue::new("network.protocol.version", version));
    }
    attrs
}

/// Go's `r.URL.Path` is the decoded path, so the attribute is too.
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

/// `host[:port]` or `[v6]:port`, as Go's SplitHostPort reads a Host header.
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

/// Runs `fut` inside a child span of the current one, recording its error on
/// the span — `tracing.Op` from homelab-go. Wrapping a store method's body this
/// way makes a slow query its own span rather than time folded into the
/// handler that called it.
pub async fn op<T, E: std::fmt::Display>(
    tracer: &'static str,
    name: &'static str,
    attrs: Vec<KeyValue>,
    fut: impl Future<Output = std::result::Result<T, E>>,
) -> std::result::Result<T, E> {
    let tracer = global::tracer(tracer);
    let span = tracer
        .span_builder(name)
        .with_attributes(attrs)
        .start_with_context(&tracer, &Context::current());
    let cx = Context::current_with_span(span);
    let result = fut.with_context(cx.clone()).await;
    let span = cx.span();
    if let Err(err) = &result {
        span.add_event(
            "exception",
            vec![KeyValue::new("exception.message", err.to_string())],
        );
        span.set_status(Status::error(err.to_string()));
    }
    span.end();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_spans_as_otelhttp_does() {
        assert_eq!(span_name(&Method::GET, Some("/{$}")), "GET /{$}");
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

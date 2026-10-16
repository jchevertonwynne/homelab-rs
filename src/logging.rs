//! The process-wide log output: one JSON object per line on stdout —
//! `time`, `level`, `msg`, then the event's fields — with the active span's
//! trace and span ids on every record logged inside a request.
//!
//! Alloy tails stdout into Loki and lifts two fields out of each line: `level`
//! becomes a label (so the values must stay `INFO`, `WARN`, `ERROR`,
//! uppercase, as the dashboards and the Loki queries spell them) and `trace_id` becomes structured metadata, which is what links a
//! log line to its trace in Tempo. Everything else is carried through as-is.
//!
//! Log with the `tracing` macros: `tracing::error!(error = %err, "encode
//! response")` writes `{"time":…,"level":"ERROR","msg":"encode response",
//! "error":"…"}`.
//!
//! Never log user identifiers or user content through this — no email, session
//! or CSRF value, Access JWT, request header or body. These lines leave the
//! cluster and sit in Loki with its retention.

use std::fmt::{self, Write as _};
use std::io::Write as _;

use opentelemetry::trace::TraceContextExt;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, SubscriberExt};

/// Must run before anything that logs; until it does, events go nowhere.
pub fn init() {
    // The OpenTelemetry SDK narrates its own lifecycle at INFO ("Global tracer
    // provider is set"), which is noise in Loki; its warnings and errors —
    // an export that failed — are worth keeping.
    let filter = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("opentelemetry", LevelFilter::WARN)
        .with_target("opentelemetry_sdk", LevelFilter::WARN)
        .with_target("opentelemetry_otlp", LevelFilter::WARN);
    let subscriber = tracing_subscriber::registry().with(filter).with(JsonLines);
    // Only fails if a subscriber is already installed, which means init ran
    // twice; the first one is the one that counts.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

struct JsonLines;

impl<S: Subscriber> Layer<S> for JsonLines {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut line = render(event, &jiff::Zoned::now());
        line.push('\n');
        // One write per line under the lock, so concurrent requests cannot
        // interleave inside a record. A failed write to stdout has nowhere
        // else to be reported.
        let _ = std::io::stdout().lock().write_all(line.as_bytes());
    }
}

/// The record without its newline. Split from `on_event` so tests can pin the
/// clock.
fn render(event: &Event<'_>, now: &jiff::Zoned) -> String {
    let mut fields = Fields::default();
    event.record(&mut fields);
    // The OpenTelemetry SDK logs events with a name and no message
    // ("BatchSpanProcessor.ExportError"); the name is the only thing that says
    // what the line is about, so it stands in.
    let msg = if fields.msg.is_empty() {
        fields
            .attrs
            .iter()
            .find(|(k, _)| *k == "name")
            .and_then(|(_, v)| serde_json::from_str::<String>(v).ok())
            .unwrap_or_default()
    } else {
        fields.msg.clone()
    };
    line(level(event.metadata().level()), &msg, &fields.attrs, now)
}

/// One record from its parts, with each attr already encoded as JSON.
fn line(level: &str, msg: &str, attrs: &[(&str, String)], now: &jiff::Zoned) -> String {
    let mut out = String::with_capacity(160);
    out.push_str("{\"time\":");
    push_json_str(&mut out, &timestamp(now));
    out.push_str(",\"level\":");
    push_json_str(&mut out, level);
    out.push_str(",\"msg\":");
    push_json_str(&mut out, msg);
    for (key, value) in attrs {
        out.push(',');
        push_json_str(&mut out, key);
        out.push(':');
        out.push_str(value);
    }
    // After the record's own attrs. An
    // absent or unsampled span yields all-zero ids, which would look like a
    // trace id and invite a Tempo lookup that can only come back empty.
    let cx = opentelemetry::Context::current();
    let span = cx.span();
    let sc = span.span_context();
    if sc.is_valid() {
        out.push_str(&format!(
            ",\"trace_id\":\"{}\",\"span_id\":\"{}\"",
            sc.trace_id(),
            sc.span_id()
        ));
    }
    out.push('}');
    out
}

/// An INFO record whose string fields are only known at runtime — the
/// `"listening"` line, which an app extends with its own attributes. tracing's
/// macros need their field names at compile time; this writes the same shape.
pub fn info_with(msg: &str, attrs: &[(&str, String)]) {
    let encoded: Vec<(&str, String)> = attrs
        .iter()
        .map(|(k, v)| {
            (
                *k,
                serde_json::to_string(v).expect("a str always serialises"),
            )
        })
        .collect();
    let mut out = line("INFO", msg, &encoded, &jiff::Zoned::now());
    out.push('\n');
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
}

/// The level names the `level` label is queried by. tracing's TRACE is
/// filtered out before reaching here, but DEBUG is the honest name if it ever
/// is not.
fn level(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        _ => "DEBUG",
    }
}

/// RFC 3339 with milliseconds, in the local zone, "Z" for UTC. The zone comes
/// from TZ, so a pod that sets it logs local times.
fn timestamp(now: &jiff::Zoned) -> String {
    let base = now.strftime("%Y-%m-%dT%H:%M:%S%.3f").to_string();
    let offset = now.offset().seconds();
    if offset == 0 {
        return base + "Z";
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.abs();
    format!(
        "{base}{sign}{:02}:{:02}",
        offset / 3600,
        (offset % 3600) / 60
    )
}

fn push_json_str(out: &mut String, s: &str) {
    // serde_json cannot fail to serialise a str.
    out.push_str(&serde_json::to_string(s).expect("a str always serialises"));
}

/// The message, and every other field as already-encoded JSON in the order it
/// was written, so a line reads the way its call site does.
#[derive(Default)]
struct Fields {
    msg: String,
    attrs: Vec<(&'static str, String)>,
}

impl Fields {
    fn push(&mut self, field: &Field, json: String) {
        self.attrs.push((field.name(), json));
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.msg = value.to_string();
        } else {
            self.push(
                field,
                serde_json::to_string(value).expect("a str always serialises"),
            );
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // NaN and infinities are not JSON, so they are written as strings.
        let json = serde_json::Number::from_f64(value)
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("\"{value}\""));
        self.push(field, json);
    }

    /// An error is its message.
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_str(field, &value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut s = String::new();
        let _ = write!(s, "{value:?}");
        self.record_str(field, &s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// Captures what the layer would have written, with the clock fixed.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<String>>>);

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let now: jiff::Zoned = "2026-10-14T09:30:01.234567+01:00[Europe/London]"
                .parse()
                .unwrap();
            self.0.lock().unwrap().push(render(event, &now));
        }
    }

    fn capture(f: impl FnOnce()) -> Vec<String> {
        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry().with(cap.clone());
        tracing::subscriber::with_default(subscriber, f);
        cap.0.lock().unwrap().clone()
    }

    #[test]
    fn writes_one_json_line_per_event() {
        let lines = capture(|| tracing::info!(addr = ":8092", "listening"));
        assert_eq!(
            lines,
            [
                r#"{"time":"2026-10-14T09:30:01.234+01:00","level":"INFO","msg":"listening","addr":":8092"}"#
            ]
        );
    }

    #[test]
    fn errors_are_their_message() {
        let err = std::io::Error::other("broken pipe");
        let lines = capture(|| tracing::error!(error = %err, count = 3, "encode response"));
        assert!(
            lines[0].ends_with(
                r#""level":"ERROR","msg":"encode response","error":"broken pipe","count":3}"#
            ),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn carries_the_active_span_ids() {
        use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
        let sc = SpanContext::new(
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
            SpanId::from_hex("00f067aa0ba902b7").unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let cx = opentelemetry::Context::new().with_remote_span_context(sc);
        let _guard = cx.attach();
        let lines = capture(|| tracing::info!("hello"));
        assert!(
            lines[0].ends_with(
                r#""msg":"hello","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7"}"#
            ),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn no_span_means_no_ids() {
        let lines = capture(|| tracing::info!("hello"));
        assert!(!lines[0].contains("trace_id"), "{}", lines[0]);
    }

    #[test]
    fn utc_is_z() {
        let now: jiff::Zoned = "2026-01-02T03:04:05.006Z[UTC]".parse().unwrap();
        assert_eq!(timestamp(&now), "2026-01-02T03:04:05.006Z");
    }
}

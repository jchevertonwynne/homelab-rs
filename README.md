# homelab-rs

Observability and serving for the homelab's axum services. The Grafana
dashboards and alerts read these apps' metric series, log fields and span
names by name, and a renamed one is an empty panel rather than an error, so
every app emits them through here.

| | |
|---|---|
| `access` | the Cloudflare Access identity header, and the hostname's allowlist |
| `logging` | JSON lines on stdout, with the active span's `trace_id` and `span_id` |
| `trace` | OTLP/gRPC span export through tracing-opentelemetry, and a server span per request |
| `metrics` | `http_request_duration_seconds` and `http_requests_in_flight`, plus `tokio_*`, `jemalloc_*` and `process_*` |
| `http` | wires the middleware onto a router — plainly, or layered for an app behind an auth wrapper — and turns a handler's panic into a counted 500 |
| `serve` | an HTTP/1 server with timeouts, and shutdown on SIGINT/SIGTERM |
| `etag` | strong ETags over embedded assets, so a repeat request costs a 304 |
| `profiling` | a pprof heap profile on its own listener, for Alloy's pyroscope.scrape |

```toml
homelab = { git = "https://github.com/jchevertonwynne/homelab-rs", tag = "v0.5.0" }
```

```rust
let tracing = homelab::init("echo", &args.otel_endpoint)?;
homelab::profiling::spawn(args.pprof_addr);
let app = homelab::http::instrument(Router::new().route("/", get(handler)));
homelab::serve::run(args.addr, app).await?;
tracing.shutdown().await;
```

Arguments are each app's own: a `clap` parser with typed fields, `--addr
0.0.0.0:8092` and the like.

## What the dashboards read

**Metrics.** `http_request_duration_seconds` (a histogram with
`prometheus::DEFAULT_BUCKETS`) and `http_requests_in_flight`. The labels are
`method`, `route` and `status`, in that order; `status` holds the numeric code.
`route` is the method and axum's matched path — `GET /entries/{id}` — or
`unmatched`, so there is one series per route rather than one per row ever
created.

**Logs.** `time` (RFC 3339, milliseconds, local zone from `TZ`), `level`
(`INFO`, `WARN`, `ERROR`), `msg`, then the event's fields, then the trace ids.
Alloy makes `level` a Loki label and `trace_id` structured metadata, which is
what links a log line to its trace.

**Spans.** A server span per request, named for its route (`GET
/entries/{id}`) or its method alone when nothing matched, with the HTTP
semantic-convention attributes; an error only for a 5xx; an incoming
`traceparent` is continued. Beneath it, an app's own spans, which the
dashboard's operations row finds by name prefix (`db.`, `notes.`):

```rust
#[tracing::instrument(name = "db.query", skip_all, fields(db.system = "sqlite"), err)]
async fn query(&self) -> Result<Rows, Error> { … }
```

`name` is the span's name in Tempo, `skip_all` keeps arguments — user content —
out of it, and `err` marks the span an error and logs the error once, at
ERROR, under the span's name. `err` logs the error's `Display`, which for an
`anyhow::Error` is only its outermost context. Where the caller already logs
the error with its whole chain, leave `err` off and mark the span instead, so
the error is logged once:

```rust
use homelab::trace::{OpenTelemetrySpanExt as _, Status};
tracing::Span::current().set_status(Status::error(format!("{err:#}")));
```

## An app behind a wrapper

`http::instrument_layered(outer, app, excluded)` is for an app whose routes
sit behind an authentication layer, beside a few that do not (`/healthz`,
`/metrics`). A request is labelled, and its span named, by the route it
matched in whichever router served it — and the label is fixed before the
wrapper runs, so a request the wrapper refuses is still counted under its
route. Routes in `excluded` (long-lived streams) never touch the histogram or
the in-flight gauge.

## jemalloc and the heap profile

On Linux, with the default `jemalloc` feature, linking this crate makes
jemalloc the allocator. That is what the heap profile (`/debug/pprof/heap` and
`/debug/pprof/allocs`, live allocations as `inuse_space`) and the `jemalloc_*`
series read. It is C, so building for musl needs a C cross-compiler; the app
Dockerfiles use zig behind a small wrapper, and the wrapper's comments are the
place to read about why it looks the way it does. An app with no heap worth
profiling can leave the feature off (`default-features = false`) and stay free
of C.

Sampling is switched on before `main`, so the profile includes what an app
sets up at startup, and one allocation in every 64 KiB has its stack recorded,
enough to see a heap of a few MB. The first profile served leaves the binary's
symbol table live for good, parsed to name the frames — up to a MB or so, more
for a bigger binary — under `jemalloc_pprof::dump_pprof`. That is the
profiler's, not the app's.

Sampling is switched on only if jemalloc was built with a
real unwinder. A build that lost it would otherwise walk frame pointers, which
Rust code does not keep, and segfault on a sampled allocation. Without one the
app logs `"heap profiling"` at ERROR and runs without a heap profile.

## Running one in the cluster

The heap dump goes through a temp file, and a scratch image with a read-only
root has nowhere to put one: mount an `emptyDir` at `/tmp`. Without it the app
serves normally, every heap profile request fails, and the app says so at
startup. Annotate the pod `pyroscope.io/scrape: "true"` and
`pyroscope.io/port` with the profiling port, and Alloy scrapes the heap
profile from it.

## Why there is no CPU profile

v0.1 and v0.2 served one through pprof-rs, and it was most of these apps'
memory. pprof-rs preallocates a 16–22 MB sample table so its signal handler
never allocates, rebuilds it after every profile and keeps it; Alloy profiles
back to back, so it was always held, and the churn of replacing it every 15
seconds left jemalloc sitting on several times that again. echo, which runs
in 8 MB, settled at 50 MB, and list at 80. For apps with a handful of users a
flame graph was not worth that, so v0.3 dropped it. The Pi's kernel has no
BTF, which rules out profiling from outside with Alloy's eBPF profiler.

## What changed in v0.4

The crate no longer imitates the Go apps it replaced: no Go-style argument
parser (apps use `clap`), no ServeMux route spelling (`GET /` rather than `GET
/{$}`), no net/http 404/405 bodies or path-cleaning redirect, and real span
names for layered apps. `serve::run` and `profiling::spawn` take a
`SocketAddr`.

## What changed in v0.4.1

Heap sampling starts before `main` rather than when the router is built, and
records one allocation in every 64 KiB rather than every 512. Before, nothing
an app allocated at startup was ever in a profile, and most apps' profiles
were empty.

## What changed in v0.5

Spans are `tracing` spans, exported through tracing-opentelemetry, so an app
marks a function with `#[tracing::instrument]` where it used to wrap its body
in `trace::op`, which is gone. `homelab::init` starts logging and tracing
together, replacing `logging::init` and `trace::init`: the span layer has to
be in the subscriber from the start, because tracing-opentelemetry finds
itself by downcasting the subscriber, which a layer added later cannot allow.
A panicking handler is a 500, counted and logged, where it was a reset
connection. The span export queue holds 256 rather than the SDK's 2048, which
it allocated up front.

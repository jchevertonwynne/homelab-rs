# homelab-rs

Observability and serving for the homelab's axum services — the Rust
counterpart of [homelab-go](https://github.com/jchevertonwynne/homelab-go), held
to the same contract. An app moved from one to the other emits the same metric
series, log lines and spans, because the Grafana dashboards and alerts read
them by name and a renamed one is an empty panel rather than an error.

| | |
|---|---|
| `access` | the Cloudflare Access identity header, and the hostname's allowlist |
| `flags` | Go `flag`-style arguments (`-addr :8092`), so a Deployment's `args` carry over unchanged |
| `logging` | slog-shaped JSON on stdout, with the active span's `trace_id` and `span_id` |
| `trace` | OTLP/gRPC span export, an otelhttp-shaped server span per request, and `op` for child spans |
| `metrics` | `http_request_duration_seconds` and `http_requests_in_flight`, plus `tokio_*`, `jemalloc_*` and `process_*` |
| `http` | wires the middleware onto a router — plainly, or layered for an app behind an auth wrapper — with net/http's 404, 405 and path-cleaning redirect |
| `serve` | an HTTP/1 server with timeouts, and shutdown on SIGINT/SIGTERM |
| `etag` | strong ETags over embedded assets, with ServeContent's conditional and range handling |
| `profiling` | a pprof heap profile on its own listener, for Alloy's pyroscope.scrape |

```toml
homelab = { git = "https://github.com/jchevertonwynne/homelab-rs", tag = "v0.2.0" }
```

```rust
homelab::logging::init();
homelab::profiling::spawn(":6060".into());
let tracing = homelab::trace::init("echo", "alloy:4317")?;
let app = homelab::http::instrument(Router::new().route("/", get(handler)));
homelab::serve::run(":8092", app).await?;
tracing.shutdown().await;
```

## An app behind a wrapper

`http::instrument_layered(outer, app, excluded)` is for the Go shape
`mux.Handle("/", authenticate(app))` beside a few unwrapped routes
(`/healthz`, `/metrics`), instrumented with `metrics.WithRouters(app)` and
`WithExcludedRoutes`. Metrics label by the app's matched pattern even when the
wrapper refuses the request, and by `/` for anything no route matched; spans
are named from the outer router, so every app route is `GET /`, `POST /` —
which is what otelhttp recorded for those apps. Excluded routes (long-lived
streams) never touch the histogram or the in-flight gauge.

## The contract with homelab-go

**Route labels and span names use `http.ServeMux` spelling.** An axum route `/`
is `GET /{$}`, `/static/{*path}` is `/static/`, `/entries/{id}` is unchanged.
That keeps each endpoint's series continuous across a rewrite. `status` is a
string label holding the numeric code, labels stay in method/route/status
order, and the buckets are `prometheus.DefBuckets`.

**Log lines are slog's.** `time` (RFC 3339, milliseconds, local zone from
`TZ`), `level` (`INFO`, `WARN`, `ERROR`), `msg`, then the event's fields, then
the trace ids. Alloy makes `level` a label and `trace_id` structured metadata.

**Spans are otelhttp's.** Named `"GET /{$}"` once routed and `"GET"` when
nothing matched; a server span; an error only for a 5xx; an incoming
`traceparent` is continued.

**What has no equivalent.** Go's `go_*` runtime series and its goroutine,
mutex and block profiles — and, deliberately, its CPU profile (see below).
Alloy's requests for those profiles get a 404. The
`tokio_*` series are the nearest signal: `tokio_alive_tasks` for the goroutine
count, per-worker busy time for scheduler headroom.

## jemalloc and the heap profile

On Linux, with the default `jemalloc` feature, linking this crate makes
jemalloc the allocator. That is what the heap profile (`/debug/pprof/heap` and
`/debug/pprof/allocs`, live allocations as `inuse_space`) and the `jemalloc_*`
series read. It is C, so building for musl needs a C cross-compiler; the app
Dockerfiles use zig behind a small wrapper, and the wrapper's comments are the
place to read about why it looks the way it does.

Heap sampling starts off and is switched on only if jemalloc was built with a
real unwinder. A build that lost it would otherwise walk frame pointers, which
Rust code does not keep, and segfault on a sampled allocation. Without one the
app logs `"heap profiling"` at ERROR and runs without a heap profile.

## Running one in the cluster

The heap dump goes through a temp file, and a scratch image with a read-only
root has nowhere to put one: mount an `emptyDir` at `/tmp`. Without it the app
serves normally, every heap profile request fails, and the app says so at
startup.

## Why there is no CPU profile

v0.1 and v0.2 served one through pprof-rs, and it was most of these apps'
memory. pprof-rs preallocates a 16–22 MB sample table so its signal handler
never allocates, rebuilds it after every profile and keeps it; Alloy profiles
back to back, so it was always held, and the churn of replacing it every 15
seconds left jemalloc sitting on several times that again. echo, which runs
in 8 MB, settled at 50 MB, and list at 80. For apps with a handful of users a
flame graph was not worth that, so v0.3 dropped it. The Pi's kernel has no
BTF, which rules out profiling from outside with Alloy's eBPF profiler.

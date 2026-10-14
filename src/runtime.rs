//! The tokio runtime's own metrics: the nearest thing an async Rust process
//! has to `go_goroutines` and the Go scheduler's view of itself.
//!
//! Only the stable subset, so no app needs building with `tokio_unstable`.
//! `tokio_alive_tasks` is the goroutine count's counterpart — a line that only
//! ever climbs is a task leak, as a climbing goroutine count was. The
//! per-worker busy time is the share of each worker thread spent polling
//! tasks rather than parked: a worker pinned near one second per second is a
//! runtime with no headroom, which is what a handler doing blocking work on
//! the executor looks like from outside.
//!
//! Labelled by worker index, which is bounded by the worker count — the
//! number of CPUs unless the app says otherwise.

use std::sync::{LazyLock, Mutex, OnceLock};

use prometheus::{CounterVec, IntCounterVec, IntGauge, Opts};
use tokio::runtime::Handle;

static HANDLE: OnceLock<Handle> = OnceLock::new();

struct Series {
    workers: IntGauge,
    alive_tasks: IntGauge,
    global_queue_depth: IntGauge,
    busy_seconds: CounterVec,
    parks: IntCounterVec,
    /// What each counter last reported, per worker. tokio hands out running
    /// totals and a Prometheus counter only goes up by a delta, so the
    /// difference is what gets added.
    last: Mutex<Vec<(f64, u64)>>,
}

static SERIES: LazyLock<Series> = LazyLock::new(|| {
    let gauge = |name: &str, help: &str| {
        let g = IntGauge::with_opts(Opts::new(name, help)).expect("a valid gauge definition");
        prometheus::register(Box::new(g.clone())).expect("registered once");
        g
    };
    let busy = CounterVec::new(
        Opts::new(
            "tokio_worker_busy_seconds_total",
            "Time each runtime worker thread has spent polling tasks.",
        ),
        &["worker"],
    )
    .expect("a valid counter definition");
    prometheus::register(Box::new(busy.clone())).expect("registered once");
    let parks = IntCounterVec::new(
        Opts::new(
            "tokio_worker_parks_total",
            "Times each runtime worker thread has parked for want of work.",
        ),
        &["worker"],
    )
    .expect("a valid counter definition");
    prometheus::register(Box::new(parks.clone())).expect("registered once");
    Series {
        workers: gauge("tokio_workers", "Worker threads in the tokio runtime."),
        alive_tasks: gauge(
            "tokio_alive_tasks",
            "Tasks spawned on the runtime that have not yet finished.",
        ),
        global_queue_depth: gauge(
            "tokio_global_queue_depth",
            "Tasks waiting in the runtime's shared queue for a worker.",
        ),
        busy_seconds: busy,
        parks,
        last: Mutex::new(Vec::new()),
    }
});

/// Remembers the runtime this is called inside, so a scrape can read it. A
/// call outside one is a no-op, and the series simply never appear.
pub(crate) fn register() {
    if let Ok(handle) = Handle::try_current() {
        let _ = HANDLE.set(handle);
        LazyLock::force(&SERIES);
    }
}

pub(crate) fn refresh() {
    let Some(handle) = HANDLE.get() else { return };
    let m = handle.metrics();
    let s = &*SERIES;
    s.workers.set(m.num_workers() as i64);
    s.alive_tasks.set(m.num_alive_tasks() as i64);
    s.global_queue_depth.set(m.global_queue_depth() as i64);

    let mut last = s.last.lock().unwrap_or_else(|e| e.into_inner());
    last.resize(m.num_workers(), (0.0, 0));
    for (worker, (busy_was, parks_was)) in last.iter_mut().enumerate() {
        let label = worker.to_string();
        let busy = m.worker_total_busy_duration(worker).as_secs_f64();
        let parks = m.worker_park_count(worker);
        s.busy_seconds
            .with_label_values(&[&label])
            .inc_by((busy - *busy_was).max(0.0));
        s.parks
            .with_label_values(&[&label])
            .inc_by(parks.saturating_sub(*parks_was));
        *busy_was = busy;
        *parks_was = parks;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reports_the_runtime_it_was_registered_in() {
        register();
        let _task = tokio::spawn(std::future::pending::<()>());
        refresh();
        assert_eq!(SERIES.workers.get(), 2);
        assert!(SERIES.alive_tasks.get() >= 1);
        // Both workers exist as series even if one has never run anything.
        assert!(
            SERIES
                .busy_seconds
                .get_metric_with_label_values(&["1"])
                .is_ok()
        );
    }
}

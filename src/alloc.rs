//! jemalloc as the process allocator, with its heap profiler sampling, and its
//! own accounting exported as metrics.
//!
//! Linking this crate is enough: the `#[global_allocator]` below applies to
//! the whole binary. It exists for two signals — a heap profile, and how much
//! the heap holds against how much the process keeps resident — and because musl's own allocator, which a
//! static binary otherwise gets, is slow under contention.
//!
//! The sampling interval is 2^16 bytes (64 KiB): on average one allocation in
//! every 64 KiB allocated has its stack recorded. jemalloc's default is 512
//! KiB, which these apps' live heaps — a few MB — fill with a handful of
//! samples at most, often none: a profile with nothing in it. The cost of
//! sampling is a stack walk per sample, so it scales with traffic, and these
//! apps have little.
//!
//! The page size is fixed when jemalloc is compiled, not read at runtime. The
//! image builds set it to 4 KiB, the Pi's; see the Dockerfile.
//!
//! A heap profile reports only what was allocated while sampling was on, so it
//! is switched on before `main`, by a constructor that runs as the process
//! starts: switched on any later, everything an app sets up at startup — its
//! runtime, connection pool, templates and assets, most of what it holds — is
//! missing from the profile for as long as the process lives. Linking the
//! crate is still all an app does.
//!
//! jemalloc starts with sampling off, and the constructor switches it on only
//! once it has checked how this jemalloc was built to record a stack. With
//! `_Unwind_Backtrace` (libgcc's interface, which Rust's own libunwind
//! provides) it walks the binary's unwind tables, which is safe. Without it,
//! jemalloc falls back to walking frame pointers, which Rust code does not
//! keep, and the first sampled allocation in a deep enough stack segfaults the
//! process. An image whose build quietly lost the unwinder — which is exactly
//! what cross-compiling with zig did until its wrapper mapped -lgcc to
//! -lunwind — therefore gets an error line and no heap profile, rather than a
//! crash loop. Nothing can log before `main`, so the constructor keeps the
//! outcome and [`register`] logs a failure.
//!
//! The first profile served leaves about 1 MB permanently live: the binary's
//! symbol table, parsed to name the frames and kept for the next one. It shows
//! up in the profile under `jemalloc_pprof::dump_pprof` and in
//! `jemalloc_allocated_bytes`, and it is not the app's.

use std::ffi::c_char;
use std::sync::{LazyLock, OnceLock};

use prometheus::{IntGauge, Opts};
use tikv_jemalloc_ctl::{epoch, stats};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc reads its options from this symbol at startup, before `main`. The
/// `_rjem_` prefix is because tikv-jemallocator prefixes jemalloc's symbols
/// rather than replacing musl's malloc, which a static musl link would
/// otherwise define twice.
#[repr(transparent)]
pub struct MallocConf(*const c_char);

// A pointer to a static, NUL-terminated string that nothing ever writes.
unsafe impl Sync for MallocConf {}

#[used]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static MALLOC_CONF: MallocConf =
    MallocConf(c"prof:true,prof_active:false,lg_prof_sample:16".as_ptr());

/// How switching the sampler on went, decided before `main`.
static ACTIVATION: OnceLock<Result<(), String>> = OnceLock::new();

// SAFETY: runs before main, so it must not rely on anything main sets up. It
// uses jemalloc, which initialises itself on first use, and statics that
// initialise lazily; it does not log or touch a runtime.
#[ctor::ctor(unsafe)]
fn activate_before_main() {
    ACTIVATION.get_or_init(activate);
}

/// What jemalloc says it holds, refreshed at each scrape. `allocated` is what
/// the program asked for and still holds — the live heap — and
/// `resident` is what that costs in physical memory, which is the number the
/// container's memory limit is measured against.
struct Gauges {
    allocated: IntGauge,
    active: IntGauge,
    resident: IntGauge,
    mapped: IntGauge,
    retained: IntGauge,
    metadata: IntGauge,
}

static GAUGES: LazyLock<Gauges> = LazyLock::new(|| {
    let gauge = |name: &str, help: &str| {
        let g = IntGauge::with_opts(Opts::new(name, help)).expect("a valid gauge definition");
        prometheus::register(Box::new(g.clone())).expect("registered once");
        g
    };
    Gauges {
        allocated: gauge(
            "jemalloc_allocated_bytes",
            "Bytes allocated by the application and not yet freed.",
        ),
        active: gauge(
            "jemalloc_active_bytes",
            "Bytes in pages jemalloc has handed out to the application.",
        ),
        resident: gauge(
            "jemalloc_resident_bytes",
            "Bytes of physical memory jemalloc's pages occupy.",
        ),
        mapped: gauge(
            "jemalloc_mapped_bytes",
            "Bytes jemalloc has mapped into active extents.",
        ),
        retained: gauge(
            "jemalloc_retained_bytes",
            "Bytes of virtual memory jemalloc keeps mapped but returned to the OS.",
        ),
        metadata: gauge(
            "jemalloc_metadata_bytes",
            "Bytes jemalloc spends on its own bookkeeping.",
        ),
    }
});

pub(crate) fn register() {
    LazyLock::force(&GAUGES);
    static REPORT: std::sync::Once = std::sync::Once::new();
    REPORT.call_once(|| {
        // get_or_init in case the constructor never ran, which would leave a
        // late profile rather than none.
        if let Err(err) = ACTIVATION.get_or_init(activate) {
            tracing::error!(error = err, "heap profiling");
        }
    });
}

fn activate() -> Result<(), String> {
    if !safe_unwinder() {
        return Err("jemalloc was built without an unwinder (config.prof_libgcc and config.prof_libunwind are both false); leaving heap profiling off rather than risk a crash".into());
    }
    // Through jemalloc_pprof's handle rather than mallctl directly: it
    // remembers whether it activated the profiler, and refuses to dump
    // otherwise. try_lock because nothing else can hold it this early, and a
    // blocking lock would panic were this ever reached inside the runtime.
    jemalloc_pprof::PROF_CTL
        .as_ref()
        .ok_or_else(|| "jemalloc was not started with prof:true".to_string())
        .and_then(|ctl| ctl.try_lock().map_err(|e| e.to_string()))
        .and_then(|mut ctl| ctl.activate().map_err(|e| e.to_string()))
}

/// Whether this jemalloc records stacks through a real unwinder rather than
/// by walking frame pointers.
fn safe_unwinder() -> bool {
    let read = |name: &[u8]| {
        // SAFETY: the config.* entries are documented as readable bools.
        unsafe { tikv_jemalloc_ctl::raw::read::<bool>(name) }.unwrap_or(false)
    };
    read(b"config.prof_libgcc\0") || read(b"config.prof_libunwind\0")
}

/// jemalloc caches its statistics; advancing the epoch is what refreshes them.
pub(crate) fn refresh() {
    if epoch::advance().is_err() {
        return;
    }
    let g = &*GAUGES;
    let set = |gauge: &IntGauge, value: Result<usize, tikv_jemalloc_ctl::Error>| {
        if let Ok(v) = value {
            gauge.set(v as i64);
        }
    };
    set(&g.allocated, stats::allocated::read());
    set(&g.active, stats::active::read());
    set(&g.resident, stats::resident::read());
    set(&g.mapped, stats::mapped::read());
    set(&g.retained, stats::retained::read());
    set(&g.metadata, stats::metadata::read());
}

/// The heap profile in pprof's format, gzipped: the live sampled allocations
/// as `inuse_space`, which is the sample type Pyroscope files heap
/// profile under. Blocking — it writes a dump to the temp directory and reads
/// it back.
pub(crate) fn heap_profile() -> Result<Vec<u8>, String> {
    let ctl = jemalloc_pprof::PROF_CTL
        .as_ref()
        .ok_or("heap profiling is not available: jemalloc was not started with prof:true")?;
    let mut ctl = ctl.blocking_lock();
    if !ctl.activated() {
        return Err("heap profiling is not active".into());
    }
    ctl.dump_pprof().map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The build-configuration check, so a test run on the target catches an
    // image that would have crashed, rather than a pod doing it.
    #[test]
    fn jemalloc_records_stacks_with_an_unwinder() {
        assert!(
            safe_unwinder(),
            "jemalloc would walk frame pointers; see the module docs"
        );
    }

    #[test]
    fn jemalloc_is_the_allocator_and_profiling_is_on() {
        assert!(
            // SAFETY: opt.prof is documented as a readable bool.
            unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0") }.unwrap_or(false),
            "_rjem_malloc_conf was not picked up, so jemalloc started without prof:true"
        );
        register();
        // SAFETY: prof.active is documented as a readable bool.
        assert!(unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0") }.unwrap());
        let held: Vec<u8> = vec![7; 8 << 20];
        refresh();
        assert!(GAUGES.allocated.get() >= held.len() as i64);
        assert!(GAUGES.resident.get() > 0);
    }

    #[test]
    fn dumps_a_gzipped_heap_profile() {
        register();
        let _held: Vec<u8> = vec![7; 8 << 20];
        let body = heap_profile().expect("heap profile");
        assert_eq!(&body[..2], &[0x1f, 0x8b], "gzipped pprof");
    }
}

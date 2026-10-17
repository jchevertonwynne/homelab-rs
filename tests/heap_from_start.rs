//! The heap profile covers what a process allocates before it builds its
//! router. Sampling used to be switched on there, so everything an app set up
//! at startup was missing from every profile it served.
//!
//! A file of its own, so it is a process of its own: no other test can have
//! switched sampling on first. Nothing here calls into the crate, either —
//! linking it has to be enough, as it is for an app.
#![cfg(all(feature = "jemalloc", target_os = "linux"))]

use homelab as _;

#[test]
fn samples_what_was_allocated_before_anything_ran() {
    let held: Vec<u8> = vec![7; 8 << 20];
    let ctl = jemalloc_pprof::PROF_CTL
        .as_ref()
        .expect("jemalloc started with prof:true");
    let mut ctl = ctl.blocking_lock();
    assert!(ctl.activated(), "sampling was not switched on before main");
    // SAFETY: prof.active is documented as a readable bool.
    let active = unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0") };
    assert_eq!(active, Ok(true), "jemalloc itself says sampling is off");
    let profile = ctl.dump_profile().expect("heap dump");
    let sampled: f64 = profile.iter().map(|(stack, _)| stack.weight).sum();
    assert!(
        sampled >= held.len() as f64 / 2.0,
        "{sampled} bytes sampled, with {} live",
        held.len()
    );
}

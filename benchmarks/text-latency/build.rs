//! Builds `guests/scratchpad` for this benchmark -- once for the GATE build
//! (ordinary production guest, `world kernel`), and, only when this crate's
//! own `bench-probe` feature is on, a second time for the DIAGNOSTIC build
//! (the guest's own `bench-probe` feature, `world kernel-bench`). See
//! `docs/adr/0001-userland-text-authority.md` for why the guest itself is
//! never forked for this: both artifacts come from the same source tree.

fn main() {
    // GATE artifact: always built, never with extra features. This is the
    // one whose numbers are checked against the p95 <= 5 ms target.
    instar_guest_build::build_guests(&[("scratchpad", "instar-scratchpad", "SCRATCHPAD_WASM")]);

    // DIAGNOSTIC artifact: only when this crate is itself built with
    // `--features bench-probe` (cargo sets CARGO_FEATURE_BENCH_PROBE for
    // build scripts in that case). Its own numbers are never the gate --
    // see benchmarks/text-latency/README.md.
    if std::env::var_os("CARGO_FEATURE_BENCH_PROBE").is_some() {
        instar_guest_build::build_guests_with_features(&[(
            "scratchpad",
            "instar-scratchpad",
            "SCRATCHPAD_BENCH_WASM",
            &["bench-probe"],
        )]);
    }
}

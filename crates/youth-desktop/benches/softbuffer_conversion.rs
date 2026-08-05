//! Benchmarks the presentation-boundary RGBA8 → 0x00RRGGBB conversion
//! (`youth_desktop::softbuffer_bridge::convert_rgba8_to_rgbx32`) at the
//! physical frame sizes the desktop presentation targets, plus the
//! correctness-path rejection cases.
//!
//! Reporting layout: criterion 0.5.1 ships only `Bytes`/`BytesDecimal`/
//! `Elements` throughput (no `ElementsAndBytes`), so pixel-count and
//! byte-traffic views are separate groups, each measured with whatever that
//! variant actually processes per iteration. The source/traffic calculation
//! behind each group's throughput is stated in the doc comment above the
//! group -- no invented units.
//!
//! Every buffer is allocated once outside the timed closure and reused, so
//! no benchmarked iteration allocates.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use youth_desktop::softbuffer_bridge::convert_rgba8_to_rgbx32;
use youth_paint::PhysicalSize;

/// Physical frame sizes in pixels: 4K UHD, Full HD, and two common
/// laptop/tablet window sizes.
const SIZES: [(u32, u32); 4] = [(640, 480), (1024, 720), (1920, 1080), (3840, 2160)];

/// Packs one source buffer into one destination buffer and black-boxes the
/// result so the compiler cannot elide the conversion. Never asserted inside
/// the timed loop -- the conversion is pure and deterministic.
fn convert_once(src: &[u8], dst: &mut [u32], size: PhysicalSize) {
    let _ = black_box(convert_rgba8_to_rgbx32(
        black_box(src),
        black_box(dst),
        size,
    ));
}

fn pixels_of((width, height): (u32, u32)) -> u64 {
    u64::from(width) * u64::from(height)
}

/// Which per-iteration traffic metric a convert group reports: the number of
/// pixels scanned, or the RGBA8 source bytes read (four bytes per packed
/// destination word).
#[derive(Clone, Copy)]
enum ConvertMetric {
    Pixels,
    Traffic,
}

/// The accepted-path conversion, reported once per pixel and once per RGBA
/// source byte (`convert/pixels` and `convert/traffic` groups respectively).
fn convert_benchmarks(c: &mut Criterion) {
    bench_convert_group(c, "convert/pixels", ConvertMetric::Pixels);
    bench_convert_group(c, "convert/traffic", ConvertMetric::Traffic);
}

/// Populates one convert group across the four target sizes with both
/// accepted-path call shapes.
fn bench_convert_group(c: &mut Criterion, group_name: &str, metric: ConvertMetric) {
    let mut group = c.benchmark_group(group_name);
    for &(width, height) in &SIZES {
        let size = PhysicalSize { width, height };
        let pixels = pixels_of((width, height));
        // A fully-opaque source: the conversion accepts it, so every
        // conversion benchmark measures the accepted hot path.
        let src = vec![255u8; (pixels * 4) as usize];

        match metric {
            ConvertMetric::Pixels => {
                group.throughput(Throughput::Elements(pixels));
            }
            ConvertMetric::Traffic => {
                group.throughput(Throughput::BytesDecimal(pixels * 4));
            }
        }
        let id = format!("{width}x{height}");

        // (1) Reused destination `Vec<u32>`: the caller keeps one long-lived
        //     allocation and converts into it every frame.
        let mut reused_dst = vec![0u32; pixels as usize];
        group.bench_with_input(BenchmarkId::new("reused_vec", &id), &size, |b, &size| {
            b.iter(|| convert_once(&src, &mut reused_dst, size));
        });

        // (2) Acquired-like direct `&mut [u32]`: a persistent slice standing
        //     in for the buffer softbuffer returns from
        //     `surface.buffer_mut()` -- the exact call the opt-in Vello path
        //     in native.rs makes. The same conversion as (1); kept as a
        //     separately documented call site because this direct-buffer
        //     shape is the one that removes the avoidable copy.
        let mut acquired = vec![0u32; pixels as usize];
        let acquired_dst: &mut [u32] = &mut acquired;
        group.bench_with_input(
            BenchmarkId::new("acquired_slice", &id),
            &size,
            |b, &size| {
                b.iter(|| convert_once(&src, acquired_dst, size));
            },
        );
    }
    group.finish();
}

/// Conversion plus the extra `copy_from_slice` the legacy comparison path
/// pays: the RGBA source is first packed into an intermediate `Vec<u32>`,
/// then memcpy'd into the acquired-like buffer. Traffic per iteration is the
/// RGBA source bytes read (4/pixel) plus the packed words copied (4/pixel),
/// i.e. 8 bytes per pixel -- this variant exists to expose that copy as
/// avoidable in the direct-buffer path.
fn convert_plus_copy_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("convert_plus_copy/traffic");
    for &(width, height) in &SIZES {
        let size = PhysicalSize { width, height };
        let pixels = pixels_of((width, height));
        group.throughput(Throughput::BytesDecimal(pixels * 8));
        let src = vec![255u8; (pixels * 4) as usize];
        // `scratch` is the intermediate `Vec<u32>` the conversion lands in
        // (which the direct path does not need); `surface_like` stands in for
        // the acquired softbuffer buffer receiving the copy.
        let mut scratch = vec![0u32; pixels as usize];
        let mut surface_like = vec![0u32; pixels as usize];
        let id = format!("{width}x{height}");
        group.bench_with_input(
            BenchmarkId::new("convert_plus_copy", &id),
            &size,
            |b, &size| {
                b.iter(|| {
                    convert_once(&src, &mut scratch, size);
                    black_box(&mut surface_like).copy_from_slice(black_box(&scratch));
                    // Force the copied buffer to stay observable so the memcpy is
                    // never dead-stored away.
                    black_box(surface_like.as_mut_slice());
                });
            },
        );
    }
    group.finish();
}

/// Correctness-path rejection cases, benchmarked so the failure scan stays
/// cheap -- these are not hot-path targets. Throughput is per pixel scanned.
fn reject_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("reject/pixels");
    let size = PhysicalSize {
        width: 640,
        height: 480,
    };
    let pixels = 640_u64 * 480;
    group.throughput(Throughput::Elements(pixels));
    let word_len = pixels as usize;

    // Rejection at the very first pixel: alpha 0 in the first pixel, so the
    // conversion fails immediately after the length checks, before writing
    // anything into the destination.
    let mut src_first = vec![255u8; word_len * 4];
    src_first[3] = 0;
    let mut dst = vec![0u32; word_len];
    group.bench_function(BenchmarkId::new("reject_first_pixel", "640x480"), |b| {
        b.iter(|| convert_once(&src_first, &mut dst, size));
    });

    // Rejection at the last pixel: the conversion walks the entire buffer
    // (partially modifying the destination along the way, as documented on
    // the bridge function) before failing. The caller must never present
    // after any bridge error.
    let mut src_last = vec![255u8; word_len * 4];
    src_last[word_len * 4 - 1] = 0;
    group.bench_function(BenchmarkId::new("reject_last_pixel", "640x480"), |b| {
        b.iter(|| convert_once(&src_last, &mut dst, size));
    });
    group.finish();
}

criterion_group!(
    softbuffer_conversion,
    convert_benchmarks,
    convert_plus_copy_benchmarks,
    reject_benchmarks
);
criterion_main!(softbuffer_conversion);

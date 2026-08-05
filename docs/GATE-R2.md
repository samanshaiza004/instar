# Gate R2 — softbuffer conversion, benchmark, and resize/scale coverage

Date: 2026-08-05

Scope of R2 (on top of R1's uncommitted `youth-render-vello-cpu` worktree):

1. **`youth_paint::rgba8_to_rgbx32`** — an allocation-free conversion from a
   caller-owned premultiplied RGBA8 buffer (`[r, g, b, a]` row-major) to
   softbuffer's packed numeric `0x00RRGGBB` u32 output. It lives in
   `youth-paint` because it is renderer-neutral (no Vello types involved);
   the packing is shift-based and host-endianness independent; destination
   length is validated exactly, and a size whose buffer cannot be
   represented is rejected via `PaintError::SizeExceedsBackendLimit` before
   any length check (the byte length is computed in u64 and narrowed with
   `usize::try_from`, so 32-bit platforms never get a lossy cast).
2. **Alpha policy** — softbuffer carries no alpha and the Youth scene
   contract clears every frame opaque, so a premultiplied pixel with
   alpha != 255 is **rejected** with `PaintError::NonOpaquePixel`
   (`index`, `alpha`) rather than silently dropped. Opaque premultiplied
   pixels (alpha 255) equal their straight RGB value, so no
   un-premultiplication occurs. Unit tests cover channel ordering, row-major
   scanline order, source/destination length mismatch in both directions,
   and translucent/partial premultiplied bytes (fully transparent black,
   half-alpha red, and a translucent pixel past an opaque one).
3. **Criterion benchmark** — `softbuffer_conversion` bench target in
   `youth-render-vello-cpu` (criterion added as a dev-dependency only,
   `harness = false`). It benchmarks only the conversion hot loop with a
   reused destination buffer at exactly 640x480, 1024x720, 1920x1080, and
   3840x2160, with throughput reported per source byte and all setup
   outside the timed closure.
4. **Resize/scale coverage** — a new backend test derives physical sizes
   from a 640x360 logical fixture at scale factors exactly 1.0, 1.25, 1.5,
   and 2.0 with deterministic rounding, renders an opaque scene at every
   size into the same caller-owned target/backend, asserts target
   dimensions and representative pixels, and transitions 1.0 -> 2.0 -> 1.0
   so context recreation is exercised in both directions. The backend never
   receives a scale factor; the test feeds the physical sizes the desktop
   producer derives from the window. The existing resize test is kept.

## What this note measures

- The conversion benchmark is **code**: a compiled, listed criterion bench
  target. Provisional local-machine evidence from a short run (20 samples,
  ~1 s warm-up, 2 s measurement, macOS arm64, optimized bench profile):

  | Frame | Median time | Source throughput |
  | --- | ---: | ---: |
  | 640x480 | 548 µs | ~2.09 GiB/s |
  | 1024x720 | 1.48 ms | ~1.85 GiB/s |
  | 1920x1080 | 2.95 ms | ~2.61 GiB/s |
  | 3840x2160 | 10.9 ms | ~2.83 GiB/s |

  Command: `cargo bench -p youth-render-vello-cpu --bench softbuffer_conversion`

  These are single-host provisional numbers with a deliberately small sample
  size; no threshold is claimed from them. A full measurement pass belongs
  in `metrics/`.

## What this note does not measure

- **Package size** and **cold start** are measurement tasks, not code. No
  startup-time or package-size claims are made in R2; they remain open
  until a real measurement pass on the target platforms produces numbers.
- The conversion is benchmarked in isolation. End-to-end present latency
  (render + conversion + surface present) is not measured here, and
  `native.rs::present` (including its `copy_from_slice`) is intentionally
  unchanged in R2.

## R3 note (2026-08-05)

The converter and its benchmark moved to the presentation boundary in Gate R3
(see `docs/GATE-R3.md`): `rgba8_to_rgbx32` is now
`youth_desktop::softbuffer_bridge::convert_rgba8_to_rgbx32` with a
presentation-specific `SoftbufferError`, the criterion benchmark now lives at
`crates/youth-desktop/benches/softbuffer_conversion.rs`, and `youth-paint`
contains no softbuffer/presentation-format knowledge. The alpha policy,
32-bit-safe oversized-size handling, and unit-test coverage described above
moved with it (plus a new partial-destination-modification test), and a
selectable `YOUTH_RENDER_BACKEND=vello_cpu` path in `native.rs` converts
directly into the acquired softbuffer buffer with no intermediate copy.

//! What does one keystroke cost in a large document?
//!
//! ```text
//! cargo run --release --example textbench
//! ```
//!
//! # The question
//!
//! Package A exists to answer whether the host can keep a local editing
//! replica that stays fast as a document grows. A rope makes that *possible*;
//! it does not make it automatic, and nothing in the unit suite would notice a
//! `to_string()` creeping onto the editing path. This is the measurement that
//! would.
//!
//! # What is deliberately not measured
//!
//! No OS input, no IME, no shaping, no pixels. Package A has no native text
//! input path, so an `OS input -> pixels` number would be a number about
//! machinery that does not exist yet. What is measured is exactly what A owns:
//!
//! ```text
//! synthetic host edit  ->  TextBuffer
//! TextBuffer edit      ->  every attached TextView transformed
//! ```
//!
//! # Two instruments, because neither is sufficient
//!
//! [`instar_text::instrument`] counts contiguous copies made *through this
//! crate's API* — it says who asked for the whole document. It cannot see a
//! copy built inside `storage.rs` from `crop`'s chunks directly.
//!
//! The counting allocator below sees every allocation the process makes,
//! including ones this crate has no name for, but cannot attribute them. It
//! also cannot see bytes a B-tree moves *within* an allocation it already
//! owns, so it is a lower bound on copying rather than a total. Reporting it
//! as "bytes copied" would be a claim this program cannot support; it is
//! reported as what it is, bytes allocated.
//!
//! Together they are decisive. The implementation this benchmark exists to
//! reject —
//!
//! ```text
//! rope -> contiguous String -> edit the String -> rebuild the rope
//! ```
//!
//! — is caught by the counter if it goes through `materialize`, and by the
//! allocator if it does not. Under it, the allocated-per-edit column tracks
//! document size, and the 1 MiB and 10 MiB latency columns stop resembling
//! each other.
//!
//! # What a healthy result looks like
//!
//! The claim is about shape, not a magic number. A B-tree gaining a level is
//! allowed to cost something, so 18 µs at 1 MiB and 25 µs at 10 MiB is
//! healthy. What is not healthy is growth that tracks document bytes.
//!
//! ```text
//! no O(document-size) latency on a small local edit
//! no whole-buffer materialization anywhere on the editing path
//! undo cost proportional to changed material, not to the document
//! each extra view costs view-sized memory, not document-sized
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use instar_text::{
    Selection, TextBufferId, TextEdit, TextSystem, TextViewId, Viewport, instrument,
};

// ---------------------------------------------------------------- allocator

/// Counts what the process allocates, so a contiguous copy cannot hide.
///
/// Deliberately a lower bound on copying: bytes a B-tree moves inside an
/// allocation it already holds are invisible here. The number is honest about
/// being allocation rather than movement.
struct Counting;

static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            ALLOCATED.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        } else {
            FREED.fetch_add((layout.size() - new_size) as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocated() -> u64 {
    ALLOCATED.load(Ordering::Relaxed)
}

/// Bytes allocated and not yet freed.
fn live() -> i64 {
    ALLOCATED.load(Ordering::Relaxed) as i64 - FREED.load(Ordering::Relaxed) as i64
}

// ----------------------------------------------------------------- fixtures

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// A document of ordinary 64-byte lines.
fn ordinary(bytes: usize) -> String {
    let line = "the quick brown fox jumps over the lazy dog, and again, twice\n";
    let mut text = String::with_capacity(bytes + line.len());
    while text.len() < bytes {
        text.push_str(line);
    }
    text
}

/// One line, several megabytes long.
///
/// Not an exotic case to be filed under "hostile inputs": a minified bundle or
/// a one-line JSON log is exactly this, and it is what finds a line index that
/// quietly assumed lines are short.
fn single_line(bytes: usize) -> String {
    "x".repeat(bytes)
}

struct Document {
    name: &'static str,
    text: String,
}

fn documents() -> Vec<Document> {
    vec![
        Document {
            name: "1 MiB",
            text: ordinary(MIB),
        },
        Document {
            name: "10 MiB",
            text: ordinary(10 * MIB),
        },
        Document {
            name: "5 MiB single line",
            text: single_line(5 * MIB),
        },
    ]
}

// --------------------------------------------------------------- operations

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    InsertStart,
    InsertMiddle,
    InsertEnd,
    DeleteMiddle,
    ReplaceSelection,
    Paste,
    Undo,
    Redo,
}

const OPS: [Op; 8] = [
    Op::InsertStart,
    Op::InsertMiddle,
    Op::InsertEnd,
    Op::DeleteMiddle,
    Op::ReplaceSelection,
    Op::Paste,
    Op::Undo,
    Op::Redo,
];

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::InsertStart => "insert start",
            Op::InsertMiddle => "insert middle",
            Op::InsertEnd => "insert end",
            Op::DeleteMiddle => "delete middle",
            Op::ReplaceSelection => "replace selection",
            Op::Paste => "paste 100 KiB",
            Op::Undo => "undo",
            Op::Redo => "redo",
        }
    }

    /// Enough samples for a distribution rather than a stopwatch reading.
    ///
    /// Paste gets fewer because each one moves 100 KiB, and thousands of them
    /// would be measuring a document the fixture never described.
    fn iterations(self) -> usize {
        match self {
            Op::Paste => 200,
            Op::Undo | Op::Redo => 2_000,
            _ => 5_000,
        }
    }
}

/// The selection a "replace selection" operation stands in for: 100 bytes
/// replaced by 10, mid-document.
const SELECTION_BYTES: usize = 100;
const REPLACEMENT: &str = "0123456789";
const PASTE_BYTES: usize = 100 * KIB;

// ------------------------------------------------------------------- timing

struct Latency {
    p50: Duration,
    p95: Duration,
    p99: Duration,
}

fn percentiles(mut samples: Vec<Duration>) -> Latency {
    assert!(!samples.is_empty(), "a latency table needs samples");
    samples.sort_unstable();
    let at = |q: f64| {
        let index = ((samples.len() - 1) as f64 * q).round() as usize;
        samples[index]
    };
    Latency {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
    }
}

fn duration(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 10_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 10_000_000 {
        format!("{:.0}µs", ns as f64 / 1_000.0)
    } else {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    }
}

fn bytes(n: u64) -> String {
    if n < 10 * KIB as u64 {
        format!("{n} B")
    } else if n < 10 * MIB as u64 {
        format!("{:.0} KiB", n as f64 / KIB as f64)
    } else {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    }
}

// ----------------------------------------------------------------- the runs

/// One system with one view over one document.
fn open(text: &str) -> (TextSystem, TextBufferId, TextViewId) {
    let mut system = TextSystem::new();
    let buffer = system
        .open_buffer(text)
        .expect("a fixture is well within the buffer bound");
    let view = system
        .open_view(buffer)
        .expect("a fixture is well within the view bound");
    (system, buffer, view)
}

fn len_of(system: &TextSystem, buffer: TextBufferId) -> usize {
    system.buffer(buffer).expect("a live buffer").len_bytes()
}

/// Material an operation needs, allocated once.
///
/// Built before any measurement starts, because a 100 KiB paste buffer built
/// inside the measured region would show up as 100 KiB the *editor* allocated.
/// The same applies to reading back the bytes a replacement will overwrite:
/// that read is a `materialize` call, and counting the benchmark's own
/// bookkeeping against the editor would make the headline counter meaningless.
struct Prepared {
    paste: String,
    selection: String,
    middle: usize,
}

fn prepare(system: &TextSystem, buffer: TextBufferId, op: Op) -> Prepared {
    let middle = len_of(system, buffer) / 2;
    let selection = if op == Op::ReplaceSelection {
        system
            .buffer(buffer)
            .expect("a live buffer")
            .slice(middle..middle + SELECTION_BYTES)
            .expect("in bounds")
            .materialize()
    } else {
        String::new()
    };
    let paste = if op == Op::Paste {
        "p".repeat(PASTE_BYTES)
    } else {
        String::new()
    };
    Prepared {
        paste,
        selection,
        middle,
    }
}

/// The measured operation, and nothing else.
fn apply(
    system: &mut TextSystem,
    buffer: TextBufferId,
    view: TextViewId,
    op: Op,
    prepared: &Prepared,
) -> Duration {
    let len = len_of(system, buffer);
    let middle = prepared.middle;

    match op {
        Op::InsertStart => timed(system, view, TextEdit::insert(0, "x")),
        Op::InsertMiddle => timed(system, view, TextEdit::insert(middle, "x")),
        Op::InsertEnd => timed(system, view, TextEdit::insert(len, "x")),
        Op::DeleteMiddle => timed(system, view, TextEdit::delete(middle..middle + 1)),
        Op::ReplaceSelection => timed(
            system,
            view,
            TextEdit::replace(middle..middle + SELECTION_BYTES, REPLACEMENT),
        ),
        Op::Paste => timed(
            system,
            view,
            TextEdit::insert(middle, prepared.paste.as_str()),
        ),
        Op::Undo => {
            let start = Instant::now();
            system.undo(view).expect("something to undo");
            start.elapsed()
        }
        Op::Redo => {
            let start = Instant::now();
            system.redo(view).expect("something to redo");
            start.elapsed()
        }
    }
}

/// Puts the document back to the size the fixture names, outside every
/// measurement, so each iteration edits a document of the stated size.
///
/// Only the two operations that move enough material to matter are restored.
/// A one-byte insertion repeated five thousand times moves a 1 MiB fixture by
/// five thousand bytes, which is not a different document.
fn restore(system: &mut TextSystem, view: TextViewId, op: Op, prepared: &Prepared) {
    let middle = prepared.middle;
    match op {
        Op::ReplaceSelection => {
            system
                .apply_edit(
                    view,
                    TextEdit::replace(
                        middle..middle + REPLACEMENT.len(),
                        prepared.selection.as_str(),
                    ),
                )
                .expect("restoring the replaced selection");
        }
        Op::Paste => {
            system
                .apply_edit(view, TextEdit::delete(middle..middle + PASTE_BYTES))
                .expect("removing the paste");
        }
        _ => {}
    }
}

fn timed(system: &mut TextSystem, view: TextViewId, edit: TextEdit) -> Duration {
    let start = Instant::now();
    system.apply_edit(view, edit).expect("a well-formed edit");
    start.elapsed()
}

/// Banks the history undo and redo need, without timing it.
fn prepare_history(system: &mut TextSystem, buffer: TextBufferId, view: TextViewId, op: Op) {
    if !matches!(op, Op::Undo | Op::Redo) {
        return;
    }
    let depth = op.iterations() + 1;
    for _ in 0..depth {
        let middle = len_of(system, buffer) / 2;
        system
            .apply_edit(view, TextEdit::insert(middle, "abcdefgh"))
            .expect("banking history");
    }
    if op == Op::Redo {
        for _ in 0..depth {
            system.undo(view).expect("banking a redo branch");
        }
    }
}

fn latency_of(document: &Document, op: Op) -> Latency {
    let (mut system, buffer, view) = open(&document.text);
    let prepared = prepare(&system, buffer, op);
    prepare_history(&mut system, buffer, view, op);

    let iterations = op.iterations();
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        samples.push(apply(&mut system, buffer, view, op, &prepared));
        restore(&mut system, view, op, &prepared);
    }
    percentiles(samples)
}

/// What exactly one operation costs, as integers rather than an average.
struct Accounting {
    payload: u64,
    undo_retained: u64,
    materializations: u64,
    materialized_bytes: u64,
    whole_buffer: u64,
    allocated: u64,
}

fn accounting_of(document: &Document, op: Op) -> Accounting {
    let (mut system, buffer, view) = open(&document.text);
    let prepared = prepare(&system, buffer, op);
    // Undo and redo need exactly one banked step, not a benchmark's worth.
    if matches!(op, Op::Undo | Op::Redo) {
        system
            .apply_edit(view, TextEdit::insert(prepared.middle, "abcdefgh"))
            .expect("one banked edit");
        if op == Op::Redo {
            system.undo(view).expect("one banked undo");
        }
    }

    let retained_before = system
        .buffer(buffer)
        .expect("a live buffer")
        .journal()
        .retained_bytes() as u64;
    instrument::reset();
    let allocated_before = allocated();

    // The same code path the latency table times, measured once. The duration
    // is discarded: one sample is not a latency claim. No restore runs, so
    // nothing after the operation lands in these numbers.
    let _ = apply(&mut system, buffer, view, op, &prepared);

    let allocated = allocated() - allocated_before;
    let copying = instrument::snapshot();
    let retained_after = system
        .buffer(buffer)
        .expect("a live buffer")
        .journal()
        .retained_bytes() as u64;

    Accounting {
        payload: payload_of(op),
        undo_retained: retained_after.saturating_sub(retained_before),
        materializations: copying.materializations,
        materialized_bytes: copying.materialized_bytes,
        whole_buffer: copying.whole_buffer_materializations,
        allocated,
    }
}

/// The bytes the operation itself carries, as a baseline the other columns are
/// read against.
fn payload_of(op: Op) -> u64 {
    match op {
        Op::InsertStart | Op::InsertMiddle | Op::InsertEnd => 1,
        Op::DeleteMiddle => 0,
        Op::ReplaceSelection => REPLACEMENT.len() as u64,
        Op::Paste => PASTE_BYTES as u64,
        Op::Undo | Op::Redo => 8,
    }
}

// ------------------------------------------------------------- views, memory

/// What a second view of the same buffer costs.
fn views_table() {
    println!("\n=== views of one buffer ===\n");
    println!(
        "{:<10}  {:>12}  {:>12}  {:>16}",
        "views", "insert p50", "marginal", "live bytes/view"
    );

    let text = ordinary(10 * MIB);
    let mut baseline = Duration::ZERO;
    for count in [1usize, 2, 8] {
        let (mut system, buffer, first) = open(&text);

        let before = live();
        let mut views = vec![first];
        for _ in 1..count {
            views.push(system.open_view(buffer).expect("within the view bound"));
        }
        let per_view = if count > 1 {
            (live() - before) / (count as i64 - 1)
        } else {
            0
        };

        // Every view sits at a different place, so a transform that quietly
        // did nothing would still have to move each of them.
        let len = len_of(&system, buffer);
        for (index, view) in views.iter().enumerate() {
            let head = len / 2 + index;
            system
                .view_mut(*view)
                .expect("a live view")
                .set_selection(Selection { anchor: head, head });
        }

        let mut samples = Vec::with_capacity(2_000);
        for _ in 0..2_000 {
            let middle = len_of(&system, buffer) / 2;
            samples.push(timed(&mut system, first, TextEdit::insert(middle, "x")));
        }
        let latency = percentiles(samples);
        if count == 1 {
            baseline = latency.p50;
        }
        // Per *extra* view, so the column reads as the cost of attaching one
        // more rather than the cost of having any.
        let marginal = if count > 1 {
            latency.p50.saturating_sub(baseline) / (count as u32 - 1)
        } else {
            Duration::ZERO
        };

        println!(
            "{:<10}  {:>12}  {:>12}  {:>16}",
            count,
            duration(latency.p50),
            duration(marginal),
            if count > 1 {
                per_view.to_string()
            } else {
                "-".to_string()
            }
        );
    }
    println!(
        "\nA view is a caret, a selection, a wrap width and a scroll offset. If the\n\
         per-view number ever resembles the document, a view has started holding\n\
         a copy of it."
    );
}

/// How much of a document a view has to shape to draw one screen of it.
///
/// The window only, not the shaping: package B1 has no font stack wired to a
/// `TextView` yet, so `glyphs lowered` and `frame time` are not here. Which
/// bytes get shaped is the architectural claim, and it is decided entirely by
/// this table — a window that tracks the document rather than the viewport
/// cannot be rescued by a fast shaper.
fn viewport_table() {
    println!("\n=== viewport-bounded shaping window ===\n");
    println!(
        "{:<20}  {:<10}  {:>10}  {:>6}  {:>14}  {:>10}  {:>12}",
        "document", "position", "first row", "rows", "bytes shaped", "truncated", "window p50"
    );

    // 400 logical pixels of 20-pixel rows: about a screen, and around 1.5 KiB
    // of the 62-byte-line fixtures.
    let viewport = Viewport::new(400.0, 20.0);

    for document in documents() {
        let (system, buffer, _) = open(&document.text);
        let storage = system.buffer(buffer).expect("a live buffer").text();

        // Proportional rather than a fixed pixel offset: 1.9 million pixels is
        // past the end of the 1 MiB document, and a clamped empty window would
        // report a zero that looks like the claim being proved when it is only
        // the fixture running out.
        let deep = (storage.len_lines() as f32 * 0.8 * viewport.row_height) as i32;

        for (label, scroll) in [("top", 0i32), ("80% down", deep)] {
            let window = viewport.visible(storage, scroll).expect("in bounds");

            let mut samples = Vec::with_capacity(2_000);
            for _ in 0..2_000 {
                let start = Instant::now();
                let built = viewport.visible(storage, scroll).expect("in bounds");
                samples.push(start.elapsed());
                std::hint::black_box(built.bytes_shaped());
            }

            println!(
                "{:<20}  {:<10}  {:>10}  {:>6}  {:>14}  {:>10}  {:>12}",
                document.name,
                label,
                window.rows.start,
                window.rows.len(),
                bytes(window.bytes_shaped() as u64),
                if window.any_truncated() { "yes" } else { "no" },
                duration(percentiles(samples).p50),
            );
        }
    }
    println!(
        "\nA tenfold document costs the same window, and row 135,298 costs what row\n\
         13,528 costs. The single line is capped at MAX_SHAPED_PARAGRAPH_BYTES and\n\
         says so, because five megabytes of one paragraph is the case that would\n\
         otherwise look correct on every other fixture."
    );
}

fn memory_table() {
    println!("\n=== memory ===\n");
    println!(
        "{:<20}  {:>12}  {:>14}  {:>16}",
        "document", "text bytes", "buffer live", "journal after 1k"
    );

    for document in documents() {
        let text_bytes = document.text.len() as u64;

        let before = live();
        let (mut system, buffer, view) = open(&document.text);
        let buffer_live = live() - before;

        let journal_before = live();
        for _ in 0..1_000 {
            let middle = len_of(&system, buffer) / 2;
            system
                .apply_edit(view, TextEdit::insert(middle, "x"))
                .expect("a well-formed edit");
        }
        let journal_live = live() - journal_before;

        println!(
            "{:<20}  {:>12}  {:>14}  {:>16}",
            document.name,
            bytes(text_bytes),
            bytes(buffer_live.max(0) as u64),
            bytes(journal_live.max(0) as u64),
        );
    }
    println!(
        "\n`buffer live` includes the rope's own structure, so it exceeds the text.\n\
         `journal after 1k` is a thousand one-byte insertions: undo should cost\n\
         what was typed, not what it was typed into."
    );
}

// --------------------------------------------------------------------- main

fn main() {
    let documents = documents();

    println!("=== latency: p50 / p95 / p99 ===\n");
    print!("{:<20}", "operation");
    for document in &documents {
        print!("{:>26}", document.name);
    }
    println!();

    for op in OPS {
        print!("{:<20}", op.name());
        for document in &documents {
            let latency = latency_of(document, op);
            print!(
                "{:>26}",
                format!(
                    "{} / {} / {}",
                    duration(latency.p50),
                    duration(latency.p95),
                    duration(latency.p99)
                )
            );
        }
        println!();
    }

    println!("\n=== copying, for exactly one operation ===\n");
    println!(
        "{:<20}  {:<20}  {:>8}  {:>10}  {:>8}  {:>10}  {:>13}  {:>12}",
        "document",
        "operation",
        "payload",
        "undo-kept",
        "materlz",
        "mat bytes",
        "whole-buffer",
        "allocated"
    );
    let mut whole_buffer_total = 0u64;
    for document in &documents {
        for op in OPS {
            let a = accounting_of(document, op);
            whole_buffer_total += a.whole_buffer;
            println!(
                "{:<20}  {:<20}  {:>8}  {:>10}  {:>8}  {:>10}  {:>13}  {:>12}",
                document.name,
                op.name(),
                a.payload,
                a.undo_retained,
                a.materializations,
                a.materialized_bytes,
                a.whole_buffer,
                bytes(a.allocated),
            );
        }
    }

    viewport_table();
    views_table();
    memory_table();

    println!("\n=== the claim ===\n");
    println!(
        "whole-buffer materializations across every operation and document: {whole_buffer_total}"
    );
    if whole_buffer_total == 0 {
        println!(
            "Nothing on the editing path asked for the document contiguously, which\n\
             is the invariant instar-text's crate documentation states."
        );
    } else {
        println!(
            "SOMETHING MATERIALIZED THE WHOLE DOCUMENT. That is the defect this\n\
             benchmark exists to find; the invariant in instar-text's crate\n\
             documentation is currently false."
        );
    }
}

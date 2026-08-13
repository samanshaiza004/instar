//! Dirty-propagation benchmark: what does one edited text leaf really cost?
//!
//! ```text
//! cargo run --release --example uibench
//! ```
//!
//! # What this measures
//!
//! Instar's commit contract is a full snapshot: the guest re-encodes the
//! whole interface, the host decodes it, validates it, diffs it, lays it out,
//! lowers it to a `PaintScene`, and rasterizes it. That makes the interesting
//! question "when one deeply nested text leaf changes, does the host redo
//! work proportional to the change or proportional to the tree?"
//!
//! So this benchmark drives each tree twice through every layer:
//!
//! ```text
//! baseline  the opening commit of a freshly built tree (diff against none)
//! change    the next commit, after exactly one text leaf changed
//! ```
//!
//! The layers are timed separately — never as one total — because a change
//! that is cheap in one layer and expensive in another is a different bug
//! with a different fix. The ratio of the change pass to the baseline pass is
//! the finding: a ratio near 1 means that layer redoes the whole tree.
//!
//! # Change-shape matrix
//!
//! Below the size sweep, a second table fixes the tree at 4,000 nodes and
//! varies the shape of the change: identical re-commit (A), one text leaf
//! (B), one container layout property (D), or every tenth text leaf (E).
//! There is deliberately no C: C was "one leaf foreground changed", and
//! protocol v1 has no style at all, so C is a single `n/a` row explaining
//! that style arrives in protocol v2.
//!
//! Every matrix row adds `ran` and `target`. `ran` says whether the host
//! would execute that layer today, derived from the ChangeSet; `target` is a
//! recorded intention, not something measured. The gap between `measured`
//! and `target` is the actual finding — today every scenario except A is
//! O(tree) at layout, lower, and raster — and the table exists to make that
//! gap visible and to show it closing later.
//!
//! # Why medians
//!
//! These durations are microseconds, so a single sample is mostly noise from
//! the allocator and the code cache. Every layer is measured repeatedly and
//! reported as the median. The iteration count is printed in the ledger so
//! the reader knows how much stability each number had.
//!
//! # Tree shape
//!
//! `root > column > column > text`: one root holds a single column of
//! columns, and every one of those columns holds text leaves. That is the
//! realistic "a list of rows" fan-out, and the changed leaf is the deepest
//! leaf of the last column — genuinely three levels down.
//!
//! The nominal 10,000-node shape is host-side only on purpose.
//! [`instar_ui::protocol::limits::MAX_NODES`] is 4096, so that tree cannot be
//! committed over the wire at all: decode rejects it with `TooManyNodes`.
//! It is built directly with `Node::root` / `Node::column` / `Node::text`,
//! the limit is not raised, and the rows say `over-limit` so nobody mistakes
//! an absent wire pass for an omission.

use std::time::{Duration, Instant};

use instar_host::SceneBuilder;
use instar_paint::PhysicalSize as PaintSize;
use instar_shell::{Presenter, default_font};
use instar_ui::ScrollState;
use instar_ui::protocol::decode_batch;
use instar_ui::{ChangeSet, DecodedUiSnapshot, Node, NodeKind, TextContext, Tree, Viewport, diff};
use instar_window::{LogicalSize, PhysicalSize as WindowSize, WindowId, WindowMetricsChanged};

const WINDOW: WindowId = WindowId::from_raw(1);
const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;

/// How many times each layer of each pass is measured. Odd, so the median is
/// an actual sample rather than an interpolation.
const ITERATIONS: usize = 9;

fn main() {
    let mut run = Run::new();
    run.note("iterations", ITERATIONS.to_string());
    run.note(
        "tree_shape",
        "root > column > column > text; one root column of columns, each column holding text leaves; \
         the changed leaf is the deepest leaf of the last column"
            .to_string(),
    );
    run.note("viewport", format!("{WIDTH}x{HEIGHT}"));
    run.note(
        "protocol_limit_nodes",
        instar_ui::protocol::limits::MAX_NODES.to_string(),
    );

    let metrics = metrics();
    let builder = SceneBuilder::new();
    let mut text = TextContext::with_monospace_face(default_font());
    let mut presenter = Presenter::new(PaintSize {
        width: WIDTH,
        height: HEIGHT,
    })
    .expect("the renderer starts");

    for shape in SHAPES {
        measure(
            shape,
            &builder,
            &mut text,
            &mut presenter,
            &metrics,
            &mut run,
        );
    }
    // The 4,000-node wire shape: the matrix fixes the tree size and varies
    // the shape of the change instead.
    measure_shape_matrix(
        &SHAPES[2],
        &builder,
        &mut text,
        &mut presenter,
        &metrics,
        &mut run,
    );

    run.report();
}

// --- tree shapes ---

/// One tree size to drive through the benchmark.
///
/// `columns` and `leaves` are chosen per nominal size; the actual node count
/// (`columns * (leaves + 1) + 2`, for root and the root column) is printed in
/// the ledger next to the nominal number.
struct Shape {
    nominal: usize,
    columns: usize,
    leaves: usize,
    /// False for the 10,000-node shape: it exceeds the protocol's node limit,
    /// so it is built directly and measured through host layers only.
    wire: bool,
}

impl Shape {
    fn nodes(&self) -> usize {
        self.columns * (self.leaves + 1) + 2
    }
}

const SHAPES: &[Shape] = &[
    Shape {
        nominal: 100,
        columns: 7,
        leaves: 13,
        wire: true,
    },
    Shape {
        nominal: 1000,
        columns: 10,
        leaves: 99,
        wire: true,
    },
    Shape {
        nominal: 4000,
        columns: 10,
        leaves: 398,
        wire: true,
    },
    Shape {
        nominal: 10000,
        columns: 20,
        leaves: 498,
        wire: false,
    },
];

/// Builds the shape: root, one column of columns, then the text leaves.
///
/// `changed` rewrites only the last leaf of the last column, to a string of
/// the same length, so the encoded batch sizes of the two passes are
/// identical and the byte column is not hiding a change.
fn build(shape: &Shape, changed: bool) -> Tree {
    let mut key = 0u32;
    let mut columns = Vec::with_capacity(shape.columns);
    for c in 0..shape.columns {
        let mut leaves = Vec::with_capacity(shape.leaves);
        for l in 0..shape.leaves {
            let is_target = changed && c == shape.columns - 1 && l == shape.leaves - 1;
            let text = if is_target {
                // One past the real index: same length, visibly different text.
                format!("row {c:04} col {0:04}", shape.leaves)
            } else {
                format!("row {c:04} col {l:04}")
            };
            leaves.push(Node::text(key, text));
            key += 1;
        }
        columns.push(Node::column(key, leaves));
        key += 1;
    }
    let root_column = Node::column(key, columns);
    key += 1;
    Tree::new(Node::root(key, vec![root_column]))
}

// --- change-shape matrix ---

/// One change shape to drive at the fixed 4,000-node tree size.
///
/// `changed` rewrites the baseline snapshot. `missing` is true for the one
/// scenario protocol v1 cannot express at all (C), which gets a single `n/a`
/// row instead of a measurement.
struct ChangeShape {
    id: &'static str,
    /// Scenario description for the ledger.
    what: &'static str,
    changed: fn(&Shape, &Tree) -> Tree,
    missing: bool,
}

const CHANGE_SHAPES: &[ChangeShape] = &[
    ChangeShape {
        id: "A",
        what: "identical snapshot: re-commit the exact same tree",
        changed: change_identical,
        missing: false,
    },
    ChangeShape {
        id: "B",
        what: "one text leaf changed: one deep leaf's string differs",
        changed: change_one_text_leaf,
        missing: false,
    },
    ChangeShape {
        id: "C",
        what: "no style in protocol v1; arrives in protocol v2",
        changed: change_missing,
        missing: true,
    },
    ChangeShape {
        id: "D",
        what: "one container layout property: one intermediate column's padding differs; no text changes",
        changed: change_one_layout,
        missing: false,
    },
    ChangeShape {
        id: "E",
        what: "10% of nodes changed: every tenth text leaf's string differs",
        changed: change_every_tenth_text,
        missing: false,
    },
];

/// Re-commit the exact same tree: an empty ChangeSet.
fn change_identical(_shape: &Shape, base: &Tree) -> Tree {
    base.clone()
}

/// Change one deep leaf: the last text leaf of the last column.
fn change_one_text_leaf(shape: &Shape, base: &Tree) -> Tree {
    let mut tree = base.clone();
    let last_column = tree.root.children[0].children.len() - 1;
    let last_leaf = tree.root.children[0].children[last_column].children.len() - 1;
    let node = &mut tree.root.children[0].children[last_column].children[last_leaf];
    node.kind = NodeKind::Text {
        // Same length as the baseline leaf's text, so the two encoded
        // snapshots are the same size and the wire column is not hiding
        // anything.
        text: format!("row {last_column:04} col {:04}", shape.leaves),
    };
    tree
}

/// Change one intermediate column's layout intent: padding 0 -> 8.
fn change_one_layout(_shape: &Shape, base: &Tree) -> Tree {
    let mut tree = base.clone();
    tree.root.children[0].children[0].layout.padding = 8;
    tree
}

/// Change every tenth text leaf's string, in every column.
fn change_every_tenth_text(_shape: &Shape, base: &Tree) -> Tree {
    let mut tree = base.clone();
    for (c, column) in tree.root.children[0].children.iter_mut().enumerate() {
        for (l, leaf) in column.children.iter_mut().enumerate() {
            if l % 10 == 0 {
                // "0999" never occurs in the baseline, so this is a real
                // change at the same encoded byte length.
                leaf.kind = NodeKind::Text {
                    text: format!("row {c:04} col 0999"),
                };
            }
        }
    }
    tree
}

/// C has no wire representation; the `n/a` row is emitted instead.
fn change_missing(_shape: &Shape, _base: &Tree) -> Tree {
    unreachable!("scenario C has no change to build: protocol v1 has no style")
}

/// The ChangeSet summary is the sanity check that each scenario built what
/// its label claims.
fn assert_scenario_sanity(id: &str, shape: &Shape, changes: &ChangeSet) {
    let touched = changes.touched().len();
    match id {
        "A" => {
            assert!(changes.is_empty(), "A must be a no-op commit");
            assert_eq!(touched, 0, "A must touch no nodes");
        }
        "B" => {
            assert_eq!(touched, 1, "B must touch exactly one leaf");
            assert!(changes.needs_layout(), "B needs layout");
            assert!(changes.needs_paint(), "B needs paint");
        }
        "D" => {
            assert_eq!(touched, 1, "D must touch exactly one column");
            assert!(changes.needs_layout(), "D needs layout");
            assert!(changes.needs_paint(), "D needs paint");
        }
        "E" => {
            let leaves = shape.columns * shape.leaves;
            let share = touched as f64 / leaves as f64;
            assert!(
                (0.09..0.11).contains(&share),
                "E must touch roughly 10% of leaves: {touched} of {leaves}"
            );
            assert!(changes.needs_layout(), "E needs layout");
            assert!(changes.needs_paint(), "E needs paint");
        }
        other => unreachable!("unknown scenario {other}"),
    }
}

fn metrics() -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: f64::from(WIDTH),
            height: f64::from(HEIGHT),
        },
        physical_size: WindowSize {
            width: WIDTH,
            height: HEIGHT,
        },
        scale_factor: 1.0,
    }
}

// --- measurement ---

#[derive(Default)]
struct LayerSamples {
    baseline: Vec<Duration>,
    change: Vec<Duration>,
}

#[derive(Default)]
struct Layers {
    decode: LayerSamples,
    validate: LayerSamples,
    diff: LayerSamples,
    layout: LayerSamples,
    lower: LayerSamples,
    raster: LayerSamples,
}

struct PassTimes {
    diff: Duration,
    layout: Duration,
    lower: Duration,
    raster: Duration,
}

/// Times diff, layout, lowering, and rasterization for one tree, in one pass.
///
/// Each layer stops its clock before the next begins, and every result feeds
/// a checksum so an optimizing build cannot throw the work away. The scene is
/// kept alive while the rasterizer runs because that is how the real shell
/// presents: lower once, rasterize that scene.
#[allow(
    clippy::too_many_arguments,
    reason = "benchmark harness threads a text cache through the measured host layers"
)]
fn host_pass(
    tree: &Tree,
    previous: Option<&Tree>,
    text: &mut TextContext,
    viewport: Viewport,
    builder: &SceneBuilder,
    presenter: &mut Presenter,
    metrics: &WindowMetricsChanged,
    checksum: &mut usize,
) -> PassTimes {
    let started = Instant::now();
    let changes = diff(previous, tree).expect("the benchmark trees are valid");
    let diff = started.elapsed();
    *checksum += changes.touched().len();

    let started = Instant::now();
    let snapshot = tree.layout(text, viewport);
    let layout = started.elapsed();
    *checksum += snapshot.len();

    let started = Instant::now();
    let scene = builder.app_scene(tree, &snapshot, &ScrollState::new(), metrics, None);
    let lower = started.elapsed();
    *checksum += scene.commands.len();

    let started = Instant::now();
    let pixels = presenter
        .render(&scene)
        .expect("the host's own scene renders");
    let raster = started.elapsed();
    *checksum += pixels.len();

    PassTimes {
        diff,
        layout,
        lower,
        raster,
    }
}

fn measure(
    shape: &Shape,
    builder: &SceneBuilder,
    text: &mut TextContext,
    presenter: &mut Presenter,
    metrics: &WindowMetricsChanged,
    run: &mut Run,
) {
    let viewport = Viewport::new(WIDTH as f32, HEIGHT as f32);
    let base = build(shape, false);
    let changed = build(shape, true);
    let base_bytes = shape.wire.then(|| base.encode());
    let changed_bytes = shape.wire.then(|| changed.encode());

    let mut layers = Layers::default();
    let mut checksum = 0usize;

    for _ in 0..ITERATIONS {
        if shape.wire {
            // Baseline: the opening commit. There is no previous tree, exactly
            // as on a host that has not accepted a snapshot yet.
            let bytes = base_bytes.as_deref().expect("wire shapes always encode");
            let started = Instant::now();
            let batch = decode_batch(bytes).expect("the benchmark's own batch decodes");
            layers.decode.baseline.push(started.elapsed());
            checksum += batch.nodes.len();

            let started = Instant::now();
            let tree = DecodedUiSnapshot::from_wire(&batch)
                .expect("the benchmark's own tree validates")
                .tree;
            layers.validate.baseline.push(started.elapsed());
            checksum += tree.iter().count();

            let times = host_pass(
                &tree,
                None,
                text,
                viewport,
                builder,
                presenter,
                metrics,
                &mut checksum,
            );
            layers.diff.baseline.push(times.diff);
            layers.layout.baseline.push(times.layout);
            layers.lower.baseline.push(times.lower);
            layers.raster.baseline.push(times.raster);

            // Change: the next commit, one leaf different, still a full
            // snapshot over the wire. The previous tree is the baseline the
            // iteration just decoded.
            let bytes = changed_bytes.as_deref().expect("wire shapes always encode");
            let started = Instant::now();
            let batch = decode_batch(bytes).expect("the benchmark's own batch decodes");
            layers.decode.change.push(started.elapsed());
            checksum += batch.nodes.len();

            let started = Instant::now();
            let changed_tree = DecodedUiSnapshot::from_wire(&batch)
                .expect("the benchmark's own tree validates")
                .tree;
            layers.validate.change.push(started.elapsed());
            checksum += changed_tree.iter().count();

            let times = host_pass(
                &changed_tree,
                Some(&tree),
                text,
                viewport,
                builder,
                presenter,
                metrics,
                &mut checksum,
            );
            layers.diff.change.push(times.diff);
            layers.layout.change.push(times.layout);
            layers.lower.change.push(times.lower);
            layers.raster.change.push(times.raster);
        } else {
            // Over the protocol limit: no wire pass, host layers only, built
            // directly rather than decoded.
            let times = host_pass(
                &base,
                None,
                text,
                viewport,
                builder,
                presenter,
                metrics,
                &mut checksum,
            );
            layers.diff.baseline.push(times.diff);
            layers.layout.baseline.push(times.layout);
            layers.lower.baseline.push(times.lower);
            layers.raster.baseline.push(times.raster);

            let times = host_pass(
                &changed,
                Some(&base),
                text,
                viewport,
                builder,
                presenter,
                metrics,
                &mut checksum,
            );
            layers.diff.change.push(times.diff);
            layers.layout.change.push(times.layout);
            layers.lower.change.push(times.lower);
            layers.raster.change.push(times.raster);
        }
    }

    assert!(
        checksum > 0,
        "{}: the measured layers must actually have produced work",
        shape.nominal
    );

    if shape.wire {
        let baseline = base_bytes.expect("wire shapes encode").len();
        let change = changed_bytes.expect("wire shapes encode").len();
        run.row(Row {
            tree_size: shape.nominal,
            nodes: shape.nodes(),
            protocol: "wire",
            layer: "guest_bytes",
            baseline: baseline.to_string(),
            change: change.to_string(),
            ratio: format!("{:.2}", change as f64 / baseline as f64),
            what: "encoded batch length, bytes".to_string(),
        });
        timed_row(
            run,
            shape,
            "wire",
            "decode",
            &layers.decode,
            "instar_ui_protocol::decode_batch, microseconds",
        );
        timed_row(
            run,
            shape,
            "wire",
            "validate",
            &layers.validate,
            "instar_ui::DecodedUiSnapshot::from_wire, microseconds",
        );
    } else {
        let what = format!(
            "tree has {} nodes, over limits::MAX_NODES ({}); no wire pass",
            shape.nodes(),
            instar_ui::protocol::limits::MAX_NODES
        );
        for layer in ["guest_bytes", "decode", "validate"] {
            run.row(Row {
                tree_size: shape.nominal,
                nodes: shape.nodes(),
                protocol: "over-limit",
                layer,
                baseline: "n/a".to_string(),
                change: "n/a".to_string(),
                ratio: "n/a".to_string(),
                what: what.clone(),
            });
        }
    }

    let over_limit = if shape.wire { "wire" } else { "host-only" };
    let host_note = if shape.wire {
        "".to_string()
    } else {
        "; exceeds protocol limit, host-side only".to_string()
    };
    timed_row(
        run,
        shape,
        over_limit,
        "diff",
        &layers.diff,
        &format!("instar_ui::diff(Some(&previous), &next), microseconds{host_note}"),
    );
    timed_row(
        run,
        shape,
        over_limit,
        "layout",
        &layers.layout,
        &format!("tree.layout(&mut text, viewport), microseconds{host_note}"),
    );
    timed_row(
        run,
        shape,
        over_limit,
        "lower",
        &layers.lower,
        &format!("instar_host::SceneBuilder::app_scene, microseconds{host_note}"),
    );
    timed_row(
        run,
        shape,
        over_limit,
        "raster",
        &layers.raster,
        &format!("instar_shell::Presenter::render, microseconds{host_note}"),
    );
}

/// Change-shape matrix: the tree is fixed at 4,000 nodes and the *shape* of
/// the change varies. Same layers and median-of-N timing as the size sweep;
/// each row additionally reports whether the host would run that layer today
/// (`ran`, derived from the ChangeSet) and what the layer should cost once
/// the work is incremental (`target`).
fn measure_shape_matrix(
    shape: &Shape,
    builder: &SceneBuilder,
    text: &mut TextContext,
    presenter: &mut Presenter,
    metrics: &WindowMetricsChanged,
    run: &mut Run,
) {
    let viewport = Viewport::new(WIDTH as f32, HEIGHT as f32);
    let base = build(shape, false);
    let base_bytes = base.encode();

    for scenario in CHANGE_SHAPES {
        run.matrix_note(
            format!("matrix_scenario_{}", scenario.id),
            scenario.what.to_string(),
        );

        if scenario.missing {
            run.matrix_note(format!("matrix_{}_touched", scenario.id), "n/a".to_string());
            run.matrix_row(MatrixRow {
                tree_size: shape.nominal,
                nodes: shape.nodes(),
                scenario: scenario.id,
                layer: "all",
                ran: "n/a",
                target: "n/a",
                baseline: "n/a".to_string(),
                change: "n/a".to_string(),
                ratio: "n/a".to_string(),
                what: scenario.what.to_string(),
            });
            continue;
        }

        let changed = (scenario.changed)(shape, &base);
        let changes = diff(Some(&base), &changed).expect("the benchmark trees are valid");
        assert_scenario_sanity(scenario.id, shape, &changes);
        run.matrix_note(
            format!("matrix_{}_touched", scenario.id),
            changes.touched().len().to_string(),
        );

        let changed_bytes = changed.encode();
        let mut layers = Layers::default();
        let mut checksum = 0usize;

        for _ in 0..ITERATIONS {
            // Baseline: the opening commit, decoded from the wire like any
            // other snapshot.
            let started = Instant::now();
            let batch = decode_batch(&base_bytes).expect("the benchmark's own batch decodes");
            layers.decode.baseline.push(started.elapsed());
            checksum += batch.nodes.len();

            let started = Instant::now();
            let tree = DecodedUiSnapshot::from_wire(&batch)
                .expect("the benchmark's own tree validates")
                .tree;
            layers.validate.baseline.push(started.elapsed());
            checksum += tree.iter().count();

            let times = host_pass(
                &tree,
                None,
                text,
                viewport,
                builder,
                presenter,
                metrics,
                &mut checksum,
            );
            layers.diff.baseline.push(times.diff);
            layers.layout.baseline.push(times.layout);
            layers.lower.baseline.push(times.lower);
            layers.raster.baseline.push(times.raster);

            // Change: the second snapshot, re-committed over the wire.
            let started = Instant::now();
            let batch = decode_batch(&changed_bytes).expect("the benchmark's own batch decodes");
            layers.decode.change.push(started.elapsed());
            checksum += batch.nodes.len();

            let started = Instant::now();
            let changed_tree = DecodedUiSnapshot::from_wire(&batch)
                .expect("the benchmark's own tree validates")
                .tree;
            layers.validate.change.push(started.elapsed());
            checksum += changed_tree.iter().count();

            let times = host_pass(
                &changed_tree,
                Some(&tree),
                text,
                viewport,
                builder,
                presenter,
                metrics,
                &mut checksum,
            );
            layers.diff.change.push(times.diff);
            layers.layout.change.push(times.layout);
            layers.lower.change.push(times.lower);
            layers.raster.change.push(times.raster);
        }

        assert!(
            checksum > 0,
            "{}: the measured layers must actually have produced work",
            scenario.id
        );

        for (layer, samples, what) in [
            (
                "decode",
                &layers.decode,
                "instar_ui_protocol::decode_batch, microseconds",
            ),
            (
                "validate",
                &layers.validate,
                "instar_ui::DecodedUiSnapshot::from_wire, microseconds",
            ),
            (
                "diff",
                &layers.diff,
                "instar_ui::diff(Some(&previous), &next), microseconds",
            ),
            (
                "layout",
                &layers.layout,
                "tree.layout(&mut text, viewport), microseconds",
            ),
            (
                "lower",
                &layers.lower,
                "instar_host::SceneBuilder::app_scene, microseconds",
            ),
            (
                "raster",
                &layers.raster,
                "instar_shell::Presenter::render, microseconds",
            ),
        ] {
            let baseline = median_us(&samples.baseline);
            let change = median_us(&samples.change);
            run.matrix_row(MatrixRow {
                tree_size: shape.nominal,
                nodes: shape.nodes(),
                scenario: scenario.id,
                layer,
                ran: ran_for(layer, &changes),
                target: target_for(scenario.id, layer),
                baseline: format!("{baseline:.3}"),
                change: format!("{change:.3}"),
                ratio: format!("{:.2}", change / baseline),
                what: what.to_string(),
            });
        }
    }
}

fn ran_for(layer: &str, changes: &ChangeSet) -> &'static str {
    match layer {
        "decode" | "validate" | "diff" => "yes",
        "layout" => yes_no(changes.needs_layout()),
        "lower" | "raster" => yes_no(changes.needs_paint()),
        _ => unreachable!("unknown layer {layer}"),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// The `target` column is a recorded intention, not something measured. The
/// gap between `measured` and `target` is the actual finding: today every
/// scenario except A is O(tree) at layout, lower, and raster, and this table
/// exists to make that gap visible and to show it closing later.
fn target_for(scenario: &str, layer: &str) -> &'static str {
    match (scenario, layer) {
        ("A", "layout" | "lower" | "raster") => "none",
        ("B", "layout") => "affected ancestry",
        ("B", "lower" | "raster") => "affected region",
        ("D", "layout") => "subtree + ancestry",
        ("D", "lower" | "raster") => "changed geometry",
        ("E", "layout" | "lower" | "raster") => "proportional to changed nodes",
        (_, "decode" | "validate" | "diff") => "O(tree), inherent",
        _ => unreachable!("unknown scenario/layer {scenario}/{layer}"),
    }
}

fn timed_row(
    run: &mut Run,
    shape: &Shape,
    protocol: &'static str,
    layer: &'static str,
    samples: &LayerSamples,
    what: &str,
) {
    let baseline = median_us(&samples.baseline);
    let change = median_us(&samples.change);
    run.row(Row {
        tree_size: shape.nominal,
        nodes: shape.nodes(),
        protocol,
        layer,
        baseline: format!("{baseline:.3}"),
        change: format!("{change:.3}"),
        ratio: format!("{:.2}", change / baseline),
        what: what.to_string(),
    });
}

/// Median of a sample set, in microseconds. Sample sizes are odd, so this is
/// an actual measured pass rather than an average of two.
fn median_us(samples: &[Duration]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2].as_secs_f64() * 1e6
}

// --- ledger ---

struct Row {
    tree_size: usize,
    nodes: usize,
    protocol: &'static str,
    layer: &'static str,
    baseline: String,
    change: String,
    ratio: String,
    what: String,
}

struct MatrixRow {
    tree_size: usize,
    nodes: usize,
    scenario: &'static str,
    layer: &'static str,
    ran: &'static str,
    target: &'static str,
    baseline: String,
    change: String,
    ratio: String,
    what: String,
}

struct Run {
    notes: Vec<(String, String)>,
    rows: Vec<Row>,
    matrix_notes: Vec<(String, String)>,
    matrix_rows: Vec<MatrixRow>,
}

impl Run {
    fn new() -> Self {
        Self {
            notes: Vec::new(),
            rows: Vec::new(),
            matrix_notes: Vec::new(),
            matrix_rows: Vec::new(),
        }
    }

    fn note(&mut self, key: impl Into<String>, value: String) {
        self.notes.push((key.into(), value));
    }

    fn matrix_note(&mut self, key: impl Into<String>, value: String) {
        self.matrix_notes.push((key.into(), value));
    }

    fn row(&mut self, row: Row) {
        self.rows.push(row);
    }

    fn matrix_row(&mut self, row: MatrixRow) {
        self.matrix_rows.push(row);
    }

    /// Tab-separated: greppable, diffable, and pasteable into a markdown doc
    /// without a parser. One metric per line first, then the table.
    fn report(&self) {
        for (key, value) in &self.notes {
            println!("{key}\t{value}");
        }
        println!();
        println!("tree_size\tnodes\tprotocol\tlayer\tbaseline\tchange\tratio\twhat");
        for row in &self.rows {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                row.tree_size,
                row.nodes,
                row.protocol,
                row.layer,
                row.baseline,
                row.change,
                row.ratio,
                row.what,
            );
        }

        println!();
        for (key, value) in &self.matrix_notes {
            println!("{key}\t{value}");
        }
        println!();
        println!("tree_size\tnodes\tscenario\tlayer\tran\ttarget\tbaseline\tchange\tratio\twhat");
        for row in &self.matrix_rows {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                row.tree_size,
                row.nodes,
                row.scenario,
                row.layer,
                row.ran,
                row.target,
                row.baseline,
                row.change,
                row.ratio,
                row.what,
            );
        }
    }
}

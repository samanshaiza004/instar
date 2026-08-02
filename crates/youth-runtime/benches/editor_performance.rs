use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use youth_editor_engine::{EditorEngine, EditorLayout, ParleyEditorEngine};

const SIZES: [usize; 3] = [1024, 64 * 1024, 1024 * 1024];

fn document(size: usize) -> String {
    "x".repeat(size)
}

fn setup_engine(size: usize, offset: usize) -> ParleyEditorEngine {
    let text = document(size);
    let mut engine = ParleyEditorEngine::with_text(&text);
    engine.move_to_byte(offset.min(text.len()));
    engine
}

fn parley_mutation(c: &mut Criterion) {
    let mut group = c.benchmark_group("parley_mutation");
    for size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        for (location, offset) in [("start", 0), ("middle", size / 2), ("end", size)] {
            group.bench_with_input(BenchmarkId::new(location, size), &size, |b, &size| {
                b.iter_batched(
                    || setup_engine(size, offset),
                    |mut engine| {
                        engine.insert(black_box("a"));
                        black_box(engine);
                    },
                    BatchSize::SmallInput,
                );
            });
        }
    }
    group.finish();
}

fn youth_local_edit(c: &mut Criterion) {
    // This deliberately isolates the host-local metadata path from worker
    // scheduling and guest execution. The release stress test covers the
    // public YouthAppHandle path; keeping this seam separate makes it clear
    // whether a regression is in editor work or runtime orchestration.
    let mut group = c.benchmark_group("youth_local_edit");
    for size in SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("metadata_only_insert", size),
            &size,
            |b, &size| {
                b.iter_batched(
                    || setup_engine(size, size),
                    |mut engine| {
                        let before = engine.selection_state();
                        engine.insert("a");
                        let after = engine.selection_state();
                        black_box((before, after));
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn presentation_refresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("presentation_refresh");
    for size in SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter_batched(
                || setup_engine(size, size),
                |mut engine| black_box(engine.presentation()),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn accessibility_refresh(c: &mut Criterion) {
    let mut group = c.benchmark_group("accessibility_refresh");
    for size in SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter_batched(
                || setup_engine(size, size),
                |mut engine| {
                    let mut update = accesskit::TreeUpdate {
                        nodes: Vec::new(),
                        tree: None,
                        tree_id: accesskit::TreeId::ROOT,
                        focus: accesskit::NodeId(0),
                    };
                    let mut node = accesskit::Node::new(accesskit::Role::MultilineTextInput);
                    let mut next = 1_u64;
                    engine.accessibility_update(
                        &mut update,
                        &mut node,
                        || {
                            let id = accesskit::NodeId(next);
                            next += 1;
                            id
                        },
                        0.0,
                        0.0,
                    );
                    black_box(update);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn editor_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("editor_snapshot");
    for size in SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter_batched(
                || setup_engine(size, size),
                |mut engine| black_box(engine.state_snapshot()),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(
    editor_benches,
    parley_mutation,
    youth_local_edit,
    presentation_refresh,
    accessibility_refresh,
    editor_snapshot
);
criterion_main!(editor_benches);

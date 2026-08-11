//! Where the 386 ms went.
//!
//! Not a regression test — a controlled reproduction. It separates one-time
//! font discovery from per-shape cost, because those need completely different
//! fixes: the first is moved out of the interactive path, the second is a real
//! performance problem in the shaping design.
//!
//! Run with `--nocapture`.

use std::time::{Duration, Instant};

use instar_ui::NodeKey;
use instar_ui::text::{Alignment, Available, FontRole, ShapingStyle, TextContext};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn shape(text: &mut TextContext, key: NodeKey, string: &str, style: ShapingStyle) -> Duration {
    let started = Instant::now();
    text.measure(key, string, style, Available::Definite(400.0));
    let _ = text.finalize(key, 400.0, Alignment::Start);
    started.elapsed()
}

#[test]
fn where_the_shaping_time_goes() {
    // 1. Constructing the context.
    let started = Instant::now();
    let mut text = TextContext::new();
    let construction = started.elapsed();

    let style = ShapingStyle::default();

    // 2. The very first shape, which is where any lazy font discovery lands.
    let first = shape(&mut text, NodeKey::first(1), "Clicked 0 times", style);

    // 3. A different string each time, so every one is a genuine rebuild and
    //    the cache cannot flatter the result.
    let mut rebuilds = Vec::new();
    for n in 1..=100 {
        let string = format!("Clicked {n} times");
        rebuilds.push(shape(&mut text, NodeKey::first(2), &string, style));
    }

    // 4. The same string repeatedly, which should hit the cache.
    let mut reuses = Vec::new();
    for _ in 0..100 {
        reuses.push(shape(
            &mut text,
            NodeKey::first(3),
            "Clicked 0 times",
            style,
        ));
    }

    // 5. Monospace, to see whether an explicitly-named face differs from the
    //    generic system-ui role — the signature of fallback/enumeration cost.
    let mono = ShapingStyle {
        role: FontRole::Monospace,
        ..style
    };
    let mono_first = shape(&mut text, NodeKey::first(4), "Clicked 0 times", mono);
    let mut mono_rebuilds = Vec::new();
    for n in 1..=100 {
        let string = format!("Clicked {n} times");
        mono_rebuilds.push(shape(&mut text, NodeKey::first(5), &string, mono));
    }

    println!("\n--- shaping cost ---");
    println!("TextContext::new()      {construction:?}");
    println!("first shape (system-ui) {first:?}");
    println!("rebuild  median         {:?}", median(rebuilds.clone()));
    println!(
        "rebuild  max            {:?}",
        rebuilds.iter().max().unwrap()
    );
    println!("reuse    median         {:?}", median(reuses.clone()));
    println!("first shape (monospace) {mono_first:?}");
    println!(
        "mono rebuild median     {:?}",
        median(mono_rebuilds.clone())
    );
    println!("stats                   {:?}", text.stats());

    // The isolation the whole file exists for: if the first shape dwarfs the
    // steady state, the cost is one-time discovery sitting in the interactive
    // path, not shaping itself.
    let steady = median(rebuilds);
    println!(
        "\nfirst / steady ratio    {:.0}x",
        first.as_secs_f64() / steady.as_secs_f64().max(f64::EPSILON)
    );
}

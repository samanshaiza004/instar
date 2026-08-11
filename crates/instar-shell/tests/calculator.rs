//! The Calculator, driven by clicks, checked at its display.
//!
//! Not a second Interaction Lab: the input seams are already proven there, and
//! repeating them here would be duplicated coverage rather than new evidence.
//! What this asserts is the thing an *application* has to get right — that a
//! sequence of ordinary clicks produces the right answer on screen, with the
//! guest's state, the host's diff, layout and the retained tree all in the
//! loop.
//!
//! Every assertion reads the display text out of the retained tree, which is
//! the only surface the user actually sees.

use instar_shell::test_harness::{RuntimeHarness, launch_component};
use instar_ui::NodeKey;
use instar_window::{LogicalSize, WindowId, WindowMetricsChanged};

const WINDOW: WindowId = WindowId::from_raw(1);
const DISPLAY: NodeKey = NodeKey::first(1);

/// The keypad, by label, as `guests/calculator` numbers them.
fn key(label: &str) -> NodeKey {
    const KEYS: [(&str, u32); 20] = [
        ("C", 10),
        ("±", 11),
        ("%", 12),
        ("÷", 13),
        ("7", 14),
        ("8", 15),
        ("9", 16),
        ("×", 17),
        ("4", 18),
        ("5", 19),
        ("6", 20),
        ("−", 21),
        ("1", 22),
        ("2", 23),
        ("3", 24),
        ("+", 25),
        ("0", 26),
        ("00", 27),
        (".", 28),
        ("=", 29),
    ];
    let id = KEYS
        .iter()
        .find(|(name, _)| *name == label)
        .unwrap_or_else(|| panic!("no key labelled {label:?}"))
        .1;
    NodeKey::first(id)
}

struct Calc(RuntimeHarness);

impl Calc {
    fn open() -> Self {
        let component = std::fs::read(env!("CALCULATOR_WASM"))
            .expect("the calculator guest is built by build.rs");
        Calc(launch_component(
            component,
            WindowMetricsChanged {
                window_id: WINDOW,
                logical_size: LogicalSize {
                    width: 320.0,
                    height: 460.0,
                },
                physical_size: instar_window::PhysicalSize {
                    width: 320,
                    height: 460,
                },
                scale_factor: 1.0,
            },
        ))
    }

    /// Presses a key by its label, at the centre of wherever layout put it.
    fn press(&mut self, label: &str) {
        self.0.click_node(key(label));
        self.0
            .await_guest_commit()
            .unwrap_or_else(|error| panic!("pressing {label:?} should reach the guest: {error}"));
    }

    fn keys(&mut self, labels: &str) {
        for label in labels.split(' ') {
            self.press(label);
        }
    }

    /// What the display currently reads.
    fn display(&self) -> String {
        self.0.read_text(DISPLAY)
    }
}

/// The whole point: clicks in, arithmetic out.
#[test]
fn clicking_the_keys_computes_and_shows_the_answer() {
    let mut calc = Calc::open();
    assert_eq!(calc.display(), "0", "a calculator opens showing zero");

    calc.keys("2 + 2 =");
    assert_eq!(calc.display(), "4");

    calc.press("C");
    assert_eq!(calc.display(), "0", "clear returns it to its opening state");

    calc.keys("1 2 × 1 2 =");
    assert_eq!(calc.display(), "144", "multi-digit entry accumulates");

    calc.press("C");
    calc.keys("7 ÷ 2 =");
    assert_eq!(calc.display(), "3.5", "and the answer is not truncated");
}

/// Entry is a string, not a float, because a user passes through states no
/// `f64` can hold.
#[test]
fn the_display_shows_what_was_typed_rather_than_a_round_trip() {
    let mut calc = Calc::open();

    calc.keys("0 .");
    assert_eq!(
        calc.display(),
        "0.",
        "a trailing point is a state a user is in, and parsing it away would \
         make the decimal key look broken"
    );

    calc.press("5");
    assert_eq!(calc.display(), "0.5");

    calc.press("C");
    calc.keys("1 ±");
    assert_eq!(calc.display(), "-1", "sign is a text edit, not arithmetic");
    calc.press("±");
    assert_eq!(calc.display(), "1");
}

/// Chaining without pressing equals, which is where a naive implementation
/// loses an operand.
#[test]
fn an_operator_applies_the_pending_one_first() {
    let mut calc = Calc::open();
    calc.keys("2 + 3 × 4 =");
    assert_eq!(
        calc.display(),
        "20",
        "left to right: 2+3 is applied when × arrives, then 5×4"
    );
}

/// Dividing by zero must not produce a panic in the guest, which would take
/// the window with it.
#[test]
fn dividing_by_zero_is_shown_rather_than_fatal() {
    let mut calc = Calc::open();
    calc.keys("1 ÷ 0 =");
    assert_eq!(calc.display(), "NaN");
    calc.press("C");
    assert_eq!(
        calc.display(),
        "0",
        "and the calculator is still usable afterwards"
    );
}

/// The two wire capabilities H2 and H3 added, seen from the application that
/// asked for them.
///
/// Both were `PAIN:` markers in the raw version and neither could be fixed by
/// an SDK: a right-aligned readout and equal-width keys are things the host
/// has to be able to do, not things a builder can paper over.
#[test]
fn the_keypad_is_evenly_divided_and_the_readout_is_right_aligned() {
    let calc = Calc::open();
    let layout = calc.0.read_layout();

    // Every key in a row is the same width, whatever its label says. Without
    // `basis: Fixed(0)` each starts from its own content and "00" is wider
    // than "0"; without `min_width: Some(0)` the content minimum binds first.
    let widths: Vec<i32> = ["0", "00", ".", "="]
        .iter()
        .map(|label| layout.get(key(label)).expect("laid out").width)
        .collect();
    assert!(
        widths.windows(2).all(|pair| pair[0] == pair[1]),
        "a keypad row divides evenly: {widths:?}"
    );

    // And the readout's glyphs sit at its right edge rather than its left.
    // Asserted on the shaped artifact, because alignment moves glyphs inside
    // a box whose rectangle does not change -- a layout assertion cannot see
    // it at all.
    let display = layout.get(DISPLAY).expect("laid out");
    let shaped = layout.text.get(&DISPLAY).expect("the readout is shaped");
    let ink_right = shaped
        .runs
        .iter()
        .flat_map(|run| run.glyphs.iter())
        .map(|glyph| glyph.x)
        .fold(f32::MIN, f32::max);
    assert!(
        ink_right > display.width as f32 * 0.5,
        "the readout is aligned End, so its ink sits in the right half of a \
         {}-wide box; the last glyph starts at {ink_right}",
        display.width
    );
}

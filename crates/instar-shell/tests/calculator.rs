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

use std::sync::Arc;
use std::time::{Duration, Instant};

use instar_host::bridge::{HostBridge, Wake};
use instar_shell::default_font;
use instar_ui::NodeKey;
use instar_window::{
    LogicalPoint, LogicalSize, PointerButton, PointerState, RawPointerEvent, WindowId,
    WindowMetricsChanged, WindowOutput,
};

const WINDOW: WindowId = WindowId::from_raw(1);
const PATIENCE: Duration = Duration::from_secs(5);
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

struct Calc(HostBridge);

impl Calc {
    fn open() -> Self {
        let wake: Wake = Arc::new(|| {});
        let component = std::fs::read(env!("CALCULATOR_WASM"))
            .expect("the calculator guest is built by build.rs");
        let mut bridge =
            HostBridge::spawn_with_monospace_face(component, WINDOW, wake, default_font())
                .expect("the calculator guest starts");
        bridge.on_window_event(WindowOutput::MetricsChanged(WindowMetricsChanged {
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
        }));
        let mut calc = Calc(bridge);
        calc.settle().expect("the calculator commits its interface");
        calc
    }

    /// Presses a key by its label, at the centre of wherever layout put it.
    fn press(&mut self, label: &str) {
        let node = key(label);
        let rect = self
            .0
            .host()
            .window(WINDOW)
            .expect("the window")
            .layout()
            .expect("layout")
            .get(node)
            .unwrap_or_else(|| panic!("{label:?} should be laid out"));
        let at = LogicalPoint::new(
            f64::from(rect.x + rect.width / 2),
            f64::from(rect.y + rect.height / 2),
        );
        for state in [PointerState::Pressed, PointerState::Released] {
            self.0
                .on_window_event(WindowOutput::Pointer(RawPointerEvent {
                    window_id: WINDOW,
                    logical_pos: at,
                    button: PointerButton::Primary,
                    state,
                }));
        }
        self.settle()
            .unwrap_or_else(|| panic!("pressing {label:?} should reach the guest"));
    }

    fn keys(&mut self, labels: &str) {
        for label in labels.split(' ') {
            self.press(label);
        }
    }

    /// What the display currently reads.
    fn display(&self) -> String {
        let host = self.0.host();
        let window = host.window(WINDOW).expect("the window");
        let tree = window.tree().expect("a tree");
        tree.iter()
            .find(|node| node.key == DISPLAY)
            .map(|node| match &node.kind {
                instar_ui::NodeKind::Text { text } => text.clone(),
                other => panic!("the display should be text, got {other:?}"),
            })
            .expect("the display")
    }

    fn settle(&mut self) -> Option<()> {
        let target = self.0.commit_sequence() + 1;
        let started = Instant::now();
        while started.elapsed() < PATIENCE {
            self.0.wait(Duration::from_millis(25));
            if self.0.commit_sequence() >= target {
                return Some(());
            }
        }
        None
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

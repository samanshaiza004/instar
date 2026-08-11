//! Instar's calculator.
//!
//! The Gallery answered "does every primitive work?". This answers the
//! different and harder question: **is Instar pleasant to write against?** A
//! toolkit can be green on every primitive while the API underneath it is
//! miserable, and only an ordinary application ever finds that out.
//!
//! So this is written directly against `instar-ui-protocol`, with no helper
//! layer, on purpose. `instar-sdk` is supposed to grow from whatever this
//! makes painful and from nothing else — which means the pain has to be
//! experienced and recorded rather than predicted. Every awkwardness is noted
//! in a comment beginning `PAIN:` so the eventual SDK has an actual list of
//! requirements instead of a designer's guesses.
//!
//! ```text
//! ┌─────────────────┐
//! │           1 234 │   display
//! ├────┬────┬────┬──┤
//! │ C  │ ±  │ %  │ ÷│
//! │ 7  │ 8  │ 9  │ ×│
//! │ 4  │ 5  │ 6  │ −│
//! │ 1  │ 2  │ 3  │ +│
//! │ 0       │ .  │ =│
//! └─────────┴────┴──┘
//! ```

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
});

use instar_ui_protocol::{
    BatchEncoder, NodeKey, WireAlign, WireColor, WireEvent, WireJustify, WireLayout,
    WirePaintStyle, WireSize, WireStyle, WireTextStyle, flags, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: u32 = 0;
const DISPLAY: u32 = 1;
const KEYPAD: u32 = 2;

/// Every key, in grid order. The id is the wire key; the label is what the
/// user reads; the `Key` is what the guest acts on.
const KEYS: [(u32, &str, Key); 20] = [
    (10, "C", Key::Clear),
    (11, "±", Key::Negate),
    (12, "%", Key::Percent),
    (13, "÷", Key::Op(Op::Div)),
    (14, "7", Key::Digit(7)),
    (15, "8", Key::Digit(8)),
    (16, "9", Key::Digit(9)),
    (17, "×", Key::Op(Op::Mul)),
    (18, "4", Key::Digit(4)),
    (19, "5", Key::Digit(5)),
    (20, "6", Key::Digit(6)),
    (21, "−", Key::Op(Op::Sub)),
    (22, "1", Key::Digit(1)),
    (23, "2", Key::Digit(2)),
    (24, "3", Key::Digit(3)),
    (25, "+", Key::Op(Op::Add)),
    (26, "0", Key::Digit(0)),
    (27, "00", Key::Digit(0)),
    (28, ".", Key::Point),
    (29, "=", Key::Equals),
];

/// Row containers, one per keypad row. Ids well clear of the keys.
const ROWS: [u32; 5] = [100, 101, 102, 103, 104];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Digit(u8),
    Point,
    Op(Op),
    Equals,
    Clear,
    Negate,
    Percent,
}

const INK: WireColor = WireColor::opaque(0xf0, 0xf0, 0xf4);
const DISPLAY_BG: WireColor = WireColor::opaque(0x16, 0x16, 0x1c);
const OP_TINT: WireColor = WireColor::opaque(0x4a, 0x6f, 0xa5);

/// The calculator itself. Ordinary arithmetic state; nothing here knows about
/// Instar.
struct Calculator {
    /// What the display shows, always. Kept as text rather than a number
    /// because "0.", "-0" and "12." are all states a user can be in and none
    /// of them survives a round trip through `f64`.
    entry: String,
    /// The left operand and the operation waiting for a right one.
    pending: Option<(f64, Op)>,
    /// Set after `=` or an operator, so the next digit starts a fresh entry
    /// rather than appending to the answer.
    replace_entry: bool,
}

impl Default for Calculator {
    fn default() -> Self {
        Self {
            entry: "0".to_string(),
            pending: None,
            replace_entry: true,
        }
    }
}

impl Calculator {
    fn value(&self) -> f64 {
        self.entry.parse().unwrap_or(0.0)
    }

    fn show(&mut self, value: f64) {
        // Trim the float's tail so 2 + 2 reads "4" and not "4.0000000001".
        let text = format!("{value:.10}");
        let text = text.trim_end_matches('0').trim_end_matches('.').to_string();
        self.entry = if text.is_empty() {
            "0".to_string()
        } else {
            text
        };
        self.replace_entry = true;
    }

    fn apply(&mut self, key: Key) {
        match key {
            Key::Digit(digit) => {
                if self.replace_entry || self.entry == "0" {
                    self.entry.clear();
                    self.replace_entry = false;
                }
                if self.entry.len() < 12 {
                    self.entry.push(char::from(b'0' + digit));
                }
                if self.entry.is_empty() {
                    self.entry.push('0');
                }
            }
            Key::Point => {
                if self.replace_entry {
                    self.entry = "0".to_string();
                    self.replace_entry = false;
                }
                if !self.entry.contains('.') {
                    self.entry.push('.');
                }
            }
            Key::Op(op) => {
                let left = match self.pending.take() {
                    Some((left, pending)) if !self.replace_entry => {
                        evaluate(left, pending, self.value())
                    }
                    _ => self.value(),
                };
                self.show(left);
                self.pending = Some((left, op));
            }
            Key::Equals => {
                if let Some((left, op)) = self.pending.take() {
                    let answer = evaluate(left, op, self.value());
                    self.show(answer);
                }
            }
            Key::Clear => *self = Self::default(),
            Key::Negate => {
                if self.entry.starts_with('-') {
                    self.entry.remove(0);
                } else if self.entry != "0" {
                    self.entry.insert(0, '-');
                }
            }
            Key::Percent => {
                let value = self.value() / 100.0;
                self.show(value);
            }
        }
    }
}

fn evaluate(left: f64, op: Op, right: f64) -> f64 {
    match op {
        Op::Add => left + right,
        Op::Sub => left - right,
        Op::Mul => left * right,
        Op::Div => {
            if right == 0.0 {
                f64::NAN
            } else {
                left / right
            }
        }
    }
}

// --- the interface ------------------------------------------------------

fn key_style(key: Key) -> WireStyle {
    let tinted = matches!(key, Key::Op(_) | Key::Equals);
    WireStyle {
        text: WireTextStyle {
            size: 20,
            ..WireTextStyle::default()
        },
        paint: WirePaintStyle {
            foreground: Some(INK),
            background: tinted.then_some(OP_TINT),
            corner_radius: 6,
            ..WirePaintStyle::default()
        },
        ..WireStyle::default()
    }
}

impl Calculator {
    fn encode(&self) -> Vec<u8> {
        let mut encoder = BatchEncoder::new();

        encoder.node(
            opcode::NODE_ROOT,
            NodeKey::first(ROOT),
            0,
            None,
            WireLayout {
                padding: 12,
                gap: 10,
                ..WireLayout::default()
            },
            2,
        );

        // PAIN 1: the display wants its text on the right, and cannot say so.
        // `align_self` positions the *node*, not the glyphs inside it, so a
        // right-aligned readout is inexpressible. Stretching the node and
        // letting the text sit left is visibly wrong for a calculator; not
        // stretching it makes the panel hug the digits and jump about as they
        // change. This is Gallery ledger entry 3, reached independently, which
        // is exactly the second-application evidence the freeze criterion
        // asks for.
        encoder.node_styled(
            opcode::NODE_TEXT,
            NodeKey::first(DISPLAY),
            0,
            Some(&self.entry),
            WireLayout {
                align_self: Some(WireAlign::Stretch),
                padding: 12,
                ..WireLayout::default()
            },
            WireStyle {
                text: WireTextStyle {
                    size: 32,
                    ..WireTextStyle::default()
                },
                paint: WirePaintStyle {
                    foreground: Some(INK),
                    background: Some(DISPLAY_BG),
                    corner_radius: 8,
                    ..WirePaintStyle::default()
                },
                ..WireStyle::default()
            },
            0,
        );

        encoder.node(
            opcode::NODE_COLUMN,
            NodeKey::first(KEYPAD),
            0,
            None,
            WireLayout {
                grow: 1.0,
                gap: 8,
                align_self: Some(WireAlign::Stretch),
                ..WireLayout::default()
            },
            // PAIN 2: this count is `ROWS.len()`, and the encoder has no way
            // to check it. Every container repeats the hazard the Gallery
            // already tripped over: declare a number, then emit that many
            // subtrees, and be wrong silently if they disagree. The wire is
            // right to be a flat depth-first stream; the *guest* should not
            // have to hold the invariant by hand.
            ROWS.len() as u16,
        );

        for (row_index, row_id) in ROWS.iter().enumerate() {
            let keys = &KEYS[row_index * 4..row_index * 4 + 4];
            encoder.node(
                opcode::NODE_ROW,
                NodeKey::first(*row_id),
                0,
                None,
                WireLayout {
                    grow: 1.0,
                    gap: 8,
                    align_self: Some(WireAlign::Stretch),
                    justify_content: WireJustify::SpaceBetween,
                    ..WireLayout::default()
                },
                keys.len() as u16,
            );
            for (id, label, key) in keys {
                encoder.node_styled(
                    opcode::NODE_BUTTON,
                    NodeKey::first(*id),
                    flags::ENABLED,
                    Some(label),
                    WireLayout {
                        // PAIN 3: "each key takes an equal quarter of the row"
                        // has no direct expression. `grow: 1.0` distributes
                        // *surplus*, so keys end up sized by their labels plus
                        // a share -- "0" is narrower than "00". A fixed width
                        // would stop following the window. This is Gallery
                        // ledger entry 1 (fractional sizing) arriving from a
                        // second direction.
                        grow: 1.0,
                        height: WireSize::Fixed(44),
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    },
                    key_style(*key),
                    0,
                );
            }
        }

        encoder.finish()
    }

    async fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(self.encode())
            .await
            .map(|_| ())
            .map_err(|error| format!("commit failed: {error:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        let WireEvent::Click { node } = event;
        // PAIN 4: the guest maps a `NodeKey` back to meaning by searching its
        // own table. That is fine at twenty keys and is the shape that does
        // not scale: every application invents the same lookup, and an id that
        // drifts from its handler fails silently rather than at compile time.
        if let Some((_, _, key)) = KEYS.iter().find(|(id, _, _)| *id == node.id) {
            self.apply(*key);
        }
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut calculator = Calculator::default();
        calculator.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|error| format!("undecodable host event: {error}"))?;
                    calculator.handle(event);
                    calculator.commit().await?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);

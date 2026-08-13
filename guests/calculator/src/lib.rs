//! Instar's calculator.
//!
//! The Gallery answered "does every primitive work?". This answers the
//! different and harder question: **is Instar pleasant to write against?** A
//! toolkit can be green on every primitive while the API underneath it is
//! miserable, and only an ordinary application ever finds that out.
//!
//! It was first written directly against `instar-ui-protocol` with no helper
//! layer, on purpose: `instar-sdk` was supposed to grow from whatever this
//! made painful and from nothing else, which meant the pain had to be
//! experienced rather than predicted. Four awkwardnesses were marked `PAIN:`.
//!
//! This is the version after. All four are gone, and each disappeared for a
//! different reason:
//!
//! ```text
//! 1  a right-aligned readout was inexpressible
//!    -> WireTextLayout::End                          new wire capability
//! 2  child counts declared by hand, silently wrong
//!    -> Ui takes them from the tree                  SDK removes a hazard
//! 3  keys sized by their labels, not evenly
//!    -> basis Fixed(0) + grow + min_width            new wire capability
//! 4  NodeKey mapped back to meaning by searching
//!    -> Routes::message, keyed on the whole key      SDK removes a hazard
//! ```
//!
//! The split is the interesting part. Two were missing *capabilities* and had
//! to reach the wire; two were missing *ergonomics* and stayed out of it. An
//! SDK that had tried to paper over 1 and 3 would have had to invent
//! semantics the host could not honour.
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
    // The world now spans two packages: instar:kernel and the optional
    // instar:text capability. Without this, types from the second are an
    // error rather than generated bindings.
    generate_all,
});

use instar_sdk::{Routes, Ui};
use instar_ui_protocol::{
    WireAlign, WireBasis, WireColor, WireEvent, WireLayout, WirePaintStyle, WireSize, WireStyle,
    WireTextAlign, WireTextLayout, WireTextStyle,
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
    /// What each control in the committed snapshot means.
    ///
    /// Held rather than rebuilt on demand because it is only valid for the
    /// tree it was built from, and `Ui::finish` hands the two out together for
    /// exactly that reason.
    routes: Option<Routes<Key>>,
}

impl Default for Calculator {
    fn default() -> Self {
        Self {
            entry: "0".to_string(),
            pending: None,
            replace_entry: true,
            routes: None,
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
            Key::Clear => {
                self.entry = "0".to_string();
                self.pending = None;
                self.replace_entry = true;
            }
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

/// Every key takes an equal share of its row.
///
/// Three statements, none of which subsumes the others. `basis: Fixed(0)` says
/// distribution starts from nothing rather than from each label's own width —
/// without it, `0` comes out narrower than `00`. `grow: 1.0` says the surplus
/// is split evenly. `min_width: Some(0)` says a key may go below its content,
/// because CSS gives flex items an automatic content-based minimum and a
/// narrow window would otherwise let the widest label win.
///
/// Deliberately spelled out rather than wrapped in an SDK `.equal_share()`
/// helper. One application uses this combination; turning a meaningful
/// arrangement of primitives into vocabulary on a single data point is how a
/// thin SDK stops being thin.
fn equal_share() -> WireLayout {
    WireLayout {
        basis: WireBasis::Fixed(0),
        grow: 1.0,
        min_width: Some(0),
        height: WireSize::Fixed(44),
        align_self: Some(WireAlign::Stretch),
        ..WireLayout::default()
    }
}

impl Calculator {
    /// The whole interface, and what each control means.
    fn view(&self) -> (Vec<u8>, Routes<Key>) {
        let mut ui = Ui::new();
        ui.root(ROOT, |ui| {
            ui.text(DISPLAY, &self.entry)
                .layout(WireLayout {
                    align_self: Some(WireAlign::Stretch),
                    padding: 12,
                    ..WireLayout::default()
                })
                .style(WireStyle {
                    text: WireTextStyle {
                        size: 32,
                        ..WireTextStyle::default()
                    },
                    // The readout reads right to left, like every calculator
                    // anyone has used. `End` rather than `Right` because the
                    // two differ for right-to-left text and only one of them
                    // is a statement of intent.
                    text_layout: WireTextLayout {
                        align: WireTextAlign::End,
                    },
                    paint: WirePaintStyle {
                        foreground: Some(INK),
                        background: Some(DISPLAY_BG),
                        corner_radius: 8,
                        ..WirePaintStyle::default()
                    },
                    ..WireStyle::default()
                });

            ui.column(KEYPAD, |ui| {
                for (row, keys) in ROWS.iter().zip(KEYS.chunks(4)) {
                    ui.row(*row, |ui| {
                        for (id, label, key) in keys {
                            ui.button(*id, *label)
                                .layout(equal_share())
                                .style(key_style(*key))
                                .on_activate(*key);
                        }
                    })
                    .layout(WireLayout {
                        grow: 1.0,
                        gap: 8,
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    });
                }
            })
            .layout(WireLayout {
                grow: 1.0,
                gap: 8,
                align_self: Some(WireAlign::Stretch),
                ..WireLayout::default()
            });
        })
        .layout(WireLayout {
            padding: 12,
            gap: 10,
            ..WireLayout::default()
        });
        ui.finish()
    }

    /// Commits, and keeps the routing table the next event will be read
    /// against.
    async fn commit(&mut self) -> Result<(), String> {
        let (bytes, routes) = self.view();
        self.routes = Some(routes);
        kernel_ui::commit(bytes, Vec::new())
            .await
            .map(|_| ())
            .map_err(|error| format!("commit failed: {error:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        // The routing table is built with the snapshot it belongs to, so a
        // key that has since been retired simply is not in it. No lookup
        // table, no matching on the numeric half of a NodeKey, no chance of
        // an event for a retired node reaching whatever replaced it.
        if let Some(key) = self.routes.as_ref().and_then(|r| r.message(&event)) {
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

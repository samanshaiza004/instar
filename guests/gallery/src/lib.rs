//! Instar's UI Gallery.
//!
//! **First an integration harness, second a visual catalog.** That ordering is
//! not a preference; it is what the defects found by the first non-counter
//! guest argued for. Three complete, tested subsystems — the wheel, the
//! pointer move, the keyboard — were each disconnected by a single missing arm
//! in one `match`. Every package was correct. No package-level test could see
//! it, because at package level nothing was missing.
//!
//! So the Interaction Lab comes before the typography, and its rule is that
//! every native input modality must be shown *entering through the real
//! platform adapter* and producing a visible effect.
//!
//! ```text
//! Interaction Lab
//! ├── Pointer      hover, press/release, activation
//! ├── Scroll       wheel, thumb drag, nested scrolling
//! ├── Keyboard     Tab, Shift+Tab, Enter, Space
//! ├── Focus        offscreen reveal, focus ring
//! ├── Accessibility  focus, activate, scroll into view
//! └── Guest Stall  block wasm for 500ms
//! ```
//!
//! # Why the readout is the proof
//!
//! Every control here changes a counter, and the counters are printed at the
//! top, outside every viewport. A test that asserts a button was *hit* proves
//! the host resolved a coordinate. A test that asserts the readout *changed*
//! proves the event reached the guest, the guest committed, and the host
//! applied it — the whole round trip, which is the part a missing seam breaks.
//!
//! # Why the stall button exists
//!
//! Instar's central claim is that native interaction is independent of guest
//! liveness: scrolling, focus, the focus ring and pressed presentation are all
//! host-owned, and a guest that is busy cannot make the window stop responding.
//! That claim was previously only visible in tests. Pressing **Stall guest
//! 500ms** blocks the wasm thread outright, and everything above must keep
//! working while it is blocked. Application consequences queue; interaction
//! does not.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
    // The world now spans two packages: instar:kernel and the optional
    // instar:text capability. Without this, types from the second are an
    // error rather than generated bindings.
    generate_all,
});

use instar_ui_protocol::{
    BatchEncoder, NodeKey, WireAlign, WireBorder, WireColor, WireDisplay, WireEvent, WireFontRole,
    WireLayout, WireOverflow, WirePaintStyle, WireSize, WireStyle, WireTextStyle, WireVisibility,
    flags, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: u32 = 0;
const STATUS: u32 = 1;
const STALL: u32 = 2;

const OUTER: u32 = 10;
const OUTER_COLUMN: u32 = 11;
const POINTER_TARGET: u32 = 12;
const DISABLED: u32 = 13;
const OUTER_SPACER: u32 = 14;
const OFFSCREEN: u32 = 15;

const INNER: u32 = 20;
const INNER_COLUMN: u32 = 21;
const INNER_TOP: u32 = 22;
const INNER_SPACER: u32 = 23;
const INNER_BOTTOM: u32 = 24;

/// Taller than the inner viewport, so the inner scroll has somewhere to go —
/// and short enough that the outer scroll still has its own overflow. Nested
/// scrolling is only tested by a fixture where *both* can move.
const INNER_SPACER_HEIGHT: u16 = 300;
/// The inner viewport. Fixed, because a nested scroll that grows would take
/// the outer scroll's overflow with it.
const INNER_VIEWPORT: u16 = 120;
/// Pushes the last Lab control well outside the outer viewport, so reaching it
/// is a real reveal.
const OUTER_SPACER_HEIGHT: u16 = 400;

/// Long enough to be unmistakably a stall rather than a slow frame, short
/// enough that a test can wait it out.
const STALL_MILLIS: u64 = 500;

const INK: WireColor = WireColor::opaque(0xd8, 0xd8, 0xe0);
const SURFACE: WireColor = WireColor::opaque(0x2a, 0x2a, 0x33);
const ACCENT: WireColor = WireColor::opaque(0x4a, 0x6f, 0xa5);
const RULE: WireColor = WireColor::opaque(0x55, 0x55, 0x62);

fn heading(id: u32, text: &str) -> El {
    El::text(id, text).style(WireStyle {
        text: WireTextStyle {
            size: 16,
            weight: 700,
            ..WireTextStyle::default()
        },
        ..WireStyle::default()
    })
}

/// A label for the specimen beneath it.
fn caption(id: u32, text: &str) -> El {
    El::text(id, text).style(WireStyle {
        text: WireTextStyle {
            size: 12,
            ..WireTextStyle::default()
        },
        paint: WirePaintStyle {
            foreground: Some(WireColor::opaque(0x90, 0x90, 0x9c)),
            ..WirePaintStyle::default()
        },
        ..WireStyle::default()
    })
}

/// A visible block, so layout intent has something to show.
fn swatch(id: u32, text: &str) -> El {
    El::text(id, text).style(WireStyle {
        paint: WirePaintStyle {
            background: Some(SURFACE),
            foreground: Some(INK),
            corner_radius: 4,
            ..WirePaintStyle::default()
        },
        ..WireStyle::default()
    })
}

fn row_of(id: u32, children: Vec<El>) -> El {
    El::row(id, children).layout(WireLayout {
        gap: 8,
        align_self: Some(WireAlign::Stretch),
        ..WireLayout::default()
    })
}

fn section(id: u32, title: &str, children: Vec<El>) -> El {
    let mut items = vec![heading(id, title)];
    items.extend(children);
    El::column(id + 1, items).layout(WireLayout {
        gap: 6,
        align_self: Some(WireAlign::Stretch),
        ..WireLayout::default()
    })
}

/// A node and its children, so child counts cannot drift.
///
/// `BatchEncoder` emits a flat depth-first stream in which every node declares
/// how many children follow. Writing that by hand is fine for a counter and
/// became a real defect here at fifteen nodes: a column declared four children
/// and had five, which desynchronized the stream and made the *next* node
/// decode as a section opcode. The catalog below has sixty.
///
/// This is not an SDK and is not trying to become one. It is one guest keeping
/// one hazard out of its own source, and the count is taken from the vector
/// rather than from the author.
struct El {
    kind: u8,
    key: NodeKey,
    flags: u8,
    text: Option<String>,
    layout: WireLayout,
    style: WireStyle,
    children: Vec<El>,
}

impl El {
    fn new(kind: u8, id: u32) -> Self {
        Self {
            kind,
            key: NodeKey::first(id),
            flags: 0,
            text: None,
            layout: WireLayout::default(),
            style: WireStyle::default(),
            children: Vec::new(),
        }
    }

    fn text(id: u32, text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::new(opcode::NODE_TEXT, id)
        }
    }

    fn button(id: u32, label: impl Into<String>) -> Self {
        Self {
            text: Some(label.into()),
            flags: flags::ENABLED,
            ..Self::new(opcode::NODE_BUTTON, id)
        }
    }

    /// Present, and refusing input. Not the same as absent.
    fn disabled_button(id: u32, label: impl Into<String>) -> Self {
        Self {
            flags: 0,
            ..Self::button(id, label)
        }
    }

    fn column(id: u32, children: Vec<El>) -> Self {
        Self {
            children,
            ..Self::new(opcode::NODE_COLUMN, id)
        }
    }

    fn row(id: u32, children: Vec<El>) -> Self {
        Self {
            children,
            ..Self::new(opcode::NODE_ROW, id)
        }
    }

    fn stack(id: u32, children: Vec<El>) -> Self {
        Self {
            children,
            ..Self::new(opcode::NODE_STACK, id)
        }
    }

    /// A viewport takes exactly one content child, which is why this signature
    /// cannot express anything else.
    fn scroll(id: u32, content: El) -> Self {
        Self {
            children: vec![content],
            ..Self::new(opcode::NODE_SCROLL, id)
        }
    }

    fn root(id: u32, children: Vec<El>) -> Self {
        Self {
            children,
            ..Self::new(opcode::NODE_ROOT, id)
        }
    }

    fn layout(mut self, layout: WireLayout) -> Self {
        self.layout = layout;
        self
    }

    fn style(mut self, style: WireStyle) -> Self {
        self.style = style;
        self
    }

    fn emit(&self, encoder: &mut BatchEncoder) {
        encoder.node_styled(
            self.kind,
            self.key,
            self.flags,
            self.text.as_deref(),
            self.layout,
            self.style,
            u16::try_from(self.children.len()).expect("a node has a sane number of children"),
        );
        for child in &self.children {
            child.emit(encoder);
        }
    }
}

/// The Layout and Style catalog: every primitive, shown.
///
/// Below the Interaction Lab, because seams are the higher risk and a catalog
/// probes the layer that already has the most tests. It is still worth having:
/// a primitive that works in a unit test and looks wrong in an application is
/// a primitive nobody will use.
fn catalog() -> Vec<El> {
    vec![
        section(
            100,
            "Row and Stack",
            vec![
                caption(102, "row: three children, gap 8"),
                row_of(
                    103,
                    vec![swatch(104, "one"), swatch(105, "two"), swatch(106, "three")],
                ),
                caption(107, "stack: children share one cell, later on top"),
                // The upper child gets its own opaque background, so the
                // layering is legible. Two transparent labels in one cell are
                // correct Stack behaviour and read as corrupted glyphs, which
                // demonstrates the primitive to nobody.
                El::stack(
                    108,
                    vec![
                        El::text(109, "")
                            .layout(WireLayout {
                                height: WireSize::Fixed(44),
                                align_self: Some(WireAlign::Stretch),
                                ..WireLayout::default()
                            })
                            .style(WireStyle {
                                paint: WirePaintStyle {
                                    background: Some(SURFACE),
                                    corner_radius: 4,
                                    ..WirePaintStyle::default()
                                },
                                ..WireStyle::default()
                            }),
                        El::text(110, "  on top  ").style(WireStyle {
                            paint: WirePaintStyle {
                                foreground: Some(WireColor::opaque(0x10, 0x10, 0x14)),
                                background: Some(WireColor::opaque(0xff, 0xd0, 0x70)),
                                corner_radius: 4,
                                ..WirePaintStyle::default()
                            },
                            ..WireStyle::default()
                        }),
                    ],
                ),
            ],
        ),
        section(
            120,
            "grow, shrink, min, max",
            vec![
                caption(122, "grow 1 against grow 2"),
                row_of(
                    123,
                    vec![
                        swatch(124, "grow 1").layout(WireLayout {
                            grow: 1.0,
                            ..WireLayout::default()
                        }),
                        swatch(125, "grow 2").layout(WireLayout {
                            grow: 2.0,
                            ..WireLayout::default()
                        }),
                    ],
                ),
                caption(126, "min_width 140, then max_width 70"),
                row_of(
                    127,
                    vec![
                        swatch(128, "min").layout(WireLayout {
                            min_width: Some(140),
                            ..WireLayout::default()
                        }),
                        swatch(129, "max: this label is far too long").layout(WireLayout {
                            max_width: Some(70),
                            ..WireLayout::default()
                        }),
                    ],
                ),
            ],
        ),
        section(
            140,
            "Absent, and absent differently",
            vec![
                caption(142, "visibility: hidden keeps its space"),
                row_of(
                    143,
                    vec![
                        swatch(144, "left"),
                        swatch(145, "hidden").layout(WireLayout {
                            visibility: WireVisibility::Hidden,
                            ..WireLayout::default()
                        }),
                        swatch(146, "right"),
                    ],
                ),
                caption(147, "display: none takes none"),
                row_of(
                    148,
                    vec![
                        swatch(149, "left"),
                        swatch(150, "gone").layout(WireLayout {
                            display: WireDisplay::None,
                            ..WireLayout::default()
                        }),
                        swatch(151, "right"),
                    ],
                ),
            ],
        ),
        section(
            160,
            "Clipping",
            vec![
                caption(162, "overflow: clip, on a box smaller than its content"),
                // `min_width` is what makes this a clipping specimen rather
                // than a wrapping one. Without it the label simply wraps to
                // the box and nothing overflows to clip.
                El::column(
                    163,
                    vec![swatch(164, "clipped on the right").layout(WireLayout {
                        min_width: Some(280),
                        ..WireLayout::default()
                    })],
                )
                .layout(WireLayout {
                    width: WireSize::Fixed(160),
                    height: WireSize::Fixed(30),
                    overflow: WireOverflow::Clip,
                    ..WireLayout::default()
                }),
            ],
        ),
        section(
            170,
            "Text",
            vec![
                caption(172, "weights 300, 400, 700"),
                row_of(
                    173,
                    vec![
                        weight_sample(174, "Light", 300),
                        weight_sample(175, "Regular", 400),
                        weight_sample(176, "Bold", 700),
                    ],
                ),
                caption(177, "sizes 11, 14, 20"),
                row_of(
                    178,
                    vec![
                        size_sample(179, "11", 11),
                        size_sample(180, "14", 14),
                        size_sample(181, "20", 20),
                    ],
                ),
                caption(182, "monospace"),
                El::text(183, "0O1lI monospace").style(WireStyle {
                    text: WireTextStyle {
                        role: WireFontRole::Monospace,
                        ..WireTextStyle::default()
                    },
                    ..WireStyle::default()
                }),
            ],
        ),
        section(
            190,
            "Surfaces",
            vec![
                caption(192, "background, border, radius, and all three"),
                row_of(
                    193,
                    vec![
                        surface(
                            194,
                            "fill",
                            Some(WireColor::opaque(0x3a, 0x3a, 0x48)),
                            None,
                            0,
                        ),
                        surface(195, "border", None, Some(2), 0),
                        surface(196, "radius", Some(ACCENT), None, 10),
                        surface(197, "all", Some(SURFACE), Some(2), 8),
                    ],
                ),
                caption(198, "enabled and disabled"),
                row_of(
                    199,
                    vec![
                        El::button(200, "Enabled"),
                        El::disabled_button(201, "Disabled"),
                    ],
                ),
            ],
        ),
        // The open question from the ledger, as a pair of specimens rather
        // than an argument. Both viewports overflow, so both grow a bar.
        // The ledger's question, answered. Styling *can* make a nested
        // viewport obviously distinct -- the delineated specimen reads as its
        // own region immediately -- and two overlay bars are still
        // indistinguishable once it does. So the boundary was never the root
        // problem, and `Scroll` needs no default chrome.
        //
        // What differs between runs is the host's scrollbar policy, not
        // anything here: run with and without `--inset-scrollbars` and compare
        // these same two specimens. It is two runs rather than a side-by-side
        // because the policy is one choice for the application, which is the
        // price of keeping it off the wire.
        section(
            210,
            "Nested Scroll: run with and without --inset-scrollbars",
            vec![
                caption(212, "plain: no boundary of its own"),
                nested_specimen(220, None),
                caption(213, "delineated: background, border, radius, padding"),
                nested_specimen(230, Some(())),
            ],
        ),
    ]
}

fn weight_sample(id: u32, text: &str, weight: u16) -> El {
    El::text(id, text).style(WireStyle {
        text: WireTextStyle {
            weight,
            ..WireTextStyle::default()
        },
        ..WireStyle::default()
    })
}

fn size_sample(id: u32, text: &str, size: u16) -> El {
    El::text(id, text).style(WireStyle {
        text: WireTextStyle {
            size,
            ..WireTextStyle::default()
        },
        ..WireStyle::default()
    })
}

fn surface(
    id: u32,
    text: &str,
    background: Option<WireColor>,
    border: Option<u16>,
    radius: u16,
) -> El {
    El::text(id, text).style(WireStyle {
        paint: WirePaintStyle {
            foreground: Some(INK),
            background,
            border: border.map(|width| WireBorder { width, color: RULE }),
            corner_radius: radius,
        },
        ..WireStyle::default()
    })
}

/// One bounded viewport with content taller than it, optionally given a
/// boundary of its own.
///
/// Consumes `id..=id + 3`. Stated because it does not, and a caller that
/// assumed one id put a caption on top of a specimen's spacer -- which the
/// host caught as a duplicate key, correctly and immediately.
///
/// The two differ *only* in style, which is the point: if the delineated one
/// reads clearly, the ledger entry closes as application presentation
/// responsibility rather than a host scrollbar policy.
fn nested_specimen(id: u32, delineated: Option<()>) -> El {
    let style = if delineated.is_some() {
        WireStyle {
            paint: WirePaintStyle {
                background: Some(WireColor::opaque(0x1e, 0x1e, 0x26)),
                border: Some(WireBorder {
                    width: 1,
                    color: RULE,
                }),
                corner_radius: 6,
                ..WirePaintStyle::default()
            },
            ..WireStyle::default()
        }
    } else {
        WireStyle::default()
    };
    El::scroll(
        id,
        El::column(
            id + 1,
            vec![
                swatch(id + 2, "scroll me"),
                El::text(id + 3, "").layout(WireLayout {
                    height: WireSize::Fixed(160),
                    ..WireLayout::default()
                }),
            ],
        )
        .layout(WireLayout {
            padding: if delineated.is_some() { 8 } else { 0 },
            gap: 6,
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        }),
    )
    .layout(WireLayout {
        height: WireSize::Fixed(90),
        align_self: Some(WireAlign::Stretch),
        ..WireLayout::default()
    })
    .style(style)
}

#[derive(Default)]
struct Gallery {
    pointer: u32,
    inner_top: u32,
    inner_bottom: u32,
    offscreen: u32,
    stalls: u32,
}

impl Gallery {
    /// The proof surface. Outside every viewport, so it cannot scroll away at
    /// the moment it is needed.
    fn status(&self) -> String {
        format!(
            "pointer {} inner {}/{} offscreen {} stalls {}",
            self.pointer, self.inner_top, self.inner_bottom, self.offscreen, self.stalls
        )
    }

    /// The whole interface: the Interaction Lab, then the catalog.
    fn tree(&self) -> El {
        El::root(
            ROOT,
            vec![
                El::text(STATUS, self.status()),
                El::button(STALL, "Stall guest 500ms"),
                El::scroll(OUTER, {
                    let mut items = vec![
                        El::button(POINTER_TARGET, "Pointer target"),
                        El::disabled_button(DISABLED, "Disabled control"),
                        El::scroll(
                            INNER,
                            El::column(
                                INNER_COLUMN,
                                vec![
                                    El::button(INNER_TOP, "Inner top"),
                                    El::text(INNER_SPACER, "inner overflow").layout(WireLayout {
                                        height: WireSize::Fixed(INNER_SPACER_HEIGHT),
                                        ..WireLayout::default()
                                    }),
                                    El::button(INNER_BOTTOM, "Inner bottom"),
                                ],
                            )
                            .layout(WireLayout {
                                gap: 8,
                                align_self: Some(WireAlign::Stretch),
                                ..WireLayout::default()
                            }),
                        )
                        .layout(WireLayout {
                            height: WireSize::Fixed(INNER_VIEWPORT),
                            align_self: Some(WireAlign::Stretch),
                            ..WireLayout::default()
                        }),
                        El::text(OUTER_SPACER, "outer overflow").layout(WireLayout {
                            height: WireSize::Fixed(OUTER_SPACER_HEIGHT),
                            ..WireLayout::default()
                        }),
                        El::button(OFFSCREEN, "Offscreen target"),
                    ];
                    // The catalog lives below the Lab, inside the same
                    // viewport, so scrolling reaches all of it and the
                    // Lab's focus order is unchanged.
                    items.extend(catalog());
                    El::column(OUTER_COLUMN, items).layout(WireLayout {
                        gap: 8,
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    })
                })
                .layout(WireLayout {
                    grow: 1.0,
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        )
        .layout(WireLayout {
            padding: 12,
            gap: 8,
            ..WireLayout::default()
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        self.tree().emit(&mut encoder);
        encoder.finish()
    }

    async fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(self.encode())
            .await
            .map(|_| ())
            .map_err(|error| format!("commit failed: {error:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        match event {
            WireEvent::Click { node } if node == NodeKey::first(POINTER_TARGET) => {
                self.pointer += 1
            }
            WireEvent::Click { node } if node == NodeKey::first(INNER_TOP) => self.inner_top += 1,
            WireEvent::Click { node } if node == NodeKey::first(INNER_BOTTOM) => {
                self.inner_bottom += 1
            }
            WireEvent::Click { node } if node == NodeKey::first(OFFSCREEN) => self.offscreen += 1,
            WireEvent::Click { node } if node == NodeKey::first(STALL) => {
                self.stalls += 1;
                stall();
            }
            // Includes the disabled control, which the host refuses to hit at
            // all. If this fixture ever counts a press there, something
            // upstream stopped enforcing it.
            WireEvent::Click { .. } => {}
        }
    }
}

/// Blocks the guest outright, on purpose.
///
/// A busy loop rather than a sleep: the point is to occupy the runtime thread
/// the way a guest doing real work would, not to park politely somewhere the
/// runtime could schedule around.
fn stall() {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(STALL_MILLIS);
    while std::time::Instant::now() < until {
        std::hint::spin_loop();
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut gallery = Gallery::default();
        gallery.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|error| format!("undecodable host event: {error}"))?;
                    gallery.handle(event);
                    gallery.commit().await?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);

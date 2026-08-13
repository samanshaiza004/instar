//! B2d: a real `winit::WindowEvent` reaches a selection, and then pixels.
//!
//! # What this proves, and what it deliberately does not
//!
//! Every test here starts from a `winit::event::WindowEvent` and goes through
//! `instar_window::winit_adapter::translate` — the seam that produced three of
//! Phase 2's eight dogfooding defects, all of them missing `match` arms in one
//! function. A test that constructed a `WindowOutput` directly would not have
//! found any of them.
//!
//! ```text
//! winit::WindowEvent
//!   -> winit_adapter::translate
//!   -> WindowOutput
//!   -> instar_host::text_view::handle_pointer     the host seam
//!   -> instar-text Selection
//!   -> caret and selection pixels
//! ```
//!
//! What is **not** here is a guest. No wire vocabulary declares an editor
//! surface, so nothing a guest commits can produce a `HostTextSurface` — and
//! adding `NODE_TEXT_VIEW` to satisfy a test would mean deciding, inside a
//! pointer fixture, who creates a `TextViewId`, how a guest obtains one,
//! whether removing a node detaches or destroys the view, whether it destroys
//! the buffer, and whether focus identity is a `NodeKey` or a `TextViewId`.
//! That is package B2e, and it is architecture rather than plumbing.
//!
//! The seam exercised here is production code, not a test harness: B2e will
//! call it with a view looked up from an attached node, and nothing in this
//! file will need rewriting.

use std::sync::Arc;

use instar_host::text_view::{
    Frame, HostTextSurface, Presentation, TextInteraction, TextPointerOutcome, handle_pointer,
    instrument, lower, present,
};
use instar_paint::{Color, PhysicalSize};
use instar_shell::{Presenter, default_font};
use instar_text::{Selection, TextSystem, TextViewport};
use instar_ui::{FontRole, ShapingStyle, TextContext, TextLayout};
use instar_window::{PhysicalSize as WindowPhysicalSize, WindowId, WindowState, winit_adapter};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, WindowEvent};

const WINDOW: WindowId = WindowId::from_raw(1);
const ROW_HEIGHT: f32 = 20.0;
const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;
const BACKGROUND: Color = Color::opaque(0, 0, 0);
const INK: Color = Color::opaque(255, 255, 255);
const CARET: Color = Color::opaque(255, 0, 0);
const SELECTION: Color = Color::opaque(0, 0, 255);

/// Eight rows of ten characters, so a drag can cross rows.
const TEXT: &str = "ABCDEFGHIJ\nKLMNOPQRST\nUVWXYZabcd\nefghijklmn\nopqrstuvwx\nyz01234567\n";

/// The window, the text resource, and the surface between them.
struct Lab {
    state: WindowState,
    system: TextSystem,
    surface: HostTextSurface,
    interaction: TextInteraction,
    /// Held for the life of the lab, not per frame: Parley intends one font
    /// stack per application, and a surface that rebuilt it would be measuring
    /// font enumeration rather than input.
    _context: TextContext,
}

impl Lab {
    fn open() -> Self {
        let mut system = TextSystem::new();
        let buffer = system.open_buffer(TEXT).expect("under the bound");
        let view = system.open_view(buffer).expect("under the bound");
        let revision = system.revision(buffer).expect("a live buffer");

        let viewport = TextViewport::new(HEIGHT as f32, ROW_HEIGHT);
        let window = viewport
            .visible(system.buffer(buffer).expect("live").text(), 0)
            .expect("in bounds");

        let mut context = TextContext::with_monospace_face(Arc::clone(&default_font()));
        let presentation = present(
            &mut context,
            system.buffer(buffer).expect("live").text(),
            &window,
            &Presentation {
                style: ShapingStyle {
                    role: FontRole::Monospace,
                    size: 14.0,
                    weight: 400,
                    wrap: false,
                },
                row_height: ROW_HEIGHT,
            },
            revision,
        )
        .expect("shapeable");

        Self {
            state: WindowState::new(
                WINDOW,
                1.0,
                WindowPhysicalSize {
                    width: WIDTH,
                    height: HEIGHT,
                },
            ),
            system,
            surface: HostTextSurface {
                view,
                viewport,
                presentation,
                revision,
            },
            interaction: TextInteraction::new(),
            _context: context,
        }
    }

    /// One real winit event, through translation, into the seam.
    fn send(&mut self, event: WindowEvent) -> TextPointerOutcome {
        let Some(output) = winit_adapter::translate(&mut self.state, WINDOW, &event) else {
            return TextPointerOutcome::Ignored;
        };
        let outcome = handle_pointer(&mut self.interaction, &self.surface, (0.0, 0.0), &output);
        if let TextPointerOutcome::SelectionChanged(selection) = outcome {
            self.system
                .view_mut(self.surface.view)
                .expect("a live view")
                .set_selection(selection);
        }
        outcome
    }

    fn move_to(&mut self, x: f64, y: f64) -> TextPointerOutcome {
        self.send(WindowEvent::CursorMoved {
            device_id: winit::event::DeviceId::dummy(),
            position: PhysicalPosition::new(x, y),
        })
    }

    fn button(&mut self, state: ElementState) -> TextPointerOutcome {
        self.send(WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        })
    }

    fn leave(&mut self) -> TextPointerOutcome {
        self.send(WindowEvent::CursorLeft {
            device_id: winit::event::DeviceId::dummy(),
        })
    }

    fn unfocus(&mut self) -> TextPointerOutcome {
        self.send(WindowEvent::Focused(false))
    }

    /// What the view's persistent selection currently is.
    fn selection(&self) -> Selection {
        self.system
            .view(self.surface.view)
            .expect("a live view")
            .selection()
    }

    /// A point on `row`, `column` characters in. Monospace, so this is stable.
    fn point(&self, row: usize, column: usize) -> (f64, f64) {
        (
            8.0 * column as f64 + 1.0,
            ROW_HEIGHT as f64 * row as f64 + 5.0,
        )
    }

    fn render(&mut self) -> Vec<u8> {
        let selection = self.selection();
        let scene = lower(
            &mut self.surface.presentation,
            &Frame {
                surface: PhysicalSize {
                    width: WIDTH,
                    height: HEIGHT,
                },
                scale: 1.0,
                viewport_width: WIDTH as f32,
                viewport_height: HEIGHT as f32,
                background: BACKGROUND,
                ink: INK,
                caret_color: CARET,
                caret: Some(selection.head),
                selection: (!selection.is_empty()).then_some(selection),
                selection_color: SELECTION,
                revision: self.surface.revision,
            },
        );
        Presenter::new(PhysicalSize {
            width: WIDTH,
            height: HEIGHT,
        })
        .expect("the renderer starts")
        .render(&scene)
        .expect("renderable")
        .to_vec()
    }
}

fn count(frame: &[u8], f: impl Fn(&[u8]) -> bool) -> usize {
    frame.chunks_exact(4).filter(|p| f(p)).count()
}

fn blue(p: &[u8]) -> bool {
    p[2] > 200 && p[0] < 60 && p[1] < 60
}

fn red(p: &[u8]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

/// A click moves the caret, through the real adapter.
#[test]
fn a_click_moves_the_caret() {
    let mut lab = Lab::open();
    let (x, y) = lab.point(2, 4);

    lab.move_to(x, y);
    let outcome = lab.button(ElementState::Pressed);

    assert!(matches!(outcome, TextPointerOutcome::SelectionChanged(_)));
    let selection = lab.selection();
    assert!(selection.is_empty(), "a click collapses the selection");
    // Row 2 begins at byte 22.
    assert!(
        (22..=32).contains(&selection.head.byte),
        "a click on row 2 landed at byte {}",
        selection.head.byte
    );

    let frame = lab.render();
    assert!(count(&frame, red) > 0, "and the caret is on screen");
}

/// A drag across rows produces a selection, and pixels behind the glyphs.
#[test]
fn a_drag_across_rows_selects_and_renders() {
    let mut lab = Lab::open();

    let (x0, y0) = lab.point(0, 5);
    lab.move_to(x0, y0);
    lab.button(ElementState::Pressed);

    let (x1, y1) = lab.point(2, 5);
    lab.move_to(x1, y1);

    let selection = lab.selection();
    assert!(!selection.is_empty(), "the drag selected something");
    assert!(
        selection.range().start < 11 && selection.range().end > 22,
        "the selection spans rows 0 to 2: {:?}",
        selection.range()
    );

    let frame = lab.render();
    assert!(
        count(&frame, blue) > 100,
        "the selection produced {} highlight pixels",
        count(&frame, blue)
    );

    lab.button(ElementState::Released);
    assert!(!lab.interaction.is_dragging(), "the release ended capture");
    assert_eq!(
        lab.selection(),
        selection,
        "and the selection survives the release"
    );
}

/// Every lifecycle event that cancels a drag, each through its real winit
/// event rather than a direct call.
#[test]
fn the_lifecycle_cancels_a_drag() {
    for (name, cancel) in [("CursorLeft", 0), ("Focused(false)", 1)] {
        let mut lab = Lab::open();
        let (x, y) = lab.point(0, 2);
        lab.move_to(x, y);
        lab.button(ElementState::Pressed);
        assert!(lab.interaction.is_dragging(), "{name}: the drag started");

        let outcome = if cancel == 0 {
            lab.leave()
        } else {
            lab.unfocus()
        };

        assert_eq!(
            outcome,
            TextPointerOutcome::CaptureReleased,
            "{name} should end the drag"
        );
        assert!(!lab.interaction.is_dragging(), "{name}: capture is gone");

        // And a subsequent move does not extend anything.
        let (x2, y2) = lab.point(3, 8);
        assert_eq!(lab.move_to(x2, y2), TextPointerOutcome::Ignored);
    }
}

/// The B2d target, measured: pointer-only interaction does no text work.
#[test]
fn pointer_only_movement_does_not_edit_reshape_or_extract() {
    let mut lab = Lab::open();
    let revision_before = lab.surface.revision;

    // Warm the extraction cache the way a first frame would.
    let _ = lab.render();

    instrument::reset();
    let extractions_before = TextLayout::extractions_on_this_thread();

    let (x, y) = lab.point(0, 1);
    lab.move_to(x, y);
    lab.button(ElementState::Pressed);
    for column in 2..9 {
        let (x, y) = lab.point(1, column);
        lab.move_to(x, y);
    }
    lab.button(ElementState::Released);
    let _ = lab.render();

    let counts = instrument::snapshot();
    assert_eq!(
        counts.presentation_reshapes, 0,
        "a pointer drag reshaped the visible text {} times",
        counts.presentation_reshapes
    );
    assert_eq!(
        TextLayout::extractions_on_this_thread(),
        extractions_before,
        "a pointer drag re-extracted glyph runs"
    );
    assert_eq!(
        lab.system
            .revision(lab.system.view(lab.surface.view).expect("live").buffer())
            .expect("live"),
        revision_before,
        "a pointer drag edited the buffer"
    );
    assert!(
        counts.caret_geometry_queries > 0,
        "but geometry was asked for, so the path really ran"
    );
    assert!(!lab.selection().is_empty(), "and a selection was produced");
}

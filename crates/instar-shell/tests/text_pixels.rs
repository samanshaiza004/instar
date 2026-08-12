//! B1b: a deeply scrolled row reaches actual pixels.
//!
//! The scene-level tests in `instar-host` prove the host asked for the right
//! glyphs at the right places. Only rasterizing proves the drawing happened —
//! the distinction Phase 2 paid for with the focus ring, which had a correct
//! scene and an invisible ring for two packages.
//!
//! The fixture is built so that ink is *evidence of a specific row*. Almost
//! every line is empty and produces no glyphs; one line, ninety thousand rows
//! down, has text on it. A window that renders the wrong rows renders nothing
//! at all, so "some ink exists" is not a claim that survives a broken origin
//! or a window computed from the top of the document.

use std::sync::Arc;

use instar_host::text_view::{Presentation, lower, present};
use instar_paint::{Color, PhysicalSize};
use instar_shell::{Presenter, default_font};
use instar_text::{Revision, TextStorage, TextViewport};
use instar_ui::{FontRole, ShapingStyle, TextContext};

const ROW_HEIGHT: f32 = 20.0;
const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;
const BACKGROUND: Color = Color::opaque(0, 0, 0);
const INK: Color = Color::opaque(255, 255, 255);

/// The one row in the document with anything on it.
const MARKED_ROW: usize = 90_000;
const TOTAL_ROWS: usize = 100_000;

/// Empty lines everywhere except [`MARKED_ROW`].
///
/// Empty lines shape to no glyphs, so any ink in a frame came from the marked
/// row and from nowhere else.
fn document() -> TextStorage {
    let mut text = String::with_capacity(TOTAL_ROWS + 32);
    for row in 0..TOTAL_ROWS {
        if row == MARKED_ROW {
            text.push_str("HELLO");
        }
        text.push('\n');
    }
    TextStorage::from_text(&text)
}

fn style() -> ShapingStyle {
    ShapingStyle {
        role: FontRole::Monospace,
        size: 14.0,
        weight: 400,
        wrap: false,
    }
}

/// Renders one frame of a view scrolled to `row`, and returns its pixels.
fn frame_at(storage: &TextStorage, row: usize) -> Vec<u8> {
    let viewport = TextViewport::new(HEIGHT as f32, ROW_HEIGHT);
    let window = viewport
        .visible(storage, (row as f32 * ROW_HEIGHT) as i32)
        .expect("a clamped window is still valid");

    let mut context = TextContext::with_monospace_face(Arc::clone(&default_font()));
    let mut presented = present(
        &mut context,
        storage,
        &window,
        &Presentation {
            style: style(),
            row_height: ROW_HEIGHT,
            wrap_width: None,
        },
        Revision::default(),
    )
    .expect("a window is always shapeable");

    let scene = lower(
        &mut presented,
        PhysicalSize {
            width: WIDTH,
            height: HEIGHT,
        },
        1.0,
        BACKGROUND,
        INK,
    );

    Presenter::new(PhysicalSize {
        width: WIDTH,
        height: HEIGHT,
    })
    .expect("the renderer starts")
    .render(&scene)
    .expect("a scene the host built is renderable")
    .to_vec()
}

/// Pixels that are not the background.
fn ink_pixels(frame: &[u8]) -> usize {
    frame
        .chunks_exact(4)
        .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
        .count()
}

/// The whole point of B1b: row 90,000 is drawn, not merely described.
#[test]
fn a_deeply_scrolled_row_actually_renders() {
    let storage = document();
    let frame = frame_at(&storage, MARKED_ROW);

    assert!(
        ink_pixels(&frame) > 20,
        "the only row with text in this document is row {MARKED_ROW}, and \
         scrolling to it produced {} lit pixels -- a window computed from the \
         top of the document would produce none",
        ink_pixels(&frame)
    );
}

/// And the control: everywhere else in the same document is blank.
///
/// Without this, "ink appeared" would be satisfied by a view that renders the
/// same rows whatever the scroll offset says.
#[test]
fn a_row_with_nothing_on_it_renders_nothing() {
    let storage = document();
    let frame = frame_at(&storage, 10_000);

    assert_eq!(
        ink_pixels(&frame),
        0,
        "rows around 10,000 are empty, so any ink here means the view is not \
         showing the rows it was asked for"
    );
}

/// The frame is opaque, as every Instar frame must be.
#[test]
fn the_frame_is_opaque() {
    let storage = document();
    let frame = frame_at(&storage, MARKED_ROW);

    assert_eq!(frame.len(), (WIDTH * HEIGHT * 4) as usize);
    assert!(
        frame.chunks_exact(4).all(|pixel| pixel[3] == 255),
        "a transparent pixel would composite against whatever the platform \
         left in the window"
    );
}

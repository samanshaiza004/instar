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

use instar_host::text_view::{Frame, Presentation, lower, present};
use instar_paint::{Color, PhysicalSize};
use instar_shell::{Presenter, default_font};
use instar_text::{Revision, Selection, TextPosition, TextStorage, TextViewport};
use instar_ui::{FontRole, ShapingStyle, TextContext};

const ROW_HEIGHT: f32 = 20.0;
const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;
const BACKGROUND: Color = Color::opaque(0, 0, 0);
const INK: Color = Color::opaque(255, 255, 255);
/// Deliberately not the ink colour, so caret pixels are distinguishable from
/// glyph pixels in a rasterized frame.
const CARET: Color = Color::opaque(255, 0, 0);
/// Distinguishable from both ink and caret in a rasterized frame.
const SELECTION: Color = Color::opaque(0, 0, 255);

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
    frame_with_caret(storage, row, None)
}

/// The same, with a caret.
fn frame_with_caret(storage: &TextStorage, row: usize, caret: Option<TextPosition>) -> Vec<u8> {
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
        },
        Revision::default(),
    )
    .expect("a window is always shapeable");

    let scene = lower(
        &mut presented,
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
            caret,
            selection: None,
            selection_color: SELECTION,
            revision: Revision::default(),
        },
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

/// Pixels of exactly the caret's colour.
fn caret_pixels(frame: &[u8]) -> Vec<(usize, usize)> {
    frame
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, pixel)| pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60)
        .map(|(index, _)| (index % WIDTH as usize, index / WIDTH as usize))
        .collect()
}

/// B2b: a caret is drawn, not merely described.
///
/// The scene-level test would pass with the caret emitted before an opaque
/// background, or clipped away, or of zero width. Only the raster says it is
/// on screen.
#[test]
fn a_caret_reaches_pixels() {
    let storage = document();
    let marked_start = MARKED_ROW; // one byte per empty row before it

    let without = frame_with_caret(&storage, MARKED_ROW, None);
    let with = frame_with_caret(
        &storage,
        MARKED_ROW,
        Some(TextPosition::at(marked_start + 2)),
    );

    assert!(
        caret_pixels(&without).is_empty(),
        "a view with no caret drew caret-coloured pixels"
    );
    let lit = caret_pixels(&with);
    assert!(
        !lit.is_empty(),
        "the caret produced no pixels of its own colour -- it was emitted, \
         clipped, covered, or rounded to zero width"
    );
    // Which rows the caret occupies is checked against where the glyphs
    // actually are, not against arithmetic: the window carries two overscan
    // rows above the marked one, so "the row with text on it" is the third
    // presented row rather than the first. An assertion that recomputed that
    // offset would be asserting the fixture rather than the caret.
    let glyph_rows: Vec<usize> = with
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, pixel)| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200)
        .map(|(index, _)| index / WIDTH as usize)
        .collect();
    let (top, bottom) = (
        *glyph_rows.iter().min().expect("the marked row has glyphs"),
        *glyph_rows.iter().max().expect("the marked row has glyphs"),
    );
    // A caret spans its line box; glyph ink sits inside that box, around the
    // x-height. So the caret's vertical span contains the glyphs' — which is
    // the structural relationship, rather than the two being at the same y.
    let caret_top = lit.iter().map(|(_, y)| *y).min().expect("lit");
    let caret_bottom = lit.iter().map(|(_, y)| *y).max().expect("lit");
    assert!(
        caret_top <= top && caret_bottom >= bottom,
        "the caret drew away from the text it belongs to: caret spans \
         {caret_top}..={caret_bottom}, glyphs span {top}..={bottom}"
    );
}

/// The caret is clipped by the same viewport as the glyphs.
///
/// One coordinate system and one clip stack: a caret belonging to a row below
/// the visible area must not leak into the frame.
#[test]
fn a_caret_below_the_viewport_does_not_leak_into_the_frame() {
    let storage = document();
    // A short viewport, so the marked row's overscan neighbours sit outside it.
    let frame = {
        let viewport = TextViewport::new(HEIGHT as f32, ROW_HEIGHT);
        let window = viewport
            .visible(&storage, (MARKED_ROW as f32 * ROW_HEIGHT) as i32)
            .expect("valid");
        let mut context = TextContext::with_monospace_face(Arc::clone(&default_font()));
        let mut presented = present(
            &mut context,
            &storage,
            &window,
            &Presentation {
                style: style(),
                row_height: ROW_HEIGHT,
            },
            Revision::default(),
        )
        .expect("shapeable");

        // A caret four rows down, with a viewport two rows tall. The row must
        // be inside the *surface* -- an earlier version of this test used the
        // last presented row, at y=440 on a 320-pixel surface, so the surface
        // clipped it and the test passed with the viewport clip removed
        // entirely. It has to be visible-if-unclipped for its absence to mean
        // anything.
        let row = &presented.segments[4];
        assert!(
            row.origin_y > 2.0 * ROW_HEIGHT && row.origin_y < HEIGHT as f32,
            "the fixture needs a row below the viewport but inside the surface: \
             y={}",
            row.origin_y
        );
        let caret = TextPosition::at(row.buffer_range.start);
        let scene = lower(
            &mut presented,
            &Frame {
                surface: PhysicalSize {
                    width: WIDTH,
                    height: HEIGHT,
                },
                scale: 1.0,
                viewport_width: WIDTH as f32,
                viewport_height: 2.0 * ROW_HEIGHT,
                background: BACKGROUND,
                ink: INK,
                caret_color: CARET,
                caret: Some(caret),
                selection: None,
                selection_color: SELECTION,
                revision: Revision::default(),
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
    };

    assert!(
        caret_pixels(&frame).is_empty(),
        "a caret twenty rows below a two-row viewport was drawn anyway, so it \
         is not sharing the glyphs' clip"
    );
}

/// A caret positioned from a stale segment is not drawn at all.
#[test]
fn a_stale_revision_draws_no_caret() {
    let storage = document();
    let viewport = TextViewport::new(HEIGHT as f32, ROW_HEIGHT);
    let window = viewport
        .visible(&storage, (MARKED_ROW as f32 * ROW_HEIGHT) as i32)
        .expect("valid");
    let mut context = TextContext::with_monospace_face(Arc::clone(&default_font()));
    let mut presented = present(
        &mut context,
        &storage,
        &window,
        &Presentation {
            style: style(),
            row_height: ROW_HEIGHT,
        },
        Revision::default(),
    )
    .expect("shapeable");

    let scene = lower(
        &mut presented,
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
            caret: Some(TextPosition::at(MARKED_ROW + 2)),
            selection: None,
            selection_color: SELECTION,
            // The buffer moved on after this window was shaped.
            revision: Revision::default().next(),
        },
    );
    let frame = Presenter::new(PhysicalSize {
        width: WIDTH,
        height: HEIGHT,
    })
    .expect("the renderer starts")
    .render(&scene)
    .expect("renderable")
    .to_vec();

    assert!(
        caret_pixels(&frame).is_empty(),
        "a caret was drawn from geometry describing text that has since changed"
    );
}

/// The caret width is chosen in logical pixels and physicalized once.
#[test]
fn a_caret_survives_a_fractional_display_scale() {
    let storage = document();
    let viewport = TextViewport::new(HEIGHT as f32, ROW_HEIGHT);
    let window = viewport
        .visible(&storage, (MARKED_ROW as f32 * ROW_HEIGHT) as i32)
        .expect("valid");

    for scale in [1.0f32, 1.25, 2.0] {
        let mut context = TextContext::with_monospace_face(Arc::clone(&default_font()));
        let mut presented = present(
            &mut context,
            &storage,
            &window,
            &Presentation {
                style: style(),
                row_height: ROW_HEIGHT,
            },
            Revision::default(),
        )
        .expect("shapeable");

        let size = PhysicalSize {
            width: (WIDTH as f32 * scale) as u32,
            height: (HEIGHT as f32 * scale) as u32,
        };
        let scene = lower(
            &mut presented,
            &Frame {
                surface: size,
                scale,
                viewport_width: WIDTH as f32,
                viewport_height: HEIGHT as f32,
                background: BACKGROUND,
                ink: INK,
                caret_color: CARET,
                caret: Some(TextPosition::at(MARKED_ROW + 2)),
                selection: None,
                selection_color: SELECTION,
                revision: Revision::default(),
            },
        );
        let frame = Presenter::new(size)
            .expect("the renderer starts")
            .render(&scene)
            .expect("renderable")
            .to_vec();

        let lit = frame
            .chunks_exact(4)
            .filter(|pixel| pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60)
            .count();
        assert!(
            lit > 0,
            "at scale {scale} a one-logical-pixel caret rounded away to nothing"
        );
    }
}

/// A document with text on several consecutive rows, for selection tests.
fn prose() -> TextStorage {
    TextStorage::from_text(&"ABCDEFGHIJ\n".repeat(8))
}

/// Renders `prose` with a selection, at the top of the document.
fn selection_frame(selection: Option<Selection>) -> Vec<u8> {
    let storage = prose();
    let viewport = TextViewport::new(HEIGHT as f32, ROW_HEIGHT);
    let window = viewport.visible(&storage, 0).expect("valid");
    let mut context = TextContext::with_monospace_face(Arc::clone(&default_font()));
    let mut presented = present(
        &mut context,
        &storage,
        &window,
        &Presentation {
            style: style(),
            row_height: ROW_HEIGHT,
        },
        Revision::default(),
    )
    .expect("shapeable");

    let scene = lower(
        &mut presented,
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
            caret: None,
            selection,
            selection_color: SELECTION,
            revision: Revision::default(),
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

fn count_where(frame: &[u8], f: impl Fn(&[u8]) -> bool) -> usize {
    frame.chunks_exact(4).filter(|p| f(p)).count()
}

/// B2c: a selection reaches pixels, and sits behind the text.
///
/// The ordering half is the focus-ring lesson again: a highlight emitted after
/// the glyphs would cover them, and the scene would look identical.
#[test]
fn a_selection_renders_behind_its_glyphs() {
    let selected = selection_frame(Some(Selection {
        anchor: TextPosition::at(2),
        head: TextPosition::at(30),
    }));
    let plain = selection_frame(None);

    let blue = |p: &[u8]| p[2] > 200 && p[0] < 60 && p[1] < 60;
    let white = |p: &[u8]| p[0] > 200 && p[1] > 200 && p[2] > 200;

    assert!(count_where(&plain, blue) == 0, "no selection, no highlight");
    assert!(
        count_where(&selected, blue) > 100,
        "the selection produced {} highlight pixels",
        count_where(&selected, blue)
    );
    // Restricted to a fully selected row, and compared against the same row
    // unselected. Counting white anywhere in the frame does not work: rows 3
    // to 7 are outside the selection, so their glyphs satisfy "some white
    // exists" even when the highlight has painted over every selected glyph.
    let row_1 = |frame: &[u8], f: &dyn Fn(&[u8]) -> bool| {
        let band = ROW_HEIGHT as usize..2 * ROW_HEIGHT as usize;
        band.flat_map(|y| (0..WIDTH as usize).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let index = (y * WIDTH as usize + x) * 4;
                f(&frame[index..index + 4])
            })
            .count()
    };

    let glyphs_plain = row_1(&plain, &white);
    let glyphs_selected = row_1(&selected, &white);
    assert!(glyphs_plain > 0, "the fixture has glyphs on row 1");
    assert!(
        row_1(&selected, &blue) > 0,
        "row 1 is inside the selection and is highlighted"
    );
    assert!(
        glyphs_selected * 2 > glyphs_plain,
        "row 1 has {glyphs_selected} glyph pixels when selected against \
         {glyphs_plain} when not -- the highlight was painted over the text \
         rather than behind it, and the scene would look identical either way"
    );
}

/// The discriminating case: a selection starting midway through one row and
/// ending midway through another.
///
/// An implementation that highlighted every touched row whole would give the
/// first and last rows the same width as the middle one.
#[test]
fn a_cross_row_selection_does_not_highlight_whole_rows() {
    let frame = selection_frame(Some(Selection {
        anchor: TextPosition::at(5),
        // Row 0 is bytes 0..10, row 1 is 11..21, row 2 is 22..32.
        head: TextPosition::at(27),
    }));

    let blue = |p: &[u8]| p[2] > 200 && p[0] < 60 && p[1] < 60;
    let widths: Vec<usize> = (0..3)
        .map(|row| {
            let y = row * ROW_HEIGHT as usize + 10;
            (0..WIDTH as usize)
                .filter(|x| {
                    let index = (y * WIDTH as usize + x) * 4;
                    blue(&frame[index..index + 4])
                })
                .count()
        })
        .collect();

    assert!(
        widths[1] > widths[0] && widths[1] > widths[2],
        "the middle row is fully selected and the first and last are not: \
         {widths:?}"
    );
    assert!(widths[0] > 0 && widths[2] > 0, "all three rows are touched");
}

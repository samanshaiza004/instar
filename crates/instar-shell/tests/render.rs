//! WP7B2: the whole pipeline, ending in pixels.
//!
//! Every test here runs the real counter guest, in a real generation, on a
//! real second thread, through the real host, and rasterizes the result with
//! the real Vello CPU backend and the real font. The only thing absent is
//! winit — because an event loop needs a display server — and what winit
//! contributes to this path is a queue, a wake, and a buffer to copy into.
//! All three are exercised directly.
//!
//! Assertions are on the *pixels*, not on the scene that produced them. A
//! scene test proves the host asked for the right drawing; only a pixel test
//! proves the drawing happened. Both matter, and the scene-level ones live in
//! `instar-host`.
//!
//! Nothing here asserts an exact color at an exact coordinate. Rasterization
//! is anti-aliased and font rendering is platform-sensitive, so the properties
//! checked are the ones that stay true across all of that: the frame is
//! opaque, the background is the host's, glyph ink exists inside the boxes
//! layout computed, and a click visibly changes the window.

use std::sync::Arc;
use std::time::{Duration, Instant};

use instar_host::bridge::{HostBridge, Wake};
use instar_host::{HostEffect, HostWindow, PresentationState};
use instar_paint::{Color, PhysicalSize};
use instar_shell::{Presenter, default_font};
use instar_ui::NodeKey;
use instar_window::{
    LogicalPoint, LogicalSize, PointerButton, PointerState, RawPointerEvent, WindowId,
    WindowMetricsChanged, WindowOutput,
};

const WINDOW: WindowId = WindowId::from_raw(1);

/// The counter guest's node keys, as a host learns them: off the wire.
const READOUT: NodeKey = NodeKey::first(3);
const INCREMENT: NodeKey = NodeKey::first(4);
const CRASH: NodeKey = NodeKey::first(6);

const PATIENCE: Duration = Duration::from_secs(5);

fn component() -> Vec<u8> {
    std::fs::read(env!("COUNTER_WASM")).expect("the counter guest is built by build.rs")
}

fn metrics(scale: f64) -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: 480.0,
            height: 320.0,
        },
        physical_size: instar_window::PhysicalSize {
            width: (480.0 * scale) as u32,
            height: (320.0 * scale) as u32,
        },
        scale_factor: scale,
    }
}

/// A bridge with the real font, metrics set, and the guest's opening
/// interface applied.
fn ready() -> HostBridge {
    let wake: Wake = Arc::new(|| {});
    let mut bridge =
        HostBridge::spawn_with_monospace_face(component(), WINDOW, wake, default_font())
            .expect("the counter guest starts");
    bridge.on_window_event(WindowOutput::MetricsChanged(metrics(1.0)));
    await_commit(&mut bridge).expect("the guest commits its opening interface");
    bridge
}

/// Waits for one more applied commit.
fn await_commit(bridge: &mut HostBridge) -> Option<()> {
    let target = bridge.commit_sequence() + 1;
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        bridge.wait(Duration::from_millis(50));
        if bridge.commit_sequence() >= target {
            return Some(());
        }
    }
    None
}

/// Waits for the guest to be reported gone, returning the effects that said so.
fn await_guest_gone(bridge: &mut HostBridge) -> Vec<HostEffect> {
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        let effects = bridge.wait(Duration::from_millis(50));
        if effects
            .iter()
            .any(|effect| matches!(effect, HostEffect::GuestGone { .. }))
        {
            return effects;
        }
    }
    panic!("the guest never reported that it was gone");
}

fn pointer(state: PointerState, x: f64, y: f64) -> WindowOutput {
    WindowOutput::Pointer(RawPointerEvent {
        window_id: WINDOW,
        logical_pos: LogicalPoint::new(x, y),
        button: PointerButton::Primary,
        state,
    })
}

fn centre(bridge: &HostBridge, key: NodeKey) -> (f64, f64) {
    let rect = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::layout)
        .and_then(|layout| layout.get(key))
        .unwrap_or_else(|| panic!("{key} should have host-computed geometry"));
    (
        f64::from(rect.x + rect.width / 2),
        f64::from(rect.y + rect.height / 2),
    )
}

fn click(bridge: &mut HostBridge, key: NodeKey) {
    let (x, y) = centre(bridge, key);
    bridge.on_window_event(pointer(PointerState::Pressed, x, y));
    bridge.on_window_event(pointer(PointerState::Released, x, y));
}

/// Renders whatever the host currently wants shown.
fn frame(bridge: &HostBridge, presenter: &mut Presenter) -> Vec<u8> {
    let scene = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::scene)
        .expect("a ready window always has something to present");
    presenter
        .render(scene)
        .expect("the host's own scene should always be renderable")
        .to_vec()
}

fn presenter() -> Presenter {
    Presenter::new(PhysicalSize {
        width: 480,
        height: 320,
    })
    .expect("the renderer starts")
}

/// How many pixels are (approximately) this color. Approximate because
/// anti-aliasing means almost nothing is exactly anything.
fn near(pixels: &[u8], color: Color, tolerance: i32) -> usize {
    pixels
        .chunks_exact(4)
        .filter(|pixel| {
            (i32::from(pixel[0]) - i32::from(color.r)).abs() <= tolerance
                && (i32::from(pixel[1]) - i32::from(color.g)).abs() <= tolerance
                && (i32::from(pixel[2]) - i32::from(color.b)).abs() <= tolerance
        })
        .count()
}

// --- The frame itself ---

/// The invariant the whole presentation path rests on: softbuffer's output
/// format carries no alpha, so a frame with a translucent pixel in it has no
/// representation and would have to be dropped.
#[test]
fn every_pixel_of_a_rendered_frame_is_opaque() {
    let bridge = ready();
    let pixels = frame(&bridge, &mut presenter());

    let translucent = pixels.chunks_exact(4).filter(|p| p[3] != 255).count();
    assert_eq!(
        translucent, 0,
        "{translucent} pixels survived compositing without full alpha; an \
         0x00RRGGBB window buffer cannot carry them"
    );
}

#[test]
fn a_rendered_frame_packs_into_a_window_buffer() {
    let bridge = ready();
    let mut presenter = presenter();
    let scene = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::scene)
        .expect("lowered");

    let mut buffer = vec![0u32; 480 * 320];
    presenter
        .render_into_window(scene, &mut buffer)
        .expect("the host's own scene should reach a window buffer");

    assert!(
        buffer.iter().any(|word| *word != buffer[0]),
        "a window of one uniform color means nothing was drawn into it"
    );
    assert!(
        buffer.iter().all(|word| word >> 24 == 0),
        "softbuffer's format is 0x00RRGGBB; the top byte must stay clear"
    );
}

#[test]
fn the_frame_is_painted_in_the_hosts_own_colors() {
    let bridge = ready();
    let pixels = frame(&bridge, &mut presenter());
    let theme = bridge.host().theme();

    assert!(
        near(&pixels, theme.background, 2) > 0,
        "the window background should be the host's"
    );
    assert!(
        near(&pixels, theme.button_face, 2) > 0,
        "the guest's buttons should be drawn in the host's button color"
    );
    assert!(
        near(&pixels, theme.disabled_face, 2) > 0,
        "Reset starts disabled, and a disabled control should look unavailable"
    );
}

/// Text actually rasterized, and inside the box the host measured for it. A
/// painter using its font's own advances instead of layout's would put ink
/// outside this rectangle while every other test here still passed.
#[test]
fn glyph_ink_lands_inside_the_box_layout_computed_for_it() {
    let bridge = ready();
    let pixels = frame(&bridge, &mut presenter());
    let theme = bridge.host().theme();

    let box_ = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::layout)
        .and_then(|layout| layout.get(READOUT))
        .expect("the readout is laid out");

    // The readout is the only text on its rows, so ink on those rows belongs
    // to it — which makes "did this run stay in its box" answerable without
    // knowing which glyph is which.
    let mut inside = 0usize;
    let mut escaped = 0usize;
    for (index, pixel) in pixels.chunks_exact(4).enumerate() {
        let is_ink = (i32::from(pixel[0]) - i32::from(theme.text.r)).abs() <= 12
            && (i32::from(pixel[1]) - i32::from(theme.text.g)).abs() <= 12
            && (i32::from(pixel[2]) - i32::from(theme.text.b)).abs() <= 12;
        if !is_ink {
            continue;
        }
        let (x, y) = ((index % 480) as i32, (index / 480) as i32);
        if y < box_.y || y >= box_.y + box_.height {
            continue;
        }
        if x >= box_.x && x < box_.x + box_.width {
            inside += 1;
        } else {
            escaped += 1;
        }
    }

    assert!(
        inside > 0,
        "no ink inside the readout's box -- the text did not rasterize at all"
    );
    assert_eq!(
        escaped, 0,
        "{escaped} ink pixels on the readout's rows fell outside the box \
         layout sized for it; painting must use the advance layout measured with"
    );
}

// --- The frame changing ---

#[test]
fn a_click_visibly_changes_the_window() {
    let mut bridge = ready();
    let mut presenter = presenter();
    let before = frame(&bridge, &mut presenter);

    click(&mut bridge, INCREMENT);
    await_commit(&mut bridge).expect("the guest answers the click");
    let after = frame(&bridge, &mut presenter);

    assert_ne!(
        before.len(),
        0,
        "the fixture should have rendered something to compare"
    );
    assert_ne!(
        before, after,
        "the readout went from 'Not clicked yet' to 'Clicked once'; if the \
         pixels are identical, nothing between the click and the screen worked"
    );
}

/// Reset starts disabled and becomes enabled once there is something to
/// reset — a guest-driven state change that has to survive the whole path
/// and come out as different pixels.
#[test]
fn a_control_becoming_enabled_is_visible() {
    let mut bridge = ready();
    let mut presenter = presenter();
    let theme = *bridge.host().theme();
    let disabled_before = near(&frame(&bridge, &mut presenter), theme.disabled_face, 2);

    click(&mut bridge, INCREMENT);
    await_commit(&mut bridge).expect("the guest answers the click");

    let disabled_after = near(&frame(&bridge, &mut presenter), theme.disabled_face, 2);
    assert!(
        disabled_after < disabled_before,
        "Reset should have stopped looking disabled ({disabled_before} -> \
         {disabled_after} pixels)"
    );
}

// --- The crash screen ---

/// The end-to-end version of WP7B2's crash path: a guest that really traps,
/// on a real thread, producing a real host-owned frame.
#[test]
fn a_trapping_guest_ends_up_on_screen_as_the_hosts_crash_screen() {
    let mut bridge = ready();
    let mut presenter = presenter();
    let theme = *bridge.host().theme();

    click(&mut bridge, CRASH);
    let effects = await_guest_gone(&mut bridge);

    assert!(
        effects.contains(&HostEffect::Render { window: WINDOW }),
        "the crash has to ask for the frame that shows it"
    );
    assert!(bridge.host().presentation().is_crashed());

    let pixels = frame(&bridge, &mut presenter);
    assert!(
        near(&pixels, theme.crash_background, 2) > 0,
        "the crash screen's background should be on screen"
    );
    assert_eq!(
        near(&pixels, theme.background, 2),
        0,
        "and none of the app's background should still be showing"
    );
    assert!(
        near(&pixels, theme.crash_text, 40) > 0,
        "a crash screen that says nothing is just a differently colored window"
    );
}

/// The layering rule, checked where it would actually be broken: the host
/// draws a crash screen without ever having authored a tree to draw it from.
#[test]
fn the_crash_screen_leaves_the_guests_last_interface_in_the_tree() {
    let mut bridge = ready();
    let before = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::tree)
        .cloned()
        .expect("the guest committed an interface");

    click(&mut bridge, CRASH);
    await_guest_gone(&mut bridge);

    assert_eq!(
        bridge.host().window(WINDOW).and_then(HostWindow::tree),
        Some(&before),
        "the retained tree must still be the guest's own words, not the \
         host's account of its death"
    );
    assert!(matches!(
        bridge.host().presentation(),
        PresentationState::Crashed { .. }
    ));
}

#[test]
fn the_crash_screen_renders_at_any_scale() {
    for scale in [1.0, 2.0] {
        let mut bridge = ready();
        bridge.on_window_event(WindowOutput::MetricsChanged(metrics(scale)));
        click(&mut bridge, CRASH);
        await_guest_gone(&mut bridge);

        let size = metrics(scale).physical_size;
        let mut presenter = Presenter::new(PhysicalSize {
            width: size.width,
            height: size.height,
        })
        .expect("the renderer starts");

        let pixels = frame(&bridge, &mut presenter);
        assert_eq!(
            pixels.len(),
            (size.width * size.height * 4) as usize,
            "the crash screen must be rendered for the window that exists"
        );
        assert!(
            near(&pixels, bridge.host().theme().crash_text, 40) > 0,
            "the crash screen should have legible text at {scale}x"
        );
    }
}

/// The focus ring has to survive being painted.
///
/// `the_focus_ring_is_drawn_only_when_focus_is_visible` asserts the host emits
/// a `StrokeRect` in the ring's colour, and it passed throughout — while the
/// running application showed no ring at all. The stroke was pushed *before*
/// the button's own face fill, so every frame drew the ring and then painted
/// over it.
///
/// That is the distinction this file exists for, stated in its own header: a
/// scene test proves the host asked for the right drawing; only a pixel test
/// proves the drawing happened.
#[test]
fn a_focused_control_actually_shows_a_ring_in_the_pixels() {
    let mut bridge = ready();
    let mut presenter = presenter();
    let ring = bridge.host().theme().focus_ring;

    let before = near(&frame(&bridge, &mut presenter), ring, 24);

    bridge.on_window_event(WindowOutput::Key(instar_window::RawKeyEvent {
        window_id: WINDOW,
        key: instar_window::Key::Tab,
        pressed: true,
        shift: false,
        repeat: false,
    }));

    let after = near(&frame(&bridge, &mut presenter), ring, 24);
    assert!(
        after > before + 40,
        "tabbing to a control must put ring-coloured pixels on screen: {before} \
         before, {after} after. A ring that exists only in the command stream \
         is a ring the user cannot see."
    );
}

/// The scrollbar thumb is the other piece of host chrome that can be drawn
/// and then lost.
///
/// The focus ring proved this failure mode is real: a `StrokeRect` pushed
/// before a later fill exists perfectly in the command stream and never
/// reaches a pixel. The thumb shares every risk factor — host-generated, drawn
/// late, near a clip boundary, and with no guest node to notice its absence —
/// so it gets the same kind of test rather than a scene assertion.
#[test]
fn a_scrollable_viewport_actually_shows_a_thumb_in_the_pixels() {
    let wake: Wake = Arc::new(|| {});
    let component =
        std::fs::read(env!("GALLERY_WASM")).expect("the Gallery guest is built by build.rs");
    let mut bridge = HostBridge::spawn_with_monospace_face(component, WINDOW, wake, default_font())
        .expect("the Gallery guest starts");
    bridge.on_window_event(WindowOutput::MetricsChanged(metrics(1.0)));
    await_commit(&mut bridge).expect("the Gallery commits its interface");

    let mut presenter = presenter();
    let pixels = frame(&bridge, &mut presenter);
    let theme = bridge.host().theme();

    assert!(
        near(&pixels, theme.scrollbar_thumb, 12) > 100,
        "the Gallery's content overflows its viewport, so a thumb must be \
         visible: only {} pixels near the thumb colour",
        near(&pixels, theme.scrollbar_thumb, 12)
    );
    // Not the track: it is a 40-alpha wash, so what lands in the buffer is the
    // background composited with it rather than the colour itself. Asserting
    // on it would be asserting on a blend, and the thumb is the part that has
    // to be visible anyway.
}

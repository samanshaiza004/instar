//! The required workload set (docs/PHASE-3.md text-latency evidence request).
//! Each function performs exactly one native-input interaction against a
//! `RuntimeHarness`, using the same production path
//! (`instar_window::winit_adapter::translate` / `WindowOutput` /
//! `HostBridge::on_window_event`) every existing real-E2E test in this repo
//! uses. `gate::GateRun::measure_one` handles timing and waiting around
//! whichever of these it is given.

use instar_shell::test_harness::RuntimeHarness;
use instar_window::{RawKeyEvent, WindowOutput};
use winit::keyboard::NamedKey;

use crate::gate::{SURFACE, WINDOW};

/// `StableKey::Character` -> `logical: u16` is `c as u16` on the host side
/// (`crates/instar-host/src/lib.rs`'s `stable_key_id`); constructing
/// `RawKeyEvent` directly (rather than through `RuntimeHarness::key`, which
/// only covers `NamedKey`) is how this benchmark reaches arbitrary
/// characters -- `winit::event::KeyEvent` cannot be constructed outside
/// winit's own event loop (see `test_harness.rs`'s own doc comment), so
/// every real integration test in this repo that needs a character key
/// already goes around it the same way.
fn character_key(ch: char, pressed: bool, repeat: bool) -> WindowOutput {
    WindowOutput::Key(RawKeyEvent {
        window_id: WINDOW,
        key: instar_window::Key::Other,
        pressed,
        shift: false,
        repeat,
        logical_key: instar_window::StableKey::Character(ch),
        physical_code: instar_window::StableCode::Unidentified,
        location: instar_window::KeyLocation::Standard,
        modifiers: instar_window::Modifiers::default(),
    })
}

/// Ensures the Surface actually has focus and an active text-input session,
/// exactly as a real click would establish -- every workload starts from
/// this so a measured sample's latency is the edit's cost, not focus setup.
/// Call through `GateRun::send_untimed`, not `measure_one`: a focus click is
/// setup, not the thing being measured, and it may or may not itself be
/// visible under dirty presentation (irrelevant either way to a sample it
/// precedes).
pub fn focus_surface(harness: &mut RuntimeHarness) {
    let (x, y) = harness.screen_point_of(SURFACE);
    harness.click_at(x, y);
}

/// 1: ordinary ASCII typing. Only the keydown is measured -- see
/// [`release_character`] for why the paired keyup is deliberately sent
/// afterward, outside the timed interval, through `GateRun::send_untimed`
/// rather than folded into this same call.
pub fn ascii_typing(harness: &mut RuntimeHarness, ch: char) {
    harness.send_output(character_key(ch, true, false));
}

/// Measures one ordinary keystroke correctly: keydown timed, keyup settled
/// afterward outside the timed interval. Every call site in `main.rs` that
/// wants "the latency of typing one character against some backdrop" should
/// go through this rather than calling `measure_one`/`ascii_typing`
/// directly, so the keydown/keyup split does not have to be re-derived (and
/// potentially re-broken) at each of the many places that need it.
pub fn measure_keystroke(run: &mut crate::gate::GateRun, ch: char) -> crate::sample::StageTimes {
    let stage_times = run.measure_one(true, |h| ascii_typing(h, ch));
    run.send_untimed(|h| release_character(h, ch));
    stage_times
}

/// The untimed companion to [`ascii_typing`]/[`key_repeat`]. A key release
/// carries no text and, under `guests/scratchpad`'s dirty-presentation
/// event loop, provably changes nothing visible -- there is no `pressed:
/// false` arm in its key-handling match, so release always falls to the
/// wildcard and skips `present()`. Timing it would either wait forever for
/// a revision that correctly never arrives, or -- worse -- fold a real but
/// invisible processing cost into a "keystroke" latency number nobody
/// asked for. Settling it here, right after the measured keydown, is what
/// keeps it from bleeding into the next sample's baseline instead.
pub fn release_character(harness: &mut RuntimeHarness, ch: char) {
    harness.send_output(character_key(ch, false, false));
}

/// 2: key repeat. A single autorepeated keydown (`repeat: true`), the shape
/// a held key produces after the platform's initial repeat delay -- no
/// paired release, matching what a real OS repeat stream looks like key by
/// key.
pub fn key_repeat(harness: &mut RuntimeHarness, ch: char) {
    harness.send_output(character_key(ch, true, true));
}

/// 3: Unicode combining text. Delivered as a single `ImeCommit`: a base
/// letter followed by a combining acute accent (U+0301) is exactly the
/// shape a real compose sequence or IME produces, and the guest's `commit`
/// path treats it as ordinary committed text.
pub fn unicode_combining_commit(harness: &mut RuntimeHarness) {
    harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text: "e\u{0301}\u{0301}n\u{0303}".to_owned(),
    });
}

/// 4: bidi text. Mixed-direction commit: Hebrew (RTL) interleaved with
/// Latin (LTR) digits and words, the shape that forces the shaping/layout
/// path to do real bidi reordering rather than a single-direction run.
pub fn bidi_commit(harness: &mut RuntimeHarness) {
    harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text: "שלום world 123 שלום".to_owned(),
    });
}

/// 5: IME commit. A short preedit composition followed by its final commit
/// -- the CJK-style input shape: candidate text is projected transiently,
/// then replaced by the committed result.
pub fn ime_commit_sequence(harness: &mut RuntimeHarness) {
    harness.send_output(WindowOutput::ImePreedit {
        window_id: WINDOW,
        text: "にほ".to_owned(),
        cursor_range: Some((6, 6)),
    });
    harness.send_output(WindowOutput::ImePreedit {
        window_id: WINDOW,
        text: "にほん".to_owned(),
        cursor_range: Some((9, 9)),
    });
    harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text: "日本".to_owned(),
    });
}

/// 6: multiline preedit updates. A composition that itself spans multiple
/// lines, updated more than once before commit -- exercises the guest's
/// transient multi-line projection path
/// (`empty_preedit_is_delivered_before_commit_without_losing_target` in
/// `crates/instar-shell/tests/scratchpad.rs` is the correctness proof this
/// benchmark's timing counterpart).
pub fn multiline_preedit_update(harness: &mut RuntimeHarness) {
    harness.send_output(WindowOutput::ImePreedit {
        window_id: WINDOW,
        text: "first line\nsecond".to_owned(),
        cursor_range: Some((17, 17)),
    });
}

/// 7: large text-commit stress. Explicitly not a paste benchmark (the
/// Surface protocol has no clipboard/paste event) and, per a finding this
/// benchmark surfaced, not literally "100 KiB" either:
/// `instar_ui_protocol::SurfaceEvent::decode` hard-rejects any single
/// event's text over `limits::MAX_TEXT_BYTES` (4096 bytes) --
/// `TextTooLong`, checked unconditionally, not just under some size-policy
/// path. A wire-level 100 KiB single commit cannot be delivered as *one*
/// native IME-commit event under the current protocol at all; this
/// workload instead measures a single commit at the protocol's actual
/// maximum. See README.md for how this was discovered (an earlier version
/// of this workload silently corrupted a *different*, later workload's
/// measurement instead of failing loudly, which is its own finding).
pub const MAX_SINGLE_COMMIT_BYTES: usize = 4000;

pub fn max_bounded_text_commit(harness: &mut RuntimeHarness) {
    const WORD: &str = "The quick brown fox jumps over the lazy dog.";
    let mut text = String::with_capacity(MAX_SINGLE_COMMIT_BYTES);
    let mut column = 0usize;
    while text.len() + WORD.len() + 1 <= MAX_SINGLE_COMMIT_BYTES {
        text.push_str(WORD);
        text.push(' ');
        column += WORD.len() + 1;
        if column > 80 {
            text.push('\n');
            column = 0;
        }
    }
    harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text,
    });
}

/// 8: pointer placement. A single click at a point inside the Surface,
/// distinct from the shared `focus_surface` setup click -- this is the
/// measured one.
pub fn pointer_placement(harness: &mut RuntimeHarness) {
    let (x, y) = harness.screen_point_of(SURFACE);
    harness.click_at(x + 20.0, y + 5.0);
}

/// 9: drag selection. Press, several intermediate moves, release -- the
/// shape a real mouse drag produces, not a single teleporting move.
pub fn drag_selection(harness: &mut RuntimeHarness) {
    let (x, y) = harness.screen_point_of(SURFACE);
    harness.move_to(x, y);
    harness.button(winit::event::ElementState::Pressed);
    for step in 1..=6 {
        harness.move_to(x + f64::from(step) * 8.0, y);
    }
    harness.button(winit::event::ElementState::Released);
}

/// 10: rapid scrolling. A burst of wheel events at one point, the shape a
/// fast trackpad/wheel fling produces.
pub fn rapid_scrolling(harness: &mut RuntimeHarness) {
    let (x, y) = harness.screen_point_of(SURFACE);
    for _ in 0..8 {
        harness.wheel(x, y, 3.0);
    }
}

/// 12/13/14: large-document / pathological-long-line backdrop. Sent once
/// before a workload's measured samples, never itself measured: builds up
/// document size via the same `ImeCommit` path every other workload uses,
/// so no guest-only preload mechanism is needed. `newline_every` of `0`
/// produces one hard, unbroken line -- the pathological-long-line shape;
/// any other value produces ordinary paragraphs.
///
/// Chunked at [`MAX_SINGLE_COMMIT_BYTES`] and driven through
/// `GateRun::measure_one` per chunk, not one giant `ImeCommit`: the same
/// `limits::MAX_TEXT_BYTES` wire cap that shapes workload 7 makes a single
/// multi-hundred-KiB commit undecodable, and sending the chunks without
/// waiting for each one to be fully processed reproduces the same queued-
/// event pileup `GateRun::measure_one`'s own doc comment describes.
pub fn preload_document(run: &mut crate::gate::GateRun, total_bytes: usize, newline_every: usize) {
    const CHUNK_WORDS: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit ";
    let mut sent = 0usize;
    let mut since_newline = 0usize;
    while sent < total_bytes {
        let mut chunk = String::with_capacity(MAX_SINGLE_COMMIT_BYTES);
        while chunk.len() + CHUNK_WORDS.len() + 1 <= MAX_SINGLE_COMMIT_BYTES
            && sent + chunk.len() < total_bytes
        {
            chunk.push_str(CHUNK_WORDS);
            since_newline += CHUNK_WORDS.len();
            if newline_every > 0 && since_newline >= newline_every {
                chunk.push('\n');
                since_newline = 0;
            }
        }
        let remaining = total_bytes - sent;
        if chunk.len() > remaining {
            let mut end = remaining;
            while end > 0 && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            chunk.truncate(end);
        }
        sent += chunk.len();
        run.measure_one(true, |h| {
            h.send_output(WindowOutput::ImeCommit {
                window_id: WINDOW,
                text: chunk,
            });
        });
    }
}

/// Diagnostic-matrix variant of [`preload_document`]: takes an explicit
/// target line count instead of a newline-every-N-bytes cadence, so byte
/// size and line count can be varied independently of each other. Same
/// chunking/draining discipline.
pub fn preload_document_with_lines(
    run: &mut crate::gate::GateRun,
    total_bytes: usize,
    target_lines: usize,
) {
    let bytes_per_line = if target_lines == 0 {
        0
    } else {
        (total_bytes / target_lines).max(1)
    };
    preload_document(run, total_bytes, bytes_per_line);
}

/// Diagnostic-matrix helper: scrolls by `wheel_lines` (negative moves the
/// viewport *forward*/down through the document -- winit's positive wheel Y
/// is flipped to a negative Instar scroll delta by
/// `crates/instar-window/src/lib.rs`'s `on_wheel` translation, and
/// `guests/scratchpad`'s own `Scratchpad::scroll` then treats a positive
/// `dy` as "scroll_y increases") and clicks near the vertical middle of the
/// viewport, placing the caret in whatever row is now presented there.
///
/// This does not hit an exact document percentage -- `scroll_y` is guest-
/// private state this benchmark cannot read back to verify, and the wheel
/// magnitude's line-to-line mapping was not independently calibrated. What
/// it does give: `wheel_lines` values in increasing magnitude produce a
/// monotonically non-decreasing scroll position (clamped at the document's
/// end), which is what a "does latency correlate with caret position"
/// sweep actually needs. Treat the resulting labels ("near start", "near
/// end") as approximate, not exact percentages -- see README.md.
pub fn scroll_then_click(harness: &mut RuntimeHarness, wheel_lines: f32) {
    let (x, y) = harness.screen_point_of(SURFACE);
    if wheel_lines != 0.0 {
        harness.wheel(x, y, wheel_lines);
    }
    harness.click_at(x, y);
}

/// 15/16/17: deletion-latency backdrop. Not part of the original 15-workload
/// evidence request (`docs/PHASE-3.md`) -- added after that request found
/// its own gap: every existing large-document workload measured *insertion*
/// (a keystroke after [`preload_document`]), and nothing measured deletion.
/// `Document::previous_grapheme_boundary` (`crates/instar-editor-core`) used
/// to scan every grapheme from byte 0 up to the caret on every call, making
/// Backspace O(caret position) regardless of how the rest of the editor
/// performed -- a large document could pass every insertion workload while
/// Backspace near its end stayed pathological. These three exist to make
/// that regression class visible the same way the insertion workloads made
/// F1 (`docs/DOS-STARVATION-AUDIT.md`) visible.
///
/// Deliberately **not** driven through the `workload!` macro in `main.rs`:
/// that macro calls [`focus_surface`] before every measured iteration, and
/// `focus_surface`'s click hit-tests against whatever rows are currently
/// *presented*. Nothing here ever sends a scroll command, so after a
/// multi-MiB preload the presented rows are still the document's first
/// ~26 lines -- a repeated click would silently relocate the caret back
/// near byte 0 before every measured Backspace, which would measure
/// "backspace near the start" while claiming to measure "backspace at the
/// end". `main.rs`'s bespoke loops for these three establish focus once,
/// before the preload, and never re-click.
///
/// Presses [`NamedKey::Backspace`] through [`RuntimeHarness::press_key`] --
/// the same production `winit_adapter` key mapping every other keyboard
/// workload in this file uses, not a hand-built event.
pub fn backspace(harness: &mut RuntimeHarness) {
    harness.press_key(NamedKey::Backspace);
}

/// Forward-delete counterpart to [`backspace`]. Not chasing a known
/// pathology -- `Document::next_grapheme_boundary` was already near-caret,
/// slicing `byte..len_bytes()` and taking only the first grapheme -- this
/// exists so a *future* regression on the forward-delete path is caught by
/// the same class of benchmark that caught the backward one, rather than
/// assumed safe because nothing currently measures it.
pub fn delete_forward(harness: &mut RuntimeHarness) {
    harness.press_key(NamedKey::Delete);
}

/// Setup for `delete_forward_large_doc`: preload, then move the caret to the
/// start of the document's last line via [`NamedKey::Home`] (`move_edge` in
/// `guests/scratchpad`, line-relative -- there is no document-Home command).
///
/// [`preload_document`] alone leaves the caret at the document's very end
/// (every chunk it sends replaces at the *current* caret, which is how the
/// caret ends up there in the first place), where forward-delete has
/// nothing following it to remove and would measure a no-op rather than a
/// real deletion. `Home` guarantees real trailing content -- at minimum the
/// rest of the last preloaded line -- for the measured deletes to consume.
pub fn preload_document_then_home_of_last_line(
    run: &mut crate::gate::GateRun,
    total_bytes: usize,
    newline_every: usize,
) {
    preload_document(run, total_bytes, newline_every);
    // Through `send_untimed`, not a bare `run.harness.press_key(..)` call, to
    // settle before the caller's first `measure_one` establishes its
    // baseline -- matching every other untimed setup action in this file.
    // `measure_one` also unconditionally settles first, so this was not a
    // correctness bug even before the change, only an inconsistency.
    run.send_untimed(|h| {
        h.press_key(NamedKey::Home);
    });
}

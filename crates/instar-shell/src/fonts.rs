//! The face the shell ships, as bytes for Parley's font context.
//!
//! This crate is the only one that links a real font file. The host's
//! `TextEngine` registers these bytes with Parley; nothing here parses them
//! or resolves glyphs by hand anymore.

use std::sync::Arc;

/// Roboto Mono, under the SIL Open Font License; `assets/OFL.txt` is the
/// licence text. The monospace role's face, registered with Parley's font
/// context rather than loaded by hand.
pub const ROBOTO_MONO: &[u8] = include_bytes!("../assets/RobotoMono.ttf");

/// The shipped face, ready to register with a [`instar_ui::TextEngine`].
pub fn default_font() -> Arc<[u8]> {
    Arc::from(ROBOTO_MONO)
}

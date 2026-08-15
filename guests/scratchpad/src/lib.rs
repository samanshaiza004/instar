//! Guest-owned editor proof for Phase 3.
//!
//! This is deliberately a small userland policy, not a host editor API. The
//! host only sees input events and the resulting bounded presentation scene.

use instar_editor_core::{Document, Position, Selection, TextEdit};

#[derive(Debug, Clone)]
pub struct Scratchpad {
    pub document: Document,
    pub carets: Vec<Selection>,
    pub preedit: Option<String>,
    composition_target: Option<Selection>,
}

impl Scratchpad {
    pub fn new(text: &str) -> Self {
        Self { document: Document::from_text(text), carets: vec![Selection::at(0)], preedit: None, composition_target: None }
    }

    /// Guest policy: a commit with no active preedit inserts at every caret in
    /// one descending atomic batch. The host has no representation of this.
    pub fn commit(&mut self, text: &str) -> Result<(), instar_editor_core::EditError> {
        let edits = self.carets.iter().map(|selection| TextEdit::replace(selection.range(), text)).collect();
        self.document.apply_batch(edits)?;
        let end = text.len();
        self.carets = self.carets.iter().map(|selection| Selection::at(selection.range().start + end)).collect();
        self.preedit = None;
        self.composition_target = None;
        Ok(())
    }

    /// First non-empty preedit captures the canonical selection. Empty preedit
    /// clears only the visual projection so the following commit can still
    /// replace the saved target.
    pub fn preedit(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() && self.composition_target.is_none() {
            self.composition_target = self.carets.first().copied();
        }
        self.preedit = (!text.is_empty()).then_some(text);
    }

    pub fn projected_text(&self) -> Result<String, instar_editor_core::EditError> {
        let Some(preedit) = &self.preedit else { return Ok(self.document.as_string()); };
        let target = self.composition_target.unwrap_or_else(|| self.carets[0]);
        let range = target.range();
        let canonical = self.document.as_string();
        Ok(format!("{}{}{}", &canonical[..range.start], preedit, &canonical[range.end..]))
    }

    pub fn primary_position(&self) -> Position { self.carets.first().map_or(Position::at(0), |s| s.head) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_carets_are_guest_policy() {
        let mut app = Scratchpad::new("abc");
        app.carets = vec![Selection::at(1), Selection::at(3)];
        app.commit("X").unwrap();
        assert_eq!(app.document.as_string(), "aXbcX");
    }

    #[test]
    fn multiline_preedit_is_transient_and_empty_preedit_keeps_target() {
        let mut app = Scratchpad::new("hello world");
        app.carets = vec![Selection { anchor: Position::at(0), head: Position::at(5) }];
        app.preedit("a\nb");
        assert_eq!(app.projected_text().unwrap(), "a\nb world");
        app.preedit("");
        app.commit("a\nb").unwrap();
        assert_eq!(app.document.as_string(), "a\nb world");
    }
}

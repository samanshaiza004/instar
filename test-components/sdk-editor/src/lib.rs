//! SDK-backed fixture with a single host-owned Editor node and a `save`
//! button that accepts the live buffer into guest-owned state -- for
//! `.youth-test`'s editor interaction family (`type`, `paste`, `compose`,
//! `expect editor text`/`selection`). Deliberately minimal: no Countdown,
//! no schedule, nothing but the Editor contract itself.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

use youth_sdk::prelude::*;

const DOCUMENT: NodeKey = node!("document");
const SAVE: NodeKey = node!("save");
const STATUS: NodeKey = node!("status");

const DOCUMENT_REVISION_KEY: &str = "document_revision";
const TEXT_KEY: &str = "text";

struct EditorFixture;

impl Application for EditorFixture {
    fn view(context: &ViewContext) -> Result<Tree> {
        let document_revision = context
            .state()
            .integer(DOCUMENT_REVISION_KEY)?
            .unwrap_or(1)
            .max(1) as u64;
        let text = context.state().text(TEXT_KEY)?.unwrap_or_default();

        Ok(Tree::root(BoxNode::column([
            Editor::new(DOCUMENT, DocumentRevision::new(document_revision), text),
            Text::new(STATUS, format!("Saved as revision {document_revision}")),
            Button::new(SAVE, "Save"),
        ])))
    }

    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        if !events.activated(SAVE) {
            return Ok(Update::unchanged());
        }

        let snapshot = context.editor().snapshot(DOCUMENT)?;
        let new_revision = DocumentRevision::new(snapshot.document_revision().get() + 1);
        context.editor().accept(
            DOCUMENT,
            snapshot.document_revision(),
            snapshot.edit_sequence(),
            new_revision,
        )?;

        context
            .state()
            .set_integer(DOCUMENT_REVISION_KEY, new_revision.get() as i64)?;
        context.state().set_text(TEXT_KEY, snapshot.text())?;

        Ok(Update::new().set_text(STATUS, format!("Saved as revision {}", new_revision.get())))
    }
}

youth_sdk::export_app!(EditorFixture);

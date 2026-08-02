//! Real `0.0.8` text-document fixture used by runtime and CLI tests.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

use youth_sdk::prelude::*;

use std::cell::Cell;

const DOCUMENT: NodeKey = node!("document");
const SAVE: NodeKey = node!("save");
const STATUS: NodeKey = node!("status");

struct TextDocumentFixture;

thread_local! {
    static STATUS_TEXT: Cell<&'static str> = const { Cell::new("Saved") };
}

impl Application for TextDocumentFixture {
    fn view(context: &ViewContext) -> Result<Tree> {
        let document = context.text_document().current()?;
        Ok(Tree::root(BoxNode::column([
            Text::new(STATUS, STATUS_TEXT.with(Cell::get)),
            Editor::document(DOCUMENT, &document),
            Button::new(SAVE, "Save").shortcut(Shortcut::primary('s')),
        ])))
    }

    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        let document = context.text_document().current()?;
        let mut update = Update::unchanged();

        if events
            .editor_dirty_changes()
            .any(|(editor, dirty)| editor == DOCUMENT.id() && dirty)
        {
            STATUS_TEXT.set("Unsaved changes");
            update = update.set_text(STATUS, "Unsaved changes");
        }

        if events.activated(SAVE) {
            context.text_document().request_save(&document, DOCUMENT)?;
            STATUS_TEXT.set("Saving...");
            update = update.set_text(STATUS, "Saving...");
        }

        for completion in events.text_document_save_completions() {
            update = match completion.outcome() {
                TextDocumentSaveOutcome::Saved {
                    document,
                    version,
                    still_dirty,
                    ..
                } => {
                    let status = if still_dirty {
                        "Unsaved changes"
                    } else {
                        "Saved"
                    };
                    STATUS_TEXT.set(status);
                    update
                        .set_editor_document_version(DOCUMENT, document, version)
                        .set_text(STATUS, status)
                }
                TextDocumentSaveOutcome::Failed(TextDocumentFailure::Conflict) => {
                    STATUS_TEXT.set("Conflict");
                    update.set_text(STATUS, "Conflict")
                }
                TextDocumentSaveOutcome::Failed(_) => {
                    STATUS_TEXT.set("Save failed");
                    update.set_text(STATUS, "Save failed")
                }
            };
        }

        Ok(update)
    }
}

youth_sdk::export_app!(TextDocumentFixture);

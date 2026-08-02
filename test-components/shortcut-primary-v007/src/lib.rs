//! Hand-written protocol 0.0.7 modifier-aware shortcut fixture.
//!
//! Exercises the new `shortcut` record (`key` + `modifiers`) introduced in
//! `youth:app@0.0.7`: a Save button declares a `Primary+Character("s")`
//! chord alongside a focused Editor, so the host's shortcut routing and the
//! wire-to-semantic-tree conversion for the new record both get a real
//! mount/activate round trip.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.7",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, ButtonData, EditorData, EventBatch, EventKind, Node, NodeData, Patch, PatchBatch,
    SetText, Shortcut, ShortcutKey, ShortcutModifiers, TextAlignment, TextContent, TextData,
    TreeSnapshot,
};

const EDITOR: u64 = 2;
const STATUS: u64 = 3;
const SAVE: u64 = 4;

struct ShortcutPrimaryFixture;

impl Guest for ShortcutPrimaryFixture {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot("idle"))
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        let activated = events.events.iter().find_map(|event| match event.kind {
            EventKind::Activate(id) => Some(id),
            EventKind::ScheduleElapsed(_) => None,
        });
        let base = events.tree_revision;
        let mut patches = Vec::new();
        if activated == Some(SAVE) {
            patches.push(Patch::SetText(SetText {
                id: STATUS,
                value: TextContent::Literal("saved".into()),
            }));
        }
        let next = if patches.is_empty() { base } else { base + 1 };
        Ok(PatchBatch {
            base_tree_revision: base,
            next_tree_revision: next,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches,
        })
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot("idle"))
    }
}

fn snapshot(status: &str) -> TreeSnapshot {
    TreeSnapshot {
        revision: 0,
        root: 1,
        nodes: vec![
            Node {
                id: 1,
                data: NodeData::Root,
                children: vec![EDITOR, STATUS, SAVE],
            },
            Node {
                id: EDITOR,
                data: NodeData::Editor(EditorData {
                    document_revision: 1,
                    text: "draft".into(),
                }),
                children: Vec::new(),
            },
            Node {
                id: STATUS,
                data: NodeData::Text(TextData {
                    content: TextContent::Literal(status.into()),
                    alignment: TextAlignment::Start,
                }),
                children: Vec::new(),
            },
            Node {
                id: SAVE,
                data: NodeData::Button(ButtonData {
                    label: "Save".into(),
                    enabled: true,
                    shortcuts: vec![Shortcut {
                        key: ShortcutKey::Character("s".into()),
                        modifiers: ShortcutModifiers::PRIMARY,
                    }],
                }),
                children: Vec::new(),
            },
        ],
    }
}

export!(ShortcutPrimaryFixture);

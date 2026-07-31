//! Hand-written protocol 0.0.6 Editor capability fixture.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.6",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, ButtonData, EditorData, EventBatch, EventKind, Node, NodeData, Patch, PatchBatch,
    SetText, TextAlignment, TextContent, TextData, TreeSnapshot,
};
use youth::editor::session::{self, EditorErrorCode, EditorSnapshot};

const EDITOR: u64 = 2;
const STATUS: u64 = 3;
const SNAPSHOT: u64 = 4;
const ACCEPT: u64 = 5;
const ACCEPT_STALE_REVISION: u64 = 6;
const ACCEPT_STALE_SEQUENCE: u64 = 7;
const REPLACE: u64 = 8;
const REPLACE_STALE_REVISION: u64 = 9;
const REPLACE_STALE_SEQUENCE: u64 = 10;
const UNKNOWN: u64 = 11;
const ACCEPT_THEN_TRAP: u64 = 12;
const ACCEPT_CURRENT: u64 = 13;
const REPLACE_CURRENT: u64 = 14;

struct EditorCapabilityFixture;

impl Guest for EditorCapabilityFixture {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        let activated = events
            .events
            .iter()
            .find_map(|event| match event.kind {
                EventKind::Activate(id) => Some(id),
                EventKind::ScheduleElapsed(_) => None,
            })
            .unwrap_or(0);
        let status = match activated {
            SNAPSHOT => describe(snapshot_editor()),
            ACCEPT => {
                session::accept(EDITOR, 42, 0, 43).expect("matching accept succeeds");
                describe(snapshot_editor())
            }
            ACCEPT_STALE_REVISION => {
                assert_error(
                    session::accept(EDITOR, 41, 0, 43),
                    EditorErrorCode::StaleDocumentRevision,
                );
                describe(snapshot_editor())
            }
            ACCEPT_STALE_SEQUENCE => {
                assert_error(
                    session::accept(EDITOR, 42, 1, 43),
                    EditorErrorCode::StaleEditSequence,
                );
                describe(snapshot_editor())
            }
            REPLACE => {
                session::replace(EDITOR, 42, 0, 50, "Authoritative text")
                    .expect("matching replace succeeds");
                describe(snapshot_editor())
            }
            REPLACE_STALE_REVISION => {
                assert_error(
                    session::replace(EDITOR, 41, 0, 50, "Must not install"),
                    EditorErrorCode::StaleDocumentRevision,
                );
                describe(snapshot_editor())
            }
            REPLACE_STALE_SEQUENCE => {
                assert_error(
                    session::replace(EDITOR, 42, 1, 50, "Must not install"),
                    EditorErrorCode::StaleEditSequence,
                );
                describe(snapshot_editor())
            }
            UNKNOWN => {
                assert_error(session::snapshot(999), EditorErrorCode::UnknownEditor);
                assert_error(
                    session::accept(999, 42, 0, 43),
                    EditorErrorCode::UnknownEditor,
                );
                assert_error(
                    session::replace(999, 42, 0, 50, "Unknown"),
                    EditorErrorCode::UnknownEditor,
                );
                "unknown-editor".to_owned()
            }
            ACCEPT_THEN_TRAP => {
                session::accept(EDITOR, 42, 0, 99).expect("accept stages before trap");
                panic!("intentional trap after staged Editor accept");
            }
            ACCEPT_CURRENT => {
                let current = snapshot_editor();
                session::accept(
                    EDITOR,
                    current.document_revision,
                    current.edit_sequence,
                    current.document_revision + 1,
                )
                .expect("current session accept succeeds");
                describe(snapshot_editor())
            }
            REPLACE_CURRENT => {
                let current = snapshot_editor();
                session::replace(
                    EDITOR,
                    current.document_revision,
                    current.edit_sequence,
                    60,
                    "Current authoritative text",
                )
                .expect("current session replace succeeds");
                describe(snapshot_editor())
            }
            _ => "ignored".to_owned(),
        };

        let base = events.tree_revision;
        Ok(PatchBatch {
            base_tree_revision: base,
            next_tree_revision: base + 1,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches: vec![Patch::SetText(SetText {
                id: STATUS,
                value: TextContent::Literal(status),
            })],
        })
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }
}

fn snapshot_editor() -> EditorSnapshot {
    session::snapshot(EDITOR).expect("known Editor snapshots")
}

fn describe(snapshot: EditorSnapshot) -> String {
    format!(
        "{}|{}|{}",
        snapshot.document_revision, snapshot.edit_sequence, snapshot.text
    )
}

fn assert_error<T>(result: Result<T, EditorErrorCode>, expected: EditorErrorCode) {
    match result {
        Err(error) => assert_eq!(error, expected),
        Ok(_) => panic!("capability call must fail"),
    }
}

fn snapshot() -> TreeSnapshot {
    let actions = [
        (SNAPSHOT, "Snapshot"),
        (ACCEPT, "Accept"),
        (ACCEPT_STALE_REVISION, "Accept stale revision"),
        (ACCEPT_STALE_SEQUENCE, "Accept stale sequence"),
        (REPLACE, "Replace"),
        (REPLACE_STALE_REVISION, "Replace stale revision"),
        (REPLACE_STALE_SEQUENCE, "Replace stale sequence"),
        (UNKNOWN, "Unknown"),
        (ACCEPT_THEN_TRAP, "Accept then trap"),
        (ACCEPT_CURRENT, "Accept current"),
        (REPLACE_CURRENT, "Replace current"),
    ];
    let mut nodes = vec![
        Node {
            id: 1,
            data: NodeData::Root,
            children: std::iter::once(EDITOR)
                .chain(std::iter::once(STATUS))
                .chain(actions.iter().map(|(id, _)| *id))
                .collect(),
        },
        Node {
            id: EDITOR,
            data: NodeData::Editor(EditorData {
                document_revision: 42,
                text: "Scratchpad draft".into(),
            }),
            children: Vec::new(),
        },
        Node {
            id: STATUS,
            data: NodeData::Text(TextData {
                content: TextContent::Literal("ready".into()),
                alignment: TextAlignment::Start,
            }),
            children: Vec::new(),
        },
    ];
    nodes.extend(actions.into_iter().map(|(id, label)| Node {
        id,
        data: NodeData::Button(ButtonData {
            label: label.into(),
            enabled: true,
            shortcuts: Vec::new(),
        }),
        children: Vec::new(),
    }));
    TreeSnapshot {
        revision: 0,
        root: 1,
        nodes,
    }
}

export!(EditorCapabilityFixture);

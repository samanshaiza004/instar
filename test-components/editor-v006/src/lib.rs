//! Hand-written protocol 0.0.6 Editor-node mounting fixture.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.6",
});

use exports::youth::app::lifecycle::Guest;
use std::cell::Cell;
use youth::app::ui::{AppError, EditorData, EventBatch, Node, NodeData, PatchBatch, TreeSnapshot};

const DOCUMENT_REVISION: u64 = 42;
const TEXT: &str = "Scratchpad draft";

thread_local! {
    static RESYNC_COUNT: Cell<u32> = const { Cell::new(0) };
}

struct EditorFixture;

impl Guest for EditorFixture {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot(true, false, DOCUMENT_REVISION, TEXT))
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        Ok(PatchBatch {
            base_tree_revision: events.tree_revision,
            next_tree_revision: events.tree_revision,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches: Vec::new(),
        })
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        let call = RESYNC_COUNT.with(|count| {
            let call = count.get();
            count.set(call + 1);
            call
        });
        Ok(match call {
            0 => snapshot(true, false, DOCUMENT_REVISION, TEXT),
            1 => snapshot(false, false, DOCUMENT_REVISION, TEXT),
            2 => snapshot(true, true, DOCUMENT_REVISION, TEXT),
            _ => snapshot(true, true, 99, "Out-of-band replacement"),
        })
    }
}

fn snapshot(
    include_primary: bool,
    include_independent: bool,
    primary_revision: u64,
    primary_text: &str,
) -> TreeSnapshot {
    let mut children = Vec::new();
    let mut nodes = vec![Node {
        id: 1,
        data: NodeData::Root,
        children: Vec::new(),
    }];
    if include_primary {
        children.push(2);
        nodes.push(Node {
            id: 2,
            data: NodeData::Editor(EditorData {
                document_revision: primary_revision,
                text: primary_text.into(),
            }),
            children: Vec::new(),
        });
    }
    if include_independent {
        children.push(3);
        nodes.push(Node {
            id: 3,
            data: NodeData::Editor(EditorData {
                document_revision: 7,
                text: "Independent document".into(),
            }),
            children: Vec::new(),
        });
    }
    nodes[0].children = children;

    TreeSnapshot {
        revision: 0,
        root: 1,
        nodes,
    }
}

export!(EditorFixture);

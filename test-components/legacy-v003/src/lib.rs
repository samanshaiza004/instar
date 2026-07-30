//! Hand-written protocol 0.0.3 compatibility fixture.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.3",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, BoxData, BoxLayout, EventBatch, Node, NodeData, PatchBatch, TextAlignment, TextData,
    TreeSnapshot,
};

struct Legacy;

impl Guest for Legacy {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
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
        Ok(snapshot())
    }
}

fn snapshot() -> TreeSnapshot {
    TreeSnapshot {
        revision: 0,
        root: 1,
        nodes: vec![
            Node {
                id: 1,
                data: NodeData::Root,
                children: vec![2],
            },
            Node {
                id: 2,
                data: NodeData::Box(BoxData {
                    enabled: true,
                    layout: BoxLayout::Column,
                }),
                children: vec![3],
            },
            Node {
                id: 3,
                data: NodeData::Text(TextData {
                    value: "legacy 0.0.3".into(),
                    alignment: TextAlignment::Start,
                }),
                children: Vec::new(),
            },
        ],
    }
}

export!(Legacy);

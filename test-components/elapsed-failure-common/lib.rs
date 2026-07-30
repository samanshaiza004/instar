//! Shared raw-protocol fixtures for elapsed delivery failures.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.4",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, EventBatch, EventKind, Node, NodeData, Patch, PatchBatch, SetText, TextAlignment,
    TextData, TreeSnapshot,
};

struct ElapsedFailure;

impl Guest for ElapsedFailure {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        assert!(matches!(
            events.events.first().map(|event| &event.kind),
            Some(EventKind::ScheduleElapsed(_))
        ));
        if env!("CARGO_PKG_NAME") == "youth-elapsed-trap" {
            panic!("intentional elapsed-delivery trap");
        }
        Ok(PatchBatch {
            base_tree_revision: events.tree_revision,
            next_tree_revision: events.tree_revision + 1,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches: vec![Patch::SetText(SetText {
                id: 999,
                value: "invalid".into(),
            })],
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
                data: NodeData::Text(TextData {
                    value: "ready".into(),
                    alignment: TextAlignment::Start,
                }),
                children: Vec::new(),
            },
        ],
    }
}

export!(ElapsedFailure);

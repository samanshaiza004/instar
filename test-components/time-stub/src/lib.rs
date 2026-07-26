//! Protocol 0.0.4 fixture that verifies the scheduling capability stub.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.4",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, BoxLayout, EventBatch, Node, NodeData, Patch, PatchBatch,
    SetText, TextAlignment, TextData, TreeSnapshot,
};
use youth::time::scheduler::{ScheduleErrorCode, ScheduleOptions};

struct TimeStub;

impl Guest for TimeStub {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot("ready"))
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        let value = match youth::time::scheduler::schedule_after(
            1_000,
            &ScheduleOptions { notification: None },
        ) {
            Err(ScheduleErrorCode::Unavailable) => "unavailable",
            _ => return Err(error(AppErrorCode::Internal)),
        };
        Ok(PatchBatch {
            base_tree_revision: events.tree_revision,
            next_tree_revision: events.tree_revision + 1,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches: vec![Patch::SetText(SetText {
                id: 3,
                value: value.into(),
            })],
        })
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot("ready"))
    }
}

fn snapshot(value: &str) -> TreeSnapshot {
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
                    value: value.into(),
                    alignment: TextAlignment::Start,
                }),
                children: Vec::new(),
            },
        ],
    }
}

const fn error(code: AppErrorCode) -> AppError {
    AppError {
        code,
        message: None,
    }
}

export!(TimeStub);

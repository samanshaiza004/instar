//! Protocol 0.0.4 fixture for schedule success and transactional rollback.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app-v0.0.4",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, BoxLayout, ButtonData, EventBatch, EventKind, Node, NodeData,
    Patch, PatchBatch, SetText, TextAlignment, TextData, TreeSnapshot,
};
use youth::time::scheduler::ScheduleOptions;

struct TimeStub;

impl Guest for TimeStub {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot("ready"))
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        youth::time::scheduler::schedule_after(1_000, &ScheduleOptions { notification: None })
            .map_err(|_| error(AppErrorCode::Internal))?;
        let activated = events.events.last().and_then(|event| match event.kind {
            EventKind::Activate(id) => Some(id),
            EventKind::ScheduleElapsed(_) => None,
        });
        if activated == Some(5) {
            panic!("intentional trap after schedule creation");
        }
        let target = if activated == Some(6) { 4 } else { 3 };
        Ok(PatchBatch {
            base_tree_revision: events.tree_revision,
            next_tree_revision: events.tree_revision + 1,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches: vec![Patch::SetText(SetText {
                id: target,
                value: "scheduled".into(),
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
                children: vec![3, 4, 5, 6],
            },
            Node {
                id: 3,
                data: NodeData::Text(TextData {
                    value: value.into(),
                    alignment: TextAlignment::Start,
                }),
                children: Vec::new(),
            },
            button(4, "Schedule"),
            button(5, "Schedule then trap"),
            button(6, "Schedule then return an invalid patch"),
        ],
    }
}

fn button(id: u64, label: &str) -> Node {
    Node {
        id,
        data: NodeData::Button(ButtonData {
            label: label.into(),
            enabled: true,
            shortcuts: Vec::new(),
        }),
        children: Vec::new(),
    }
}

const fn error(code: AppErrorCode) -> AppError {
    AppError {
        code,
        message: None,
    }
}

export!(TimeStub);

use std::fmt;

use crate::RuntimeLimits;
use crate::bindings::v002::youth::app::ui as generated;
use crate::bindings::v003::youth::app::ui as generated_v003;
use crate::bindings::v004::youth::app::ui as generated_v004;
use crate::bindings::v005::youth::app::ui as generated_v005;
use crate::bindings::v006::youth::app::ui as generated_v006;
use crate::bindings::{RawPatchBatch, RawTreeSnapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireErrorKind {
    InvalidValue,
    TransferLimit,
}

#[derive(Debug)]
pub struct WireError {
    pub kind: WireErrorKind,
    message: String,
}

impl WireError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: WireErrorKind::InvalidValue,
            message: message.into(),
        }
    }

    fn transfer(max: usize) -> Self {
        Self {
            kind: WireErrorKind::TransferLimit,
            message: format!("guest result exceeds the {max}-byte transfer limit"),
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WireError {}

struct TransferBudget {
    remaining: usize,
    maximum: usize,
}

impl TransferBudget {
    fn new(maximum: usize) -> Self {
        Self {
            remaining: maximum,
            maximum,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), WireError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| WireError::transfer(self.maximum))?;
        Ok(())
    }

    fn charge_list(&mut self, len: usize, element_size: usize) -> Result<(), WireError> {
        let bytes = len
            .checked_mul(element_size)
            .ok_or_else(|| WireError::transfer(self.maximum))?;
        self.charge(bytes)
    }
}

fn node_id(value: u64) -> Result<youth_tree::NodeId, WireError> {
    youth_tree::NodeId::new(value)
        .ok_or_else(|| WireError::invalid("node ID 0 is reserved and invalid"))
}

pub(crate) fn tree_snapshot(
    value: RawTreeSnapshot,
    limits: &RuntimeLimits,
) -> Result<youth_tree::TreeSnapshot, WireError> {
    match value {
        RawTreeSnapshot::V002(value) => tree_snapshot_v002(value, limits),
        RawTreeSnapshot::V003(value) => tree_snapshot_v003(value, limits),
        RawTreeSnapshot::V004(value) => tree_snapshot_v004(value, limits),
        RawTreeSnapshot::V005(value) => tree_snapshot_v005(value, limits),
        RawTreeSnapshot::V006(value) => tree_snapshot_v006(value, limits),
    }
}

fn tree_snapshot_v002(
    value: generated::TreeSnapshot,
    limits: &RuntimeLimits,
) -> Result<youth_tree::TreeSnapshot, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(16)?;
    budget.charge_list(value.nodes.len(), 32)?;
    let root = node_id(value.root)?;
    let mut nodes = Vec::with_capacity(value.nodes.len());
    for node in value.nodes {
        nodes.push(convert_node(node, limits, &mut budget)?);
    }
    Ok(youth_tree::TreeSnapshot {
        revision: value.revision,
        root,
        nodes,
    })
}

fn tree_snapshot_v003(
    value: generated_v003::TreeSnapshot,
    limits: &RuntimeLimits,
) -> Result<youth_tree::TreeSnapshot, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(16)?;
    budget.charge_list(value.nodes.len(), 32)?;
    let root = node_id(value.root)?;
    let nodes = value
        .nodes
        .into_iter()
        .map(|node| convert_node_v003(node, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::TreeSnapshot {
        revision: value.revision,
        root,
        nodes,
    })
}

fn tree_snapshot_v004(
    value: generated_v004::TreeSnapshot,
    limits: &RuntimeLimits,
) -> Result<youth_tree::TreeSnapshot, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(16)?;
    budget.charge_list(value.nodes.len(), 32)?;
    let root = node_id(value.root)?;
    let nodes = value
        .nodes
        .into_iter()
        .map(|node| convert_node_v004(node, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::TreeSnapshot {
        revision: value.revision,
        root,
        nodes,
    })
}

fn tree_snapshot_v005(
    value: generated_v005::TreeSnapshot,
    limits: &RuntimeLimits,
) -> Result<youth_tree::TreeSnapshot, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(16)?;
    budget.charge_list(value.nodes.len(), 32)?;
    let root = node_id(value.root)?;
    let nodes = value
        .nodes
        .into_iter()
        .map(|node| convert_node_v005(node, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::TreeSnapshot {
        revision: value.revision,
        root,
        nodes,
    })
}

fn tree_snapshot_v006(
    value: generated_v006::TreeSnapshot,
    limits: &RuntimeLimits,
) -> Result<youth_tree::TreeSnapshot, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(16)?;
    budget.charge_list(value.nodes.len(), 32)?;
    let root = node_id(value.root)?;
    let nodes = value
        .nodes
        .into_iter()
        .map(|node| convert_node_v006(node, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::TreeSnapshot {
        revision: value.revision,
        root,
        nodes,
    })
}

fn convert_node(
    value: generated::Node,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Node, WireError> {
    budget.charge_list(value.children.len(), size_of::<u64>())?;
    let children = value
        .children
        .into_iter()
        .map(node_id)
        .collect::<Result<Vec<_>, _>>()?;
    let data = match value.data {
        generated::NodeData::Root => youth_tree::NodeData::Root,
        generated::NodeData::Box(value) => youth_tree::NodeData::Box {
            enabled: value.enabled,
        },
        generated::NodeData::Text(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(format!(
                    "text value has {} bytes, exceeding the limit of {}",
                    value.value.len(),
                    limits.tree.max_text_len
                )));
            }
            youth_tree::NodeData::Text { value: value.value }
        }
        generated::NodeData::Button(value) => {
            budget.charge(value.label.len())?;
            if value.label.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(format!(
                    "button label has {} bytes, exceeding the limit of {}",
                    value.label.len(),
                    limits.tree.max_label_len
                )));
            }
            youth_tree::NodeData::Button {
                label: value.label,
                enabled: value.enabled,
            }
        }
    };
    Ok(youth_tree::Node {
        id: node_id(value.id)?,
        data,
        children,
    })
}

fn convert_node_v003(
    value: generated_v003::Node,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Node, WireError> {
    use youth_tree::{NodeData, ShortcutKey, TextAlignment};

    budget.charge_list(value.children.len(), size_of::<u64>())?;
    let children = value
        .children
        .into_iter()
        .map(node_id)
        .collect::<Result<Vec<_>, _>>()?;
    let data = match value.data {
        generated_v003::NodeData::Root => NodeData::Root,
        generated_v003::NodeData::Box(value) => match value.layout {
            generated_v003::BoxLayout::Column => NodeData::Box {
                enabled: value.enabled,
            },
            generated_v003::BoxLayout::Row => NodeData::Row {
                enabled: value.enabled,
            },
            generated_v003::BoxLayout::Grid(grid) => NodeData::Grid {
                enabled: value.enabled,
                columns: grid.columns,
            },
        },
        generated_v003::NodeData::Text(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(format!(
                    "text value has {} bytes, exceeding the limit of {}",
                    value.value.len(),
                    limits.tree.max_text_len
                )));
            }
            match value.alignment {
                generated_v003::TextAlignment::Start => NodeData::Text { value: value.value },
                generated_v003::TextAlignment::Center => NodeData::AlignedText {
                    value: value.value,
                    alignment: TextAlignment::Center,
                },
                generated_v003::TextAlignment::End => NodeData::AlignedText {
                    value: value.value,
                    alignment: TextAlignment::End,
                },
            }
        }
        generated_v003::NodeData::Button(value) => {
            budget.charge(value.label.len())?;
            if value.label.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(format!(
                    "button label has {} bytes, exceeding the limit of {}",
                    value.label.len(),
                    limits.tree.max_label_len
                )));
            }
            budget.charge_list(value.shortcuts.len(), 16)?;
            let shortcuts = value
                .shortcuts
                .into_iter()
                .map(|shortcut| match shortcut {
                    generated_v003::ShortcutKey::Character(value) => {
                        budget.charge(value.len())?;
                        Ok(ShortcutKey::Character(value))
                    }
                    generated_v003::ShortcutKey::Enter => Ok(ShortcutKey::Enter),
                    generated_v003::ShortcutKey::Escape => Ok(ShortcutKey::Escape),
                    generated_v003::ShortcutKey::Backspace => Ok(ShortcutKey::Backspace),
                })
                .collect::<Result<Vec<_>, WireError>>()?;
            if shortcuts.is_empty() {
                NodeData::Button {
                    label: value.label,
                    enabled: value.enabled,
                }
            } else {
                NodeData::ShortcutButton {
                    label: value.label,
                    enabled: value.enabled,
                    shortcuts,
                }
            }
        }
    };
    Ok(youth_tree::Node {
        id: node_id(value.id)?,
        data,
        children,
    })
}

fn convert_node_v004(
    value: generated_v004::Node,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Node, WireError> {
    use youth_tree::{NodeData, ShortcutKey, TextAlignment};

    budget.charge_list(value.children.len(), size_of::<u64>())?;
    let children = value
        .children
        .into_iter()
        .map(node_id)
        .collect::<Result<Vec<_>, _>>()?;
    let data = match value.data {
        generated_v004::NodeData::Root => NodeData::Root,
        generated_v004::NodeData::Box(value) => match value.layout {
            generated_v004::BoxLayout::Column => NodeData::Box {
                enabled: value.enabled,
            },
            generated_v004::BoxLayout::Row => NodeData::Row {
                enabled: value.enabled,
            },
            generated_v004::BoxLayout::Grid(grid) => NodeData::Grid {
                enabled: value.enabled,
                columns: grid.columns,
            },
        },
        generated_v004::NodeData::Text(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(format!(
                    "text value has {} bytes, exceeding the limit of {}",
                    value.value.len(),
                    limits.tree.max_text_len
                )));
            }
            match value.alignment {
                generated_v004::TextAlignment::Start => NodeData::Text { value: value.value },
                generated_v004::TextAlignment::Center => NodeData::AlignedText {
                    value: value.value,
                    alignment: TextAlignment::Center,
                },
                generated_v004::TextAlignment::End => NodeData::AlignedText {
                    value: value.value,
                    alignment: TextAlignment::End,
                },
            }
        }
        generated_v004::NodeData::Button(value) => {
            budget.charge(value.label.len())?;
            if value.label.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(format!(
                    "button label has {} bytes, exceeding the limit of {}",
                    value.label.len(),
                    limits.tree.max_label_len
                )));
            }
            budget.charge_list(value.shortcuts.len(), 16)?;
            let shortcuts = value
                .shortcuts
                .into_iter()
                .map(|shortcut| match shortcut {
                    generated_v004::ShortcutKey::Character(value) => {
                        budget.charge(value.len())?;
                        Ok(ShortcutKey::Character(value))
                    }
                    generated_v004::ShortcutKey::Enter => Ok(ShortcutKey::Enter),
                    generated_v004::ShortcutKey::Escape => Ok(ShortcutKey::Escape),
                    generated_v004::ShortcutKey::Backspace => Ok(ShortcutKey::Backspace),
                })
                .collect::<Result<Vec<_>, WireError>>()?;
            if shortcuts.is_empty() {
                NodeData::Button {
                    label: value.label,
                    enabled: value.enabled,
                }
            } else {
                NodeData::ShortcutButton {
                    label: value.label,
                    enabled: value.enabled,
                    shortcuts,
                }
            }
        }
    };
    Ok(youth_tree::Node {
        id: node_id(value.id)?,
        data,
        children,
    })
}

fn convert_node_v005(
    value: generated_v005::Node,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Node, WireError> {
    use youth_tree::{NodeData, ShortcutKey};

    budget.charge_list(value.children.len(), size_of::<u64>())?;
    let children = value
        .children
        .into_iter()
        .map(node_id)
        .collect::<Result<Vec<_>, _>>()?;
    let data = match value.data {
        generated_v005::NodeData::Root => NodeData::Root,
        generated_v005::NodeData::Box(value) => match value.layout {
            generated_v005::BoxLayout::Column => NodeData::Box {
                enabled: value.enabled,
            },
            generated_v005::BoxLayout::Row => NodeData::Row {
                enabled: value.enabled,
            },
            generated_v005::BoxLayout::Grid(grid) => NodeData::Grid {
                enabled: value.enabled,
                columns: grid.columns,
            },
        },
        generated_v005::NodeData::Text(value) => {
            convert_text_content_v005(value.content, value.alignment, limits, budget)?
        }
        generated_v005::NodeData::Button(value) => {
            budget.charge(value.label.len())?;
            if value.label.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(format!(
                    "button label has {} bytes, exceeding the limit of {}",
                    value.label.len(),
                    limits.tree.max_label_len
                )));
            }
            budget.charge_list(value.shortcuts.len(), 16)?;
            let shortcuts = value
                .shortcuts
                .into_iter()
                .map(|shortcut| match shortcut {
                    generated_v005::ShortcutKey::Character(value) => {
                        budget.charge(value.len())?;
                        Ok(ShortcutKey::Character(value))
                    }
                    generated_v005::ShortcutKey::Enter => Ok(ShortcutKey::Enter),
                    generated_v005::ShortcutKey::Escape => Ok(ShortcutKey::Escape),
                    generated_v005::ShortcutKey::Backspace => Ok(ShortcutKey::Backspace),
                })
                .collect::<Result<Vec<_>, WireError>>()?;
            if shortcuts.is_empty() {
                NodeData::Button {
                    label: value.label,
                    enabled: value.enabled,
                }
            } else {
                NodeData::ShortcutButton {
                    label: value.label,
                    enabled: value.enabled,
                    shortcuts,
                }
            }
        }
    };
    Ok(youth_tree::Node {
        id: node_id(value.id)?,
        data,
        children,
    })
}

fn convert_text_content_v005(
    content: generated_v005::TextContent,
    alignment: generated_v005::TextAlignment,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::NodeData, WireError> {
    use youth_tree::{NodeData, TextAlignment};

    match content {
        generated_v005::TextContent::Literal(value) => {
            budget.charge(value.len())?;
            if value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(format!(
                    "text value has {} bytes, exceeding the limit of {}",
                    value.len(),
                    limits.tree.max_text_len
                )));
            }
            Ok(match alignment {
                generated_v005::TextAlignment::Start => NodeData::Text { value },
                generated_v005::TextAlignment::Center => NodeData::AlignedText {
                    value,
                    alignment: TextAlignment::Center,
                },
                generated_v005::TextAlignment::End => NodeData::AlignedText {
                    value,
                    alignment: TextAlignment::End,
                },
            })
        }
        generated_v005::TextContent::Countdown(value) => {
            budget.charge(24)?;
            let (schedule, precision, format) = convert_countdown_v005(value);
            Ok(match alignment {
                generated_v005::TextAlignment::Start => NodeData::Countdown {
                    schedule,
                    precision,
                    format,
                },
                generated_v005::TextAlignment::Center => NodeData::AlignedCountdown {
                    schedule,
                    precision,
                    format,
                    alignment: TextAlignment::Center,
                },
                generated_v005::TextAlignment::End => NodeData::AlignedCountdown {
                    schedule,
                    precision,
                    format,
                    alignment: TextAlignment::End,
                },
            })
        }
    }
}

fn convert_countdown_v005(
    value: generated_v005::CountdownData,
) -> (
    youth_tree::ScheduleRef,
    youth_tree::TimePrecision,
    youth_tree::CountdownFormat,
) {
    let schedule = youth_tree::ScheduleRef {
        id: value.schedule.id,
        generation: value.schedule.generation,
    };
    let precision = match value.precision {
        generated_v005::TimePrecision::Seconds => youth_tree::TimePrecision::Seconds,
    };
    let format = match value.format {
        generated_v005::CountdownFormat::MinutesSeconds => {
            youth_tree::CountdownFormat::MinutesSeconds
        }
    };
    (schedule, precision, format)
}

fn convert_node_v006(
    value: generated_v006::Node,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Node, WireError> {
    use youth_tree::{NodeData, ShortcutKey};

    budget.charge_list(value.children.len(), size_of::<u64>())?;
    let children = value
        .children
        .into_iter()
        .map(node_id)
        .collect::<Result<Vec<_>, _>>()?;
    let data = match value.data {
        generated_v006::NodeData::Root => NodeData::Root,
        generated_v006::NodeData::Box(value) => match value.layout {
            generated_v006::BoxLayout::Column => NodeData::Box {
                enabled: value.enabled,
            },
            generated_v006::BoxLayout::Row => NodeData::Row {
                enabled: value.enabled,
            },
            generated_v006::BoxLayout::Grid(grid) => NodeData::Grid {
                enabled: value.enabled,
                columns: grid.columns,
            },
        },
        generated_v006::NodeData::Text(value) => {
            convert_text_content_v006(value.content, value.alignment, limits, budget)?
        }
        generated_v006::NodeData::Editor(value) => {
            budget.charge(value.text.len())?;
            if value.text.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(format!(
                    "editor text has {} bytes, exceeding the limit of {}",
                    value.text.len(),
                    limits.tree.max_text_len
                )));
            }
            NodeData::Editor {
                document_revision: value.document_revision,
                text: value.text,
            }
        }
        generated_v006::NodeData::Button(value) => {
            budget.charge(value.label.len())?;
            if value.label.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(format!(
                    "button label has {} bytes, exceeding the limit of {}",
                    value.label.len(),
                    limits.tree.max_label_len
                )));
            }
            budget.charge_list(value.shortcuts.len(), 16)?;
            let shortcuts = value
                .shortcuts
                .into_iter()
                .map(|shortcut| match shortcut {
                    generated_v006::ShortcutKey::Character(value) => {
                        budget.charge(value.len())?;
                        Ok(ShortcutKey::Character(value))
                    }
                    generated_v006::ShortcutKey::Enter => Ok(ShortcutKey::Enter),
                    generated_v006::ShortcutKey::Escape => Ok(ShortcutKey::Escape),
                    generated_v006::ShortcutKey::Backspace => Ok(ShortcutKey::Backspace),
                })
                .collect::<Result<Vec<_>, WireError>>()?;
            if shortcuts.is_empty() {
                NodeData::Button {
                    label: value.label,
                    enabled: value.enabled,
                }
            } else {
                NodeData::ShortcutButton {
                    label: value.label,
                    enabled: value.enabled,
                    shortcuts,
                }
            }
        }
    };
    Ok(youth_tree::Node {
        id: node_id(value.id)?,
        data,
        children,
    })
}

fn convert_text_content_v006(
    content: generated_v006::TextContent,
    alignment: generated_v006::TextAlignment,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::NodeData, WireError> {
    use youth_tree::{NodeData, TextAlignment};

    match content {
        generated_v006::TextContent::Literal(value) => {
            budget.charge(value.len())?;
            if value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(format!(
                    "text value has {} bytes, exceeding the limit of {}",
                    value.len(),
                    limits.tree.max_text_len
                )));
            }
            Ok(match alignment {
                generated_v006::TextAlignment::Start => NodeData::Text { value },
                generated_v006::TextAlignment::Center => NodeData::AlignedText {
                    value,
                    alignment: TextAlignment::Center,
                },
                generated_v006::TextAlignment::End => NodeData::AlignedText {
                    value,
                    alignment: TextAlignment::End,
                },
            })
        }
        generated_v006::TextContent::Countdown(value) => {
            budget.charge(24)?;
            let (schedule, precision, format) = convert_countdown_v006(value);
            Ok(match alignment {
                generated_v006::TextAlignment::Start => NodeData::Countdown {
                    schedule,
                    precision,
                    format,
                },
                generated_v006::TextAlignment::Center => NodeData::AlignedCountdown {
                    schedule,
                    precision,
                    format,
                    alignment: TextAlignment::Center,
                },
                generated_v006::TextAlignment::End => NodeData::AlignedCountdown {
                    schedule,
                    precision,
                    format,
                    alignment: TextAlignment::End,
                },
            })
        }
    }
}

fn convert_countdown_v006(
    value: generated_v006::CountdownData,
) -> (
    youth_tree::ScheduleRef,
    youth_tree::TimePrecision,
    youth_tree::CountdownFormat,
) {
    let schedule = youth_tree::ScheduleRef {
        id: value.schedule.id,
        generation: value.schedule.generation,
    };
    let precision = match value.precision {
        generated_v006::TimePrecision::Seconds => youth_tree::TimePrecision::Seconds,
    };
    let format = match value.format {
        generated_v006::CountdownFormat::MinutesSeconds => {
            youth_tree::CountdownFormat::MinutesSeconds
        }
    };
    (schedule, precision, format)
}

impl TryFrom<generated::TreeSnapshot> for youth_tree::TreeSnapshot {
    type Error = WireError;

    fn try_from(value: generated::TreeSnapshot) -> Result<Self, Self::Error> {
        tree_snapshot_v002(value, &RuntimeLimits::default())
    }
}

pub(crate) fn patch_batch(
    value: RawPatchBatch,
    limits: &RuntimeLimits,
) -> Result<youth_tree::PatchBatch, WireError> {
    match value {
        RawPatchBatch::V002(value) => patch_batch_v002(value, limits),
        RawPatchBatch::V003(value) => patch_batch_v003(value, limits),
        RawPatchBatch::V004(value) => patch_batch_v004(value, limits),
        RawPatchBatch::V005(value) => patch_batch_v005(value, limits),
        RawPatchBatch::V006(value) => patch_batch_v006(value, limits),
    }
}

fn patch_batch_v002(
    value: generated::PatchBatch,
    limits: &RuntimeLimits,
) -> Result<youth_tree::PatchBatch, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(24)?;
    budget.charge_list(value.patches.len(), 32)?;
    if value.patches.len() > limits.tree.max_patches {
        return Err(WireError::invalid(
            "patch list exceeds the configured limit",
        ));
    }
    let patches = value
        .patches
        .into_iter()
        .map(|patch| convert_patch(patch, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::PatchBatch {
        base_revision: value.base_tree_revision,
        next_revision: value.next_tree_revision,
        patches,
    })
}

fn patch_batch_v003(
    value: generated_v003::PatchBatch,
    limits: &RuntimeLimits,
) -> Result<youth_tree::PatchBatch, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(24)?;
    budget.charge_list(value.patches.len(), 32)?;
    if value.patches.len() > limits.tree.max_patches {
        return Err(WireError::invalid(
            "patch list exceeds the configured limit",
        ));
    }
    let patches = value
        .patches
        .into_iter()
        .map(|patch| convert_patch_v003(patch, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::PatchBatch {
        base_revision: value.base_tree_revision,
        next_revision: value.next_tree_revision,
        patches,
    })
}

fn patch_batch_v004(
    value: generated_v004::PatchBatch,
    limits: &RuntimeLimits,
) -> Result<youth_tree::PatchBatch, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(24)?;
    budget.charge_list(value.patches.len(), 32)?;
    if value.patches.len() > limits.tree.max_patches {
        return Err(WireError::invalid(
            "patch list exceeds the configured limit",
        ));
    }
    let patches = value
        .patches
        .into_iter()
        .map(|patch| convert_patch_v004(patch, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::PatchBatch {
        base_revision: value.base_tree_revision,
        next_revision: value.next_tree_revision,
        patches,
    })
}

fn patch_batch_v005(
    value: generated_v005::PatchBatch,
    limits: &RuntimeLimits,
) -> Result<youth_tree::PatchBatch, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(24)?;
    budget.charge_list(value.patches.len(), 32)?;
    if value.patches.len() > limits.tree.max_patches {
        return Err(WireError::invalid(
            "patch list exceeds the configured limit",
        ));
    }
    let patches = value
        .patches
        .into_iter()
        .map(|patch| convert_patch_v005(patch, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::PatchBatch {
        base_revision: value.base_tree_revision,
        next_revision: value.next_tree_revision,
        patches,
    })
}

fn patch_batch_v006(
    value: generated_v006::PatchBatch,
    limits: &RuntimeLimits,
) -> Result<youth_tree::PatchBatch, WireError> {
    let mut budget = TransferBudget::new(limits.max_guest_to_host_transfer);
    budget.charge(24)?;
    budget.charge_list(value.patches.len(), 32)?;
    if value.patches.len() > limits.tree.max_patches {
        return Err(WireError::invalid(
            "patch list exceeds the configured limit",
        ));
    }
    let patches = value
        .patches
        .into_iter()
        .map(|patch| convert_patch_v006(patch, limits, &mut budget))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(youth_tree::PatchBatch {
        base_revision: value.base_tree_revision,
        next_revision: value.next_tree_revision,
        patches,
    })
}

fn convert_patch(
    value: generated::Patch,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Patch, WireError> {
    Ok(match value {
        generated::Patch::Create(value) => youth_tree::Patch::Create {
            node: convert_node(value.value, limits, budget)?,
        },
        generated::Patch::Delete(value) => youth_tree::Patch::Delete {
            id: node_id(value.id)?,
        },
        generated::Patch::SetText(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(
                    "text patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetText {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated::Patch::SetLabel(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(
                    "label patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetLabel {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated::Patch::SetEnabled(value) => youth_tree::Patch::SetEnabled {
            id: node_id(value.id)?,
            value: value.value,
        },
        generated::Patch::InsertChild(value) => youth_tree::Patch::InsertChild {
            parent: node_id(value.parent)?,
            index: value.index,
            child: node_id(value.child)?,
        },
        generated::Patch::RemoveChild(value) => youth_tree::Patch::RemoveChild {
            parent: node_id(value.parent)?,
            index: value.index,
            expected_child: node_id(value.expected_child)?,
        },
        generated::Patch::MoveChild(value) => youth_tree::Patch::MoveChild {
            parent: node_id(value.parent)?,
            from_index: value.from_index,
            to_index: value.to_index,
            expected_child: node_id(value.expected_child)?,
        },
    })
}

fn convert_patch_v003(
    value: generated_v003::Patch,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Patch, WireError> {
    Ok(match value {
        generated_v003::Patch::Create(value) => youth_tree::Patch::Create {
            node: convert_node_v003(value.value, limits, budget)?,
        },
        generated_v003::Patch::Delete(value) => youth_tree::Patch::Delete {
            id: node_id(value.id)?,
        },
        generated_v003::Patch::SetText(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(
                    "text patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetText {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated_v003::Patch::SetLabel(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(
                    "label patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetLabel {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated_v003::Patch::SetEnabled(value) => youth_tree::Patch::SetEnabled {
            id: node_id(value.id)?,
            value: value.value,
        },
        generated_v003::Patch::InsertChild(value) => youth_tree::Patch::InsertChild {
            parent: node_id(value.parent)?,
            index: value.index,
            child: node_id(value.child)?,
        },
        generated_v003::Patch::RemoveChild(value) => youth_tree::Patch::RemoveChild {
            parent: node_id(value.parent)?,
            index: value.index,
            expected_child: node_id(value.expected_child)?,
        },
        generated_v003::Patch::MoveChild(value) => youth_tree::Patch::MoveChild {
            parent: node_id(value.parent)?,
            from_index: value.from_index,
            to_index: value.to_index,
            expected_child: node_id(value.expected_child)?,
        },
    })
}

fn convert_patch_v004(
    value: generated_v004::Patch,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Patch, WireError> {
    Ok(match value {
        generated_v004::Patch::Create(value) => youth_tree::Patch::Create {
            node: convert_node_v004(value.value, limits, budget)?,
        },
        generated_v004::Patch::Delete(value) => youth_tree::Patch::Delete {
            id: node_id(value.id)?,
        },
        generated_v004::Patch::SetText(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_text_len {
                return Err(WireError::invalid(
                    "text patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetText {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated_v004::Patch::SetLabel(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(
                    "label patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetLabel {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated_v004::Patch::SetEnabled(value) => youth_tree::Patch::SetEnabled {
            id: node_id(value.id)?,
            value: value.value,
        },
        generated_v004::Patch::InsertChild(value) => youth_tree::Patch::InsertChild {
            parent: node_id(value.parent)?,
            index: value.index,
            child: node_id(value.child)?,
        },
        generated_v004::Patch::RemoveChild(value) => youth_tree::Patch::RemoveChild {
            parent: node_id(value.parent)?,
            index: value.index,
            expected_child: node_id(value.expected_child)?,
        },
        generated_v004::Patch::MoveChild(value) => youth_tree::Patch::MoveChild {
            parent: node_id(value.parent)?,
            from_index: value.from_index,
            to_index: value.to_index,
            expected_child: node_id(value.expected_child)?,
        },
    })
}

fn convert_patch_v005(
    value: generated_v005::Patch,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Patch, WireError> {
    Ok(match value {
        generated_v005::Patch::Create(value) => youth_tree::Patch::Create {
            node: convert_node_v005(value.value, limits, budget)?,
        },
        generated_v005::Patch::Delete(value) => youth_tree::Patch::Delete {
            id: node_id(value.id)?,
        },
        generated_v005::Patch::SetText(value) => match value.value {
            generated_v005::TextContent::Literal(text) => {
                budget.charge(text.len())?;
                if text.len() > limits.tree.max_text_len {
                    return Err(WireError::invalid(
                        "text patch exceeds the configured limit",
                    ));
                }
                youth_tree::Patch::SetText {
                    id: node_id(value.id)?,
                    value: text,
                }
            }
            generated_v005::TextContent::Countdown(countdown) => {
                budget.charge(24)?;
                let (schedule, precision, format) = convert_countdown_v005(countdown);
                youth_tree::Patch::SetCountdown {
                    id: node_id(value.id)?,
                    schedule,
                    precision,
                    format,
                }
            }
        },
        generated_v005::Patch::SetLabel(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(
                    "label patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetLabel {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated_v005::Patch::SetEnabled(value) => youth_tree::Patch::SetEnabled {
            id: node_id(value.id)?,
            value: value.value,
        },
        generated_v005::Patch::InsertChild(value) => youth_tree::Patch::InsertChild {
            parent: node_id(value.parent)?,
            index: value.index,
            child: node_id(value.child)?,
        },
        generated_v005::Patch::RemoveChild(value) => youth_tree::Patch::RemoveChild {
            parent: node_id(value.parent)?,
            index: value.index,
            expected_child: node_id(value.expected_child)?,
        },
        generated_v005::Patch::MoveChild(value) => youth_tree::Patch::MoveChild {
            parent: node_id(value.parent)?,
            from_index: value.from_index,
            to_index: value.to_index,
            expected_child: node_id(value.expected_child)?,
        },
    })
}

fn convert_patch_v006(
    value: generated_v006::Patch,
    limits: &RuntimeLimits,
    budget: &mut TransferBudget,
) -> Result<youth_tree::Patch, WireError> {
    Ok(match value {
        generated_v006::Patch::Create(value) => youth_tree::Patch::Create {
            node: convert_node_v006(value.value, limits, budget)?,
        },
        generated_v006::Patch::Delete(value) => youth_tree::Patch::Delete {
            id: node_id(value.id)?,
        },
        generated_v006::Patch::SetText(value) => match value.value {
            generated_v006::TextContent::Literal(text) => {
                budget.charge(text.len())?;
                if text.len() > limits.tree.max_text_len {
                    return Err(WireError::invalid(
                        "text patch exceeds the configured limit",
                    ));
                }
                youth_tree::Patch::SetText {
                    id: node_id(value.id)?,
                    value: text,
                }
            }
            generated_v006::TextContent::Countdown(countdown) => {
                budget.charge(24)?;
                let (schedule, precision, format) = convert_countdown_v006(countdown);
                youth_tree::Patch::SetCountdown {
                    id: node_id(value.id)?,
                    schedule,
                    precision,
                    format,
                }
            }
        },
        generated_v006::Patch::SetLabel(value) => {
            budget.charge(value.value.len())?;
            if value.value.len() > limits.tree.max_label_len {
                return Err(WireError::invalid(
                    "label patch exceeds the configured limit",
                ));
            }
            youth_tree::Patch::SetLabel {
                id: node_id(value.id)?,
                value: value.value,
            }
        }
        generated_v006::Patch::SetEnabled(value) => youth_tree::Patch::SetEnabled {
            id: node_id(value.id)?,
            value: value.value,
        },
        generated_v006::Patch::InsertChild(value) => youth_tree::Patch::InsertChild {
            parent: node_id(value.parent)?,
            index: value.index,
            child: node_id(value.child)?,
        },
        generated_v006::Patch::RemoveChild(value) => youth_tree::Patch::RemoveChild {
            parent: node_id(value.parent)?,
            index: value.index,
            expected_child: node_id(value.expected_child)?,
        },
        generated_v006::Patch::MoveChild(value) => youth_tree::Patch::MoveChild {
            parent: node_id(value.parent)?,
            from_index: value.from_index,
            to_index: value.to_index,
            expected_child: node_id(value.expected_child)?,
        },
    })
}

impl TryFrom<generated::PatchBatch> for youth_tree::PatchBatch {
    type Error = WireError;

    fn try_from(value: generated::PatchBatch) -> Result<Self, Self::Error> {
        patch_batch_v002(value, &RuntimeLimits::default())
    }
}

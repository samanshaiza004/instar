use std::fmt;

use crate::RuntimeLimits;
use crate::bindings::youth::app::ui as generated;

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

impl TryFrom<generated::TreeSnapshot> for youth_tree::TreeSnapshot {
    type Error = WireError;

    fn try_from(value: generated::TreeSnapshot) -> Result<Self, Self::Error> {
        tree_snapshot(value, &RuntimeLimits::default())
    }
}

pub(crate) fn patch_batch(
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

impl TryFrom<generated::PatchBatch> for youth_tree::PatchBatch {
    type Error = WireError;

    fn try_from(value: generated::PatchBatch) -> Result<Self, Self::Error> {
        patch_batch(value, &RuntimeLimits::default())
    }
}

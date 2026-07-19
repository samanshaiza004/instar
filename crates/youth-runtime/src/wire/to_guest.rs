use crate::RuntimeLimits;
use crate::bindings::youth::app::ui as generated;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostEvent {
    pub sequence: u64,
    pub node: youth_tree::NodeId,
}

pub(crate) fn event_batch(
    revision: u64,
    events: &[HostEvent],
    limits: &RuntimeLimits,
) -> Result<generated::EventBatch, &'static str> {
    if events.len() > limits.max_event_batch {
        return Err("event batch exceeds the configured limit");
    }
    Ok(generated::EventBatch {
        tree_revision: revision,
        events: events
            .iter()
            .map(|event| generated::Event {
                sequence: event.sequence,
                kind: generated::EventKind::Activate(event.node.get()),
            })
            .collect(),
    })
}

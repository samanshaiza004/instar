//! B2e-3a: the kernel's attachment-table bound stays tied to the protocol's
//! node bound.
//!
//! `instar-kernel` cannot name `instar_ui_protocol::limits::MAX_NODES`: it is
//! forbidden from depending on the UI protocol (see its Cargo.toml
//! description), so `MAX_TEXT_ATTACHMENTS` is a literal 4096 with the
//! relationship asserted here, in the only crate where both sides are
//! visible. The derivation matters as much as the equality: a valid snapshot
//! has at most MAX_NODES nodes, every text view refers to at most one slot,
//! and no valid tree can therefore reference more than MAX_NODES distinct
//! attachment slots.

/// The kernel's policy bound must restate the protocol's node bound.
#[test]
fn attachment_bound_matches_the_protocol_node_bound() {
    assert_eq!(
        instar_kernel::text_bridge::MAX_TEXT_ATTACHMENTS,
        instar_ui_protocol::limits::MAX_NODES,
        "a valid snapshot has at most MAX_NODES nodes and every TextView refers \
         to at most one slot, so no valid tree can reference more than MAX_NODES \
         distinct attachment entries; the kernel's bound is this protocol bound \
         restated without creating an instar-kernel -> instar-ui-protocol dependency"
    );
}

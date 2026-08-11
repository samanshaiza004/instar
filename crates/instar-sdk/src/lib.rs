//! A thin guest-side builder for Instar interfaces.
//!
//! This crate exists because of two specific complaints, written down while
//! building the Calculator against the raw wire and marked `PAIN:` in its
//! source. It solves those two and stops.
//!
//! ```text
//! PAIN 2  child counts declared by hand, and silently wrong when they drift
//! PAIN 4  every application reinventing a NodeKey -> meaning lookup
//! ```
//!
//! # What it deliberately is not
//!
//! ```text
//! signals            hooks              component lifecycle
//! reactive store     memoization        a virtual DOM
//! guest reconciliation
//! ```
//!
//! The host already reconciles authoritative snapshots. A second reconciler in
//! the guest would be the delta protocol arriving through the back door, which
//! is the arrangement Instar was built to avoid. This is a safe snapshot
//! builder and a semantic event router: nothing else belongs here until an
//! application says otherwise, in the same way these two did.
//!
//! # It does not invent identity
//!
//! Node keys are supplied by the caller, never derived from tree position.
//! Stable identity is load-bearing across the whole system — focus, pressed
//! state, accessibility identity, and stale-event rejection all key off it —
//! and identity derived from position changes whenever the tree is
//! restructured. Removing the boilerplate *around* identity is ergonomics;
//! generating identity would be a correctness change wearing ergonomics as a
//! disguise.
//!
//! # Guest-visible layering
//!
//! ```text
//! instar-sdk
//!     ↓
//! instar-ui-protocol
//!
//! forbidden: instar-ui, instar-host, instar-window, renderers, the shell
//! ```
//!
//! Using it is optional. The wire is public and hand-encoding stays supported;
//! `guests/gallery` and `guests/counter` do exactly that.

#![forbid(unsafe_code)]

use instar_ui_protocol::{BatchEncoder, NodeKey, WireEvent, WireLayout, WireStyle, flags, opcode};

/// One node and the children collected under it.
///
/// The count is taken from the vector at emit time, which is the entire point:
/// `BatchEncoder` needs each node to declare how many children follow, and a
/// declared number that disagrees with the emitted subtrees desynchronizes the
/// stream so the *next* node decodes as a section opcode. That is not a
/// hypothetical — it happened twice while building the Gallery, and the wire
/// is right to be a flat depth-first stream. The guest simply should not have
/// to hold the invariant by hand.
struct Element {
    kind: u8,
    key: NodeKey,
    flags: u8,
    text: Option<String>,
    layout: WireLayout,
    style: WireStyle,
    children: Vec<Element>,
}

impl Element {
    fn emit(&self, encoder: &mut BatchEncoder) {
        encoder.node_styled(
            self.kind,
            self.key,
            self.flags,
            self.text.as_deref(),
            self.layout,
            self.style,
            u16::try_from(self.children.len()).unwrap_or(u16::MAX),
        );
        for child in &self.children {
            child.emit(encoder);
        }
    }
}

/// Builds one interface snapshot, and records what each control means.
///
/// `M` is the application's own message type. The SDK never interprets it.
pub struct Ui<M> {
    /// One frame per open container. The last is where new nodes land.
    frames: Vec<Vec<Element>>,
    routes: Vec<(NodeKey, M)>,
}

impl<M> Default for Ui<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> Ui<M> {
    pub fn new() -> Self {
        Self {
            frames: vec![Vec::new()],
            routes: Vec::new(),
        }
    }

    fn current(&mut self) -> &mut Vec<Element> {
        self.frames.last_mut().expect("a frame is always open")
    }

    fn leaf(&mut self, kind: u8, id: u32, flags: u8, text: Option<String>) -> Handle<'_, M> {
        self.push(Element {
            kind,
            key: NodeKey::first(id),
            flags,
            text,
            layout: WireLayout::default(),
            style: WireStyle::default(),
            children: Vec::new(),
        })
    }

    fn push(&mut self, element: Element) -> Handle<'_, M> {
        self.current().push(element);
        Handle { ui: self }
    }

    /// Opens a container, runs `build`, and closes it with the right count.
    fn container(&mut self, kind: u8, id: u32, build: impl FnOnce(&mut Self)) -> Handle<'_, M> {
        self.frames.push(Vec::new());
        build(self);
        let children = self.frames.pop().expect("the frame just opened");
        self.push(Element {
            kind,
            key: NodeKey::first(id),
            flags: 0,
            text: None,
            layout: WireLayout::default(),
            style: WireStyle::default(),
            children,
        })
    }

    pub fn root(&mut self, id: u32, build: impl FnOnce(&mut Self)) -> Handle<'_, M> {
        self.container(opcode::NODE_ROOT, id, build)
    }

    pub fn column(&mut self, id: u32, build: impl FnOnce(&mut Self)) -> Handle<'_, M> {
        self.container(opcode::NODE_COLUMN, id, build)
    }

    pub fn row(&mut self, id: u32, build: impl FnOnce(&mut Self)) -> Handle<'_, M> {
        self.container(opcode::NODE_ROW, id, build)
    }

    pub fn stack(&mut self, id: u32, build: impl FnOnce(&mut Self)) -> Handle<'_, M> {
        self.container(opcode::NODE_STACK, id, build)
    }

    /// A viewport, which takes exactly one content child.
    ///
    /// The signature cannot express anything else, because the host rejects
    /// anything else — a rule worth enforcing where it is cheapest to obey.
    pub fn scroll(&mut self, id: u32, build: impl FnOnce(&mut Self)) -> Handle<'_, M> {
        self.container(opcode::NODE_SCROLL, id, build)
    }

    pub fn text(&mut self, id: u32, text: impl Into<String>) -> Handle<'_, M> {
        self.leaf(opcode::NODE_TEXT, id, 0, Some(text.into()))
    }

    pub fn button(&mut self, id: u32, label: impl Into<String>) -> Handle<'_, M> {
        self.leaf(opcode::NODE_BUTTON, id, flags::ENABLED, Some(label.into()))
    }

    /// Present and refusing input, which is not the same as absent.
    pub fn disabled_button(&mut self, id: u32, label: impl Into<String>) -> Handle<'_, M> {
        self.leaf(opcode::NODE_BUTTON, id, 0, Some(label.into()))
    }

    /// Encodes the snapshot, and hands back the routing table for it.
    ///
    /// The two travel together on purpose: a routing table is only valid for
    /// the tree it was built from, and returning them separately would invite
    /// an application to keep a stale one.
    pub fn finish(mut self) -> (Vec<u8>, Routes<M>) {
        let roots = self.frames.pop().expect("the outermost frame");
        debug_assert!(
            self.frames.is_empty(),
            "every container closes its own frame"
        );
        let mut encoder = BatchEncoder::new();
        for element in &roots {
            element.emit(&mut encoder);
        }
        (
            encoder.finish(),
            Routes {
                routes: self.routes,
            },
        )
    }
}

/// A node that has just been added, for attaching layout, style and meaning.
///
/// It carries no index. The node it refers to is always the last in the
/// current frame, and that is guaranteed by the borrow rather than tracked: a
/// handle holds `&mut Ui` for its whole lifetime, so nothing can be pushed
/// between creating one and using it. An index field would suggest a hazard
/// the borrow checker has already ruled out, and a test for it could not
/// discriminate.
pub struct Handle<'a, M> {
    ui: &'a mut Ui<M>,
}

/// The chaining methods are deliberately *not* `#[must_use]`.
///
/// Each one's purpose is its side effect on the node; the returned handle
/// exists only so calls can be chained. Marking them would make
/// `ui.button(7, "7").on_activate(Msg::Seven);` -- the ordinary terminal call
/// -- a lint error, which is the opposite of the ergonomics this crate is for.
impl<M> Handle<'_, M> {
    fn element(&mut self) -> &mut Element {
        self.ui
            .current()
            .last_mut()
            .expect("a handle is created from a node that was just pushed")
    }

    pub fn layout(mut self, layout: WireLayout) -> Self {
        self.element().layout = layout;
        self
    }

    pub fn style(mut self, style: WireStyle) -> Self {
        self.element().style = style;
        self
    }

    /// What activating this control means to the application.
    ///
    /// Bound to the node's full [`NodeKey`], generation included. Matching on
    /// the numeric half alone — which is what an application writing its own
    /// lookup naturally does — would let an event for a retired node reach
    /// whatever replaced it, which is the ABA case the generation exists to
    /// prevent.
    pub fn on_activate(mut self, message: M) -> Self {
        let key = self.element().key;
        self.ui.routes.push((key, message));
        self
    }
}

/// What each control in one snapshot meant.
pub struct Routes<M> {
    routes: Vec<(NodeKey, M)>,
}

impl<M> Routes<M> {
    /// The message for an event, if the control it names had one.
    ///
    /// `None` covers both a control with no binding and an event naming a node
    /// this snapshot does not contain — the host already refuses stale keys,
    /// and this refuses them again rather than guessing.
    pub fn message(&self, event: &WireEvent) -> Option<&M> {
        let WireEvent::Click { node } = event;
        self.routes
            .iter()
            .find(|(key, _)| key == node)
            .map(|(_, message)| message)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_ui_protocol::decode_batch;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Msg {
        Seven,
        Equals,
    }

    /// The hazard this crate exists for: counts come from the tree, not the
    /// author.
    #[test]
    fn child_counts_come_from_what_was_actually_emitted() {
        let mut ui = Ui::<Msg>::new();
        ui.root(0, |ui| {
            ui.text(1, "display");
            ui.row(2, |ui| {
                ui.button(3, "7");
                ui.button(4, "8");
                ui.button(5, "9");
            });
        });
        let (bytes, _) = ui.finish();

        let batch = decode_batch(&bytes).expect("a builder cannot desynchronize the stream");
        let counts: Vec<(u32, u16)> = batch
            .nodes
            .iter()
            .map(|node| (node.key.id, node.child_count))
            .collect();
        assert_eq!(
            counts,
            vec![(0, 2), (1, 0), (2, 3), (3, 0), (4, 0), (5, 0)],
            "each container reports the number of children it actually has"
        );
    }

    /// Depth-first order, because that is what the wire is.
    #[test]
    fn nodes_are_emitted_depth_first() {
        let mut ui = Ui::<Msg>::new();
        ui.root(0, |ui| {
            ui.column(1, |ui| {
                ui.text(2, "inner");
            });
            ui.text(3, "after");
        });
        let (bytes, _) = ui.finish();
        let batch = decode_batch(&bytes).expect("valid");
        let ids: Vec<u32> = batch.nodes.iter().map(|node| node.key.id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3]);
    }

    /// Routing is by the whole key, generation included.
    #[test]
    fn an_activation_routes_to_its_message_and_a_stale_key_routes_to_nothing() {
        let mut ui = Ui::<Msg>::new();
        ui.root(0, |ui| {
            ui.button(7, "7").on_activate(Msg::Seven);
            ui.button(9, "=").on_activate(Msg::Equals);
            ui.button(8, "unbound");
        });
        let (_, routes) = ui.finish();

        assert_eq!(
            routes.message(&WireEvent::Click {
                node: NodeKey::first(7)
            }),
            Some(&Msg::Seven)
        );
        assert_eq!(
            routes.message(&WireEvent::Click {
                node: NodeKey::first(9)
            }),
            Some(&Msg::Equals)
        );
        assert_eq!(
            routes.message(&WireEvent::Click {
                node: NodeKey::first(8)
            }),
            None,
            "a control with no binding means nothing, rather than the wrong thing"
        );
        assert_eq!(
            routes.message(&WireEvent::Click {
                node: NodeKey::new(7, 1)
            }),
            None,
            "a later generation of the same id is a different control -- \
             matching on the numeric half is the ABA hole the generation \
             exists to close"
        );
    }

    /// One sibling's attributes do not bleed into another's, and a nested
    /// node's do not escape to the enclosing frame.
    #[test]
    fn attributes_land_on_their_own_node() {
        use instar_ui_protocol::{WirePaintStyle, WireSize};
        let mut ui = Ui::<Msg>::new();
        ui.root(0, |ui| {
            ui.text(1, "a").layout(WireLayout {
                height: WireSize::Fixed(40),
                ..WireLayout::default()
            });
            ui.text(2, "b").style(WireStyle {
                paint: WirePaintStyle {
                    corner_radius: 9,
                    ..WirePaintStyle::default()
                },
                ..WireStyle::default()
            });
        });
        let (bytes, _) = ui.finish();
        let batch = decode_batch(&bytes).expect("valid");
        let node = |id: u32| {
            batch
                .nodes
                .iter()
                .find(|node| node.key.id == id)
                .expect("present")
        };
        assert_eq!(node(1).layout.height, WireSize::Fixed(40));
        assert_eq!(node(1).style.paint.corner_radius, 0);
        assert_eq!(node(2).layout.height, WireSize::Content);
        assert_eq!(node(2).style.paint.corner_radius, 9);
    }

    /// A container's attributes survive its children being built.
    #[test]
    fn a_container_keeps_its_own_attributes() {
        use instar_ui_protocol::WireSize;
        let mut ui = Ui::<Msg>::new();
        ui.root(0, |ui| {
            ui.column(1, |ui| {
                ui.text(2, "child");
            })
            .layout(WireLayout {
                width: WireSize::Fixed(120),
                gap: 6,
                ..WireLayout::default()
            });
        });
        let (bytes, _) = ui.finish();
        let batch = decode_batch(&bytes).expect("valid");
        let column = batch
            .nodes
            .iter()
            .find(|node| node.key.id == 1)
            .expect("present");
        assert_eq!(column.layout.width, WireSize::Fixed(120));
        assert_eq!(column.layout.gap, 6);
        assert_eq!(column.child_count, 1);
    }
}

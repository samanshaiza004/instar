//! The platform accessibility seam.
//!
//! Everything AccessKit sends arrives here as a request on the main thread,
//! and everything Instar sends back leaves through [`UpdateSink`]. The one
//! thing that cannot be tested without a real desktop is the single call to
//! `accesskit_winit::Adapter::update_if_active` behind that trait; the
//! decisions about *whether* and *how often* to make that call are all on this
//! side of it, and are tested below.
//!
//! Two rules shape this module.
//!
//! The first is that the platform never touches host state. `accesskit_winit`
//! calls its handlers on whatever thread the platform adapter happens to use,
//! so Instar takes the constructor that forwards every request through the
//! event loop proxy — activation included. The alternative, a direct
//! activation handler, would have to reach the retained tree from an
//! unspecified thread.
//!
//! The second is that [`HostBridge::accessibility_update`] *drains*: what it
//! returns is not offered twice. Asking for an update while nothing is
//! listening would therefore throw it away. So [`Accessibility`] tracks
//! whether the platform is attached and does not ask at all when it is not.

use accesskit::TreeUpdate;

/// Where a finished update goes.
///
/// In the shell this is one `Adapter::update_if_active` call. In tests it is a
/// counter, which is the whole point: "exactly once per update" is a claim
/// about this trait's use, not about AccessKit.
pub(crate) trait UpdateSink {
    fn send(&mut self, update: TreeUpdate);
}

/// What the shell must do about one platform request.
///
/// Conversion into this is pure, so the transport can be checked without a
/// window, an adapter, or a guest.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// An assistive technology attached. It holds nothing, so the next update
    /// must be the entire tree rather than a diff against a tree it lacks.
    SendFullTree,
    /// Route this through the host exactly as a pointer or key would be.
    Forward {
        action: accesskit::Action,
        target: accesskit::NodeId,
    },
    /// Nothing further to do.
    Nothing,
}

/// Whether the platform is listening, and what follows from that.
#[derive(Debug, Default)]
pub(crate) struct Accessibility {
    /// Set by `InitialTreeRequested`, cleared by `AccessibilityDeactivated`.
    attached: bool,
}

impl Accessibility {
    #[cfg(test)]
    pub(crate) fn is_attached(&self) -> bool {
        self.attached
    }

    /// Classifies one request from the platform adapter.
    ///
    /// The only state this changes is whether anything is listening. Acting on
    /// the result is the caller's job, which is what keeps this testable.
    pub(crate) fn classify(&mut self, event: accesskit_winit::WindowEvent) -> Request {
        use accesskit_winit::WindowEvent as Platform;
        match event {
            Platform::InitialTreeRequested => {
                self.attached = true;
                Request::SendFullTree
            }
            Platform::ActionRequested(request) => Request::Forward {
                action: request.action,
                target: request.target_node,
            },
            Platform::AccessibilityDeactivated => {
                self.attached = false;
                Request::Nothing
            }
        }
    }

    /// Offers the host's pending update to the platform, if there is one and
    /// anything is listening.
    ///
    /// `produce` is not called when nothing is attached. That is not an
    /// optimization: calling it would drain the host's projection into a sink
    /// that discards it, and the change would never be described to the
    /// assistive technology that attached next.
    pub(crate) fn flush(
        &mut self,
        produce: impl FnOnce() -> Option<TreeUpdate>,
        sink: &mut impl UpdateSink,
    ) {
        if !self.attached {
            return;
        }
        if let Some(update) = produce() {
            sink.send(update);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{Action, ActionRequest, NodeId};
    use accesskit_winit::WindowEvent as Platform;

    fn action(action: Action) -> Platform {
        Platform::ActionRequested(ActionRequest {
            action,
            target_tree: accesskit::TreeId::ROOT,
            target_node: NodeId(7),
            data: None,
        })
    }

    fn empty_update() -> TreeUpdate {
        TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            tree_id: accesskit::TreeId::ROOT,
            focus: NodeId(0),
        }
    }

    #[derive(Default)]
    struct Counting {
        sent: usize,
    }

    impl UpdateSink for Counting {
        fn send(&mut self, _: TreeUpdate) {
            self.sent += 1;
        }
    }

    /// Every action the platform can raise crosses unchanged, and the two
    /// lifecycle events are not mistaken for actions.
    #[test]
    fn platform_requests_convert_without_loss() {
        let mut a11y = Accessibility::default();

        for want in [
            Action::Click,
            Action::Focus,
            Action::Blur,
            Action::ScrollIntoView,
            Action::SetValue,
        ] {
            assert_eq!(
                a11y.classify(action(want)),
                Request::Forward {
                    action: want,
                    target: NodeId(7)
                },
                "{want:?} must reach the host as itself -- deciding what is \
                 supported belongs to the host, not to transport"
            );
        }

        assert_eq!(
            a11y.classify(Platform::InitialTreeRequested),
            Request::SendFullTree
        );
        assert_eq!(
            a11y.classify(Platform::AccessibilityDeactivated),
            Request::Nothing
        );
    }

    /// The seam is used exactly once per update the host produces, and not at
    /// all when it produces none.
    #[test]
    fn the_adapter_seam_is_used_once_per_update_and_never_otherwise() {
        let mut a11y = Accessibility::default();
        let mut sink = Counting::default();
        a11y.classify(Platform::InitialTreeRequested);

        a11y.flush(|| Some(empty_update()), &mut sink);
        assert_eq!(sink.sent, 1, "one update, one call");

        a11y.flush(|| None, &mut sink);
        assert_eq!(
            sink.sent, 1,
            "a frame that changed nothing accessibility-observable must not \
             reach the platform at all"
        );

        for _ in 0..3 {
            a11y.flush(|| Some(empty_update()), &mut sink);
        }
        assert_eq!(sink.sent, 4, "and no call is duplicated or coalesced");
    }

    /// While nothing is attached the host is never even asked, because asking
    /// would drain a change into a void.
    #[test]
    fn nothing_is_drained_from_the_host_while_nothing_is_listening() {
        let mut a11y = Accessibility::default();
        let mut sink = Counting::default();
        let mut asked = 0;

        assert!(!a11y.is_attached(), "nothing is attached at startup");
        a11y.flush(
            || {
                asked += 1;
                Some(empty_update())
            },
            &mut sink,
        );
        assert_eq!((asked, sink.sent), (0, 0), "not asked, so nothing is lost");

        a11y.classify(Platform::InitialTreeRequested);
        a11y.flush(
            || {
                asked += 1;
                Some(empty_update())
            },
            &mut sink,
        );
        assert_eq!((asked, sink.sent), (1, 1));

        // Detaching must close the tap again, or the updates accumulated while
        // an assistive technology is away are silently discarded.
        a11y.classify(Platform::AccessibilityDeactivated);
        a11y.flush(
            || {
                asked += 1;
                Some(empty_update())
            },
            &mut sink,
        );
        assert_eq!(
            (asked, sink.sent),
            (1, 1),
            "after deactivation the host is left alone again"
        );
    }
}

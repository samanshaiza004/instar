//! The host's half of the text-resource bridge (B2e-1).
//!
//! Three lifetimes, three owners, and the whole of what B2e exists to prove:
//!
//! ```text
//! GenerationId  owns  guest capability leases     dies with the Store
//! TextSystem    owns  the native resources        outlives any guest
//! HostWindow    owns  presentation attachments    B2e-4, not yet
//! ```
//!
//! This registry is **host-global and keyed by generation**, never per window.
//! A buffer must not disappear because a native window did; the two have
//! nothing to do with each other.
//!
//! # The registry is authoritative, not the id
//!
//! > No text operation using an opaque key is accepted merely because the
//! > underlying `TextBufferId` or `TextViewId` is live. The current generation
//! > must own a matching lease.
//!
//! `TextViewId`'s own generation closes ABA — a stale key cannot reach the
//! resource that replaced it. It says nothing about *authority*:
//!
//! ```text
//! generation 17 creates V4, then dies
//! V4 survives, because something else will own it (B2e-4)
//! generation 18 presents a key naming V4
//!   -> the id is live
//!   -> without the registry, accepted
//! ```
//!
//! So every operation resolves through [`TextHost::resolve_view_lease`] or
//! [`TextHost::resolve_buffer_lease`], which check ownership before identity.

use std::collections::{HashMap, HashSet};

use instar_kernel::runtime::GenerationId;
use instar_kernel::text_bridge::{
    OpaqueResourceKey, ScreenedTextRequest, TextAnswer, TextOperation, TextRefusal,
};
use instar_text::{MAX_TEXT_BUFFERS, MAX_TEXT_VIEWS, TextBufferId, TextSystem, TextViewId};

/// What one guest generation currently holds.
#[derive(Debug, Default)]
struct GenerationLeases {
    buffers: HashSet<TextBufferId>,
    views: HashSet<TextViewId>,
}

impl GenerationLeases {
    fn is_empty(&self) -> bool {
        self.buffers.is_empty() && self.views.is_empty()
    }
}

/// How many resources and leases exist, for tests that assert a return to
/// baseline rather than that some `drop` ran.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextResourceCounts {
    pub guest_buffer_leases: usize,
    pub guest_view_leases: usize,
    pub live_buffers: usize,
    pub live_views: usize,
}

/// The text subsystem, and who is allowed to name what in it.
#[derive(Debug, Default)]
pub struct TextHost {
    system: TextSystem,
    leases: HashMap<GenerationId, GenerationLeases>,
}

/// A `TextBufferId` and an [`OpaqueResourceKey`] are the same two numbers.
///
/// The translation exists in exactly one place, and this is it: the kernel
/// cannot perform it because it has no idea these types exist, which is what
/// keeps `instar-kernel -> instar-text` from being a dependency.
fn buffer_key(id: TextBufferId) -> OpaqueResourceKey {
    OpaqueResourceKey {
        slot: id.id,
        incarnation: id.generation,
    }
}

fn view_key(id: TextViewId) -> OpaqueResourceKey {
    OpaqueResourceKey {
        slot: id.id,
        incarnation: id.generation,
    }
}

fn buffer_id(key: OpaqueResourceKey) -> TextBufferId {
    TextBufferId {
        id: key.slot,
        generation: key.incarnation,
    }
}

fn view_id(key: OpaqueResourceKey) -> TextViewId {
    TextViewId {
        id: key.slot,
        generation: key.incarnation,
    }
}

impl TextHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn system(&self) -> &TextSystem {
        &self.system
    }

    pub fn system_mut(&mut self) -> &mut TextSystem {
        &mut self.system
    }

    pub fn counts(&self) -> TextResourceCounts {
        TextResourceCounts {
            guest_buffer_leases: self.leases.values().map(|l| l.buffers.len()).sum(),
            guest_view_leases: self.leases.values().map(|l| l.views.len()).sum(),
            live_buffers: self.system.live_buffers(),
            live_views: self.system.live_views(),
        }
    }

    /// A key this generation is allowed to use, as a buffer id.
    pub fn resolve_buffer_lease(
        &self,
        generation: GenerationId,
        key: OpaqueResourceKey,
    ) -> Result<TextBufferId, TextRefusal> {
        let id = buffer_id(key);
        // Ownership first, identity second. A live id this generation does not
        // hold is not this generation's to touch.
        if !self
            .leases
            .get(&generation)
            .is_some_and(|leases| leases.buffers.contains(&id))
        {
            return Err(TextRefusal::NoSuchResource);
        }
        self.system
            .buffer(id)
            .map(|_| id)
            .map_err(|_| TextRefusal::NoSuchResource)
    }

    pub fn resolve_view_lease(
        &self,
        generation: GenerationId,
        key: OpaqueResourceKey,
    ) -> Result<TextViewId, TextRefusal> {
        let id = view_id(key);
        if !self
            .leases
            .get(&generation)
            .is_some_and(|leases| leases.views.contains(&id))
        {
            return Err(TextRefusal::NoSuchResource);
        }
        self.system
            .view(id)
            .map(|_| id)
            .map_err(|_| TextRefusal::NoSuchResource)
    }

    /// Serves one screened request.
    ///
    /// The ordering inside creation is load-bearing: allocate, **register the
    /// lease**, and only then reply. Registering after the reply would leave a
    /// window in which terminalization lands between the two and produces
    /// exactly the orphan this registry exists to prevent.
    pub fn serve(&mut self, request: ScreenedTextRequest) {
        let generation = request.generation();
        match request.operation() {
            TextOperation::CreateBuffer => match self.system.open_buffer("") {
                Ok(id) => {
                    self.leases
                        .entry(generation)
                        .or_default()
                        .buffers
                        .insert(id);
                    request.answer(TextAnswer::Created(buffer_key(id)));
                }
                Err(_) => request.refuse(TextRefusal::TooManyBuffers(MAX_TEXT_BUFFERS as u32)),
            },
            TextOperation::CreateView { buffer } => {
                let buffer = match self.resolve_buffer_lease(generation, buffer) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                match self.system.open_view(buffer) {
                    Ok(id) => {
                        self.leases.entry(generation).or_default().views.insert(id);
                        request.answer(TextAnswer::Created(view_key(id)));
                    }
                    Err(_) => request.refuse(TextRefusal::TooManyViews(MAX_TEXT_VIEWS as u32)),
                }
            }
            TextOperation::ReleaseBuffer { key } => {
                let id = match self.resolve_buffer_lease(generation, key) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                self.release_buffer(generation, id);
                request.answer(TextAnswer::Released);
            }
            TextOperation::ReleaseView { key } => {
                let id = match self.resolve_view_lease(generation, key) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                self.release_view(generation, id);
                request.answer(TextAnswer::Released);
            }
        }
    }

    /// Drops one generation's lease, then lets the subsystem decide whether
    /// the resource dies.
    ///
    /// A buffer with views still on it survives its guest lease: the guest
    /// losing the ability to *name* a document is not the document ending.
    fn release_buffer(&mut self, generation: GenerationId, id: TextBufferId) {
        if let Some(leases) = self.leases.get_mut(&generation) {
            leases.buffers.remove(&id);
        }
        if self.system.views_of(id) == 0 {
            self.system.close_buffer(id);
        }
    }

    fn release_view(&mut self, generation: GenerationId, id: TextViewId) {
        let buffer = self.system.view(id).ok().map(|view| view.buffer());
        if let Some(leases) = self.leases.get_mut(&generation) {
            leases.views.remove(&id);
        }
        self.system.close_view(id);

        // The buffer may have been kept alive only by this view. Nobody holds
        // a lease on it and nothing views it, so it goes too.
        if let Some(buffer) = buffer
            && self.system.views_of(buffer) == 0
            && !self.leases.values().any(|l| l.buffers.contains(&buffer))
        {
            self.system.close_buffer(buffer);
        }
    }

    /// Every lease a generation still holds, released.
    ///
    /// Called from `Host::on_guest_gone` for both a trap and a clean exit,
    /// because a guest whose `run` returned has ended just as completely as
    /// one that trapped — and because Store destruction runs no guest
    /// destructors at all on the trap path.
    ///
    /// Keyed only by generation. Using a `WindowId` to decide what to release
    /// would tie a document's lifetime to a surface.
    pub fn release_generation(&mut self, generation: GenerationId) {
        let Some(leases) = self.leases.remove(&generation) else {
            return;
        };
        for view in leases.views {
            self.system.close_view(view);
        }
        for buffer in leases.buffers {
            if self.system.views_of(buffer) == 0 {
                self.system.close_buffer(buffer);
            }
        }
        // Views that outlived their generation's buffer lease can leave a
        // buffer nobody can reach. Nothing else owns one yet -- B2e-4 adds
        // retained UI attachment as the second owner -- so an unreferenced
        // buffer with no lease is unreachable and goes.
        self.collect_unreachable_buffers();
        self.leases.retain(|_, leases| !leases.is_empty());
    }

    fn collect_unreachable_buffers(&mut self) {
        let leased: HashSet<TextBufferId> = self
            .leases
            .values()
            .flat_map(|leases| leases.buffers.iter().copied())
            .collect();
        let orphaned: Vec<TextBufferId> = self
            .system
            .buffers()
            .filter(|id| !leased.contains(id) && self.system.views_of(*id) == 0)
            .collect();
        for id in orphaned {
            self.system.close_buffer(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_kernel::text_bridge::{TextOperation, text_request};

    const G17: GenerationId = GenerationId(17);
    const G18: GenerationId = GenerationId(18);

    /// Serves one operation and returns what the guest would have seen.
    fn serve(
        host: &mut TextHost,
        generation: GenerationId,
        operation: TextOperation,
    ) -> Result<TextAnswer, TextRefusal> {
        let (request, wait) = text_request(generation, operation);
        let screened = request.screen(generation).expect("current");
        host.serve(screened);
        wait.blocking_recv().expect("answered")
    }

    fn create_buffer(host: &mut TextHost, generation: GenerationId) -> OpaqueResourceKey {
        match serve(host, generation, TextOperation::CreateBuffer) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a buffer, got {other:?}"),
        }
    }

    fn create_view(
        host: &mut TextHost,
        generation: GenerationId,
        buffer: OpaqueResourceKey,
    ) -> OpaqueResourceKey {
        match serve(host, generation, TextOperation::CreateView { buffer }) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a view, got {other:?}"),
        }
    }

    #[test]
    fn creating_a_buffer_and_a_view_registers_both_leases() {
        let mut host = TextHost::new();
        assert_eq!(host.counts(), TextResourceCounts::default());

        let buffer = create_buffer(&mut host, G17);
        assert_eq!(
            host.counts(),
            TextResourceCounts {
                guest_buffer_leases: 1,
                live_buffers: 1,
                ..Default::default()
            }
        );

        let view = create_view(&mut host, G17, buffer);
        let counts = host.counts();
        assert_eq!(counts.guest_view_leases, 1);
        assert_eq!(counts.live_views, 1);

        // The view really refers to that buffer, not to some other one.
        let view_id = host.resolve_view_lease(G17, view).expect("leased");
        assert_eq!(
            host.system().view(view_id).expect("live").buffer(),
            host.resolve_buffer_lease(G17, buffer).expect("leased")
        );
    }

    /// A guest losing the ability to *name* a document is not the document
    /// ending.
    #[test]
    fn dropping_a_buffer_lease_while_a_view_exists_keeps_the_buffer() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);

        assert_eq!(
            serve(&mut host, G17, TextOperation::ReleaseBuffer { key: buffer }),
            Ok(TextAnswer::Released)
        );

        let counts = host.counts();
        assert_eq!(counts.guest_buffer_leases, 0, "the lease is gone");
        assert_eq!(counts.live_buffers, 1, "the buffer is not");
        assert!(
            host.resolve_view_lease(G17, view).is_ok(),
            "and so is the view"
        );

        // Now the view goes too, and nothing is left to keep the buffer.
        assert_eq!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: view }),
            Ok(TextAnswer::Released)
        );
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    /// Slot reuse advances the incarnation, so a released key cannot reach
    /// what replaced it.
    #[test]
    fn a_released_key_cannot_reach_the_resource_that_replaced_it() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let first = create_view(&mut host, G17, buffer);
        assert_eq!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: first }),
            Ok(TextAnswer::Released)
        );

        let second = create_view(&mut host, G17, buffer);
        assert_eq!(second.slot, first.slot, "the slot was reused");
        assert_ne!(
            second.incarnation, first.incarnation,
            "and the incarnation moved, or the stale key would still work"
        );
        assert_eq!(
            host.resolve_view_lease(G17, first),
            Err(TextRefusal::NoSuchResource)
        );
    }

    /// The registry is authoritative, not the id.
    ///
    /// This is the case a generational id cannot catch on its own: the
    /// resource is genuinely live, and the asking generation simply has no
    /// authority over it.
    #[test]
    fn another_generation_cannot_use_a_live_resource_it_does_not_lease() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);

        assert!(host.resolve_view_lease(G17, view).is_ok(), "17 owns it");
        assert_eq!(
            host.resolve_view_lease(G18, view),
            Err(TextRefusal::NoSuchResource),
            "the id is live and 18 still may not touch it -- ABA protection \
             answers which resource, never whose"
        );
        assert_eq!(
            serve(&mut host, G18, TextOperation::ReleaseView { key: view }),
            Err(TextRefusal::NoSuchResource)
        );
    }

    /// Both terminal paths, because a clean exit destroys a generation as
    /// completely as a trap does.
    #[test]
    fn a_dead_generation_returns_every_lease_to_baseline() {
        for label in ["clean exit", "trap"] {
            let mut host = TextHost::new();
            let buffer = create_buffer(&mut host, G17);
            create_view(&mut host, G17, buffer);
            create_view(&mut host, G17, buffer);
            assert_eq!(host.counts().live_views, 2, "{label}");

            host.release_generation(G17);

            assert_eq!(
                host.counts(),
                TextResourceCounts::default(),
                "{label}: leases and resources both returned to baseline"
            );
        }
    }

    /// One generation dying does not touch another's.
    #[test]
    fn releasing_one_generation_leaves_another_alone() {
        let mut host = TextHost::new();
        let seventeen = create_buffer(&mut host, G17);
        let eighteen = create_buffer(&mut host, G18);

        host.release_generation(G17);

        assert_eq!(
            host.resolve_buffer_lease(G17, seventeen),
            Err(TextRefusal::NoSuchResource)
        );
        assert!(host.resolve_buffer_lease(G18, eighteen).is_ok());
        assert_eq!(host.counts().live_buffers, 1);
    }

    /// The race, in both of its two allowed shapes.
    ///
    /// There is no third: an orphaned resource with no owner at all is the
    /// outcome the registry exists to make impossible.
    #[test]
    fn a_create_racing_a_dead_generation_has_exactly_two_outcomes() {
        // Creation wins: the resource exists, its lease is registered, and
        // teardown then reclaims it.
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        create_view(&mut host, G17, buffer);
        assert_eq!(host.counts().live_views, 1);
        host.release_generation(G17);
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "creation won, and teardown reclaimed what it made"
        );

        // Terminal wins: the request is screened against a retired generation
        // and never reaches the subsystem at all.
        let mut host = TextHost::new();
        host.release_generation(G17);
        let (request, wait) = text_request(G17, TextOperation::CreateBuffer);
        let stale = request.screen(GenerationId(0)).expect_err("retired");
        assert_eq!(stale, G17);
        assert_eq!(
            wait.blocking_recv().expect("answered"),
            Err(TextRefusal::StaleGeneration)
        );
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "terminal won, and nothing was allocated"
        );
    }

    /// The wiring: `on_guest_gone` releases by generation, and the window it
    /// names has no say.
    ///
    /// The obvious fault -- releasing only what belongs to `window_id` -- is
    /// not expressible here, because `release_generation` takes no window. The
    /// risk this test covers is the other one: that the call is placed *below*
    /// the clean-exit early return, where a guest whose `run` returned would
    /// leak everything it held.
    #[test]
    fn a_guest_that_exits_cleanly_releases_its_leases_through_the_host() {
        for (label, error) in [("clean exit", None), ("trap", Some("boom".to_string()))] {
            let mut host = crate::Host::new();
            let buffer = create_buffer(host.text_resources_mut(), G17);
            create_view(host.text_resources_mut(), G17, buffer);
            assert_eq!(host.text_resources().counts().live_views, 1, "{label}");

            // A window id that names no window at all: resource lifetime has
            // nothing to do with whether a surface exists.
            host.on_guest_gone(instar_window::WindowId::from_raw(99), G17, error);

            assert_eq!(
                host.text_resources().counts(),
                TextResourceCounts::default(),
                "{label}: a dead generation's leases go, whatever the window"
            );
        }
    }

    /// A view for a buffer the asking generation does not hold is refused
    /// before anything is allocated.
    #[test]
    fn a_view_cannot_be_opened_on_a_buffer_this_generation_does_not_lease() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);

        assert_eq!(
            serve(&mut host, G18, TextOperation::CreateView { buffer }),
            Err(TextRefusal::NoSuchResource)
        );
        assert_eq!(host.counts().live_views, 0, "and nothing was allocated");
    }
}

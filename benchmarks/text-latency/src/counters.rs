//! Work/traffic counters, and the validation that catches a guest-side
//! whole-document materialization even when wire traffic looks cheap.

#[derive(Debug, Clone, Copy, Default)]
pub struct BoundaryCounters {
    pub event_rx_bytes: u64,
    pub layout_text_bytes: u64,
    pub scene_bytes: u64,
    pub other_guest_host_bytes: u64,
}

impl BoundaryCounters {
    pub fn total(&self) -> u64 {
        self.event_rx_bytes + self.layout_text_bytes + self.scene_bytes + self.other_guest_host_bytes
    }
}

/// Guest-reported work: how much of the document the guest's own code
/// touched building this sample's presentation, independent of what
/// actually crossed WIT. This is what the whole-document-copy mutant checks
/// -- see [`no_whole_document_materialization`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GuestWorkCounters {
    pub document_bytes_materialized: u64,
    pub visible_bytes_projected: u64,
}

/// The critical assertion this benchmark exists partly to prove: editing a
/// row inside a large document must not materialize the whole document,
/// even when WIT traffic for that same edit looks identical across document
/// sizes (a guest can do `document.as_string()` locally and only slice a
/// small piece for the wire -- see benchmarks/text-latency/README.md).
///
/// `visible_window_bytes` is the presented row/window's own size (a small,
/// roughly-constant quantity regardless of document size); `tolerance`
/// multiplies it to allow real slack (line lookahead, grapheme boundary
/// snapping) without allowing O(document-size) growth to slip through.
pub fn no_whole_document_materialization(
    materialized: u64,
    visible_window_bytes: u64,
    tolerance: u64,
) -> Result<(), String> {
    let bound = visible_window_bytes.saturating_mul(tolerance.max(1));
    if materialized > bound {
        return Err(format!(
            "document_bytes_materialized ({materialized}) exceeds {tolerance}x the visible \
             window ({visible_window_bytes} bytes, bound {bound}) -- this looks like a \
             guest-side whole-document copy (e.g. `document.as_string()`), which stays \
             invisible to any counter that only measures bytes crossing WIT"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod mutant_tests {
    use super::*;

    /// Mutant: a guest that does `document.as_string()` (materializing the
    /// entire 10 MiB document) and only slices 2 KiB for the wire. WIT
    /// traffic alone would report this as identical to a small document --
    /// the point of this counter is that it does not.
    #[test]
    fn whole_document_materialization_is_rejected_even_though_wire_traffic_is_small() {
        let ten_mib = 10 * 1024 * 1024;
        let visible_window = 2048;
        // The mutant: materialized == the whole document, not the window.
        let result = no_whole_document_materialization(ten_mib, visible_window, 8);
        let error = result.expect_err("whole-document materialization must be rejected");
        assert!(error.contains("whole-document"));
    }

    #[test]
    fn materialization_proportional_to_the_visible_window_is_accepted() {
        let visible_window = 2048;
        // Real slack: a bit more than exactly the window, never anywhere
        // near document size.
        let realistic = visible_window * 3;
        no_whole_document_materialization(realistic, visible_window, 8)
            .expect("proportional-to-window materialization must be accepted");
    }

    #[test]
    fn the_bound_does_not_silently_scale_with_document_size() {
        // Same visible window, two very different document sizes: the bound
        // itself (derived only from the window) must be identical, which is
        // what makes this catch O(document-size) work at any document size.
        let visible_window = 4096;
        let small_doc_bound_input = 4096 * 3;
        let large_doc_materialized_if_buggy = 10 * 1024 * 1024; // 10 MiB
        assert!(no_whole_document_materialization(small_doc_bound_input, visible_window, 8).is_ok());
        assert!(
            no_whole_document_materialization(large_doc_materialized_if_buggy, visible_window, 8)
                .is_err()
        );
    }
}

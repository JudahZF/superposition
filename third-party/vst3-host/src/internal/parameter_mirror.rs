//! Lock-free, latest-value feedback shared by the editor, processor, and loading thread.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub(crate) const MAX_MIRRORED_PARAMETERS: usize = 4096;
pub(crate) const MAX_OBSERVER_BATCH: usize = 128;

struct ValueSlot {
    dsp_value: AtomicU64,
    editor_value: AtomicU64,
    output_value: AtomicU64,
    refresh_value: AtomicU64,
    revision: AtomicU64,
    editor_revision: AtomicU64,
    output_revision: AtomicU64,
    refresh_revision: AtomicU64,
    dsp_dirty: AtomicBool,
    observer_dirty: AtomicBool,
    api_dirty: AtomicBool,
}

/// IDs and slots are fixed before the plug-in receives its component handler. An editor callback
/// only performs a binary search and atomic stores; a rapid drag cannot overflow a queue. Each
/// source has one publisher: the handler admits editor gestures only on its loading/UI thread,
/// processor output comes from the DSP thread, and refresh comes from the loading thread.
pub(crate) struct ParameterMirror {
    ids: Vec<u32>,
    slots: Vec<ValueSlot>,
    dsp_pending: AtomicBool,
    unknown_ids: AtomicU64,
    rejected_values: AtomicU64,
    rejected_off_thread_edits: AtomicU64,
    observer_cursor: AtomicUsize,
    refresh_requested: AtomicU64,
    refresh_seen: AtomicU64,
    refresh_cursor: AtomicUsize,
}

impl ParameterMirror {
    pub(crate) fn new(mut ids: Vec<u32>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        let slots = ids
            .iter()
            .map(|_| ValueSlot {
                dsp_value: AtomicU64::new(0),
                editor_value: AtomicU64::new(0),
                output_value: AtomicU64::new(0),
                refresh_value: AtomicU64::new(0),
                revision: AtomicU64::new(0),
                editor_revision: AtomicU64::new(0),
                output_revision: AtomicU64::new(0),
                refresh_revision: AtomicU64::new(0),
                dsp_dirty: AtomicBool::new(false),
                observer_dirty: AtomicBool::new(false),
                api_dirty: AtomicBool::new(false),
            })
            .collect();
        Self {
            ids,
            slots,
            dsp_pending: AtomicBool::new(false),
            unknown_ids: AtomicU64::new(0),
            rejected_values: AtomicU64::new(0),
            rejected_off_thread_edits: AtomicU64::new(0),
            observer_cursor: AtomicUsize::new(0),
            refresh_requested: AtomicU64::new(0),
            refresh_seen: AtomicU64::new(0),
            refresh_cursor: AtomicUsize::new(0),
        }
    }

    pub(crate) fn publish_editor(&self, id: u32, value: f64) -> bool {
        let Some(slot) = self.slot(id, value) else {
            return false;
        };
        slot.dsp_value.store(value.to_bits(), Ordering::Release);
        slot.dsp_dirty.store(true, Ordering::Release);
        slot.editor_value.store(value.to_bits(), Ordering::Release);
        let revision = slot.revision.fetch_add(1, Ordering::AcqRel) + 1;
        slot.editor_revision.store(revision, Ordering::Release);
        slot.observer_dirty.store(true, Ordering::Release);
        slot.api_dirty.store(true, Ordering::Release);
        self.dsp_pending.store(true, Ordering::Release);
        true
    }

    pub(crate) fn publish_output(&self, id: u32, value: f64) -> bool {
        let Some(slot) = self.slot(id, value) else {
            return false;
        };
        slot.output_value.store(value.to_bits(), Ordering::Release);
        let revision = slot.revision.fetch_add(1, Ordering::AcqRel) + 1;
        slot.output_revision.store(revision, Ordering::Release);
        slot.observer_dirty.store(true, Ordering::Release);
        true
    }

    pub(crate) fn revision(&self, id: u32) -> Option<u64> {
        let index = self.ids.binary_search(&id).ok()?;
        Some(self.slots[index].revision.load(Ordering::Acquire))
    }

    /// The controller read happens outside this method. A callback that publishes during that
    /// read advances `revision`, so the CAS refuses its stale result. The refresh uses a separate
    /// value cell; even a callback immediately after a successful CAS cannot be overwritten.
    pub(crate) fn publish_refresh_if_unchanged(&self, id: u32, value: f64, baseline: u64) -> bool {
        let Some(slot) = self.slot(id, value) else {
            return false;
        };
        let Ok(_) = slot.revision.compare_exchange(
            baseline,
            baseline.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) else {
            return false;
        };
        slot.refresh_value.store(value.to_bits(), Ordering::Release);
        slot.refresh_revision
            .store(baseline.wrapping_add(1), Ordering::Release);
        slot.observer_dirty.store(true, Ordering::Release);
        slot.api_dirty.store(true, Ordering::Release);
        true
    }

    pub(crate) fn request_controller_refresh(&self) {
        self.refresh_requested.fetch_add(1, Ordering::Release);
    }

    /// Finish the current sweep before starting a newer generation. Repeated restart requests
    /// cannot keep resetting the cursor and starving high parameter IDs.
    pub(crate) fn next_refresh_ids(&self, max: usize) -> &[u32] {
        let requested = self.refresh_requested.load(Ordering::Acquire);
        let seen = self.refresh_seen.load(Ordering::Relaxed);
        let cursor = self.refresh_cursor.load(Ordering::Relaxed);
        if cursor >= self.ids.len() && requested != seen {
            self.refresh_cursor.store(0, Ordering::Relaxed);
            self.refresh_seen.store(requested, Ordering::Relaxed);
        } else if seen == 0 && requested != 0 {
            self.refresh_seen.store(requested, Ordering::Relaxed);
        }
        if requested == 0 {
            return &[];
        }
        let start = self.refresh_cursor.load(Ordering::Relaxed);
        let end = start.saturating_add(max).min(self.ids.len());
        self.refresh_cursor.store(end, Ordering::Relaxed);
        &self.ids[start..end]
    }

    fn slot(&self, id: u32, value: f64) -> Option<&ValueSlot> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            self.rejected_values.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        match self.ids.binary_search(&id) {
            Ok(index) => Some(&self.slots[index]),
            Err(_) => {
                self.unknown_ids.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Called by the processor. No mutex, heap allocation, or plug-in/controller call.
    pub(crate) fn drain_dsp(&self, mut apply: impl FnMut(u32, f64) -> bool) {
        if !self.dsp_pending.swap(false, Ordering::AcqRel) {
            return;
        }
        for (id, slot) in self.ids.iter().zip(&self.slots) {
            if slot.dsp_dirty.swap(false, Ordering::AcqRel)
                && !apply(*id, f64::from_bits(slot.dsp_value.load(Ordering::Acquire)))
            {
                slot.dsp_dirty.store(true, Ordering::Release);
                self.dsp_pending.store(true, Ordering::Release);
            }
        }
    }

    pub(crate) fn take_observer(&self, max: usize) -> Vec<(u32, f64)> {
        let max = max.min(MAX_OBSERVER_BATCH);
        let len = self.ids.len();
        if max == 0 || len == 0 {
            return Vec::new();
        }
        let start = self.observer_cursor.load(Ordering::Relaxed) % len;
        let mut values = Vec::with_capacity(max.min(len));
        let mut scanned = 0;
        while scanned < len && values.len() < max {
            let index = (start + scanned) % len;
            let slot = &self.slots[index];
            if slot.observer_dirty.swap(false, Ordering::AcqRel) {
                values.push((self.ids[index], Self::observer_value(slot)));
            }
            scanned += 1;
        }
        self.observer_cursor
            .store((start + scanned) % len, Ordering::Relaxed);
        values
    }

    pub(crate) fn take_api(&self) -> Vec<(u32, f64)> {
        let mut values = Vec::new();
        for (id, slot) in self.ids.iter().zip(&self.slots) {
            if slot.api_dirty.swap(false, Ordering::AcqRel) {
                values.push((*id, Self::observer_value(slot)));
            }
        }
        values
    }

    fn observer_value(slot: &ValueSlot) -> f64 {
        let editor_revision = slot.editor_revision.load(Ordering::Acquire);
        let output_revision = slot.output_revision.load(Ordering::Acquire);
        let refresh_revision = slot.refresh_revision.load(Ordering::Acquire);
        let bits = if refresh_revision > editor_revision && refresh_revision > output_revision {
            slot.refresh_value.load(Ordering::Acquire)
        } else if output_revision > editor_revision {
            slot.output_value.load(Ordering::Acquire)
        } else {
            slot.editor_value.load(Ordering::Acquire)
        };
        f64::from_bits(bits)
    }

    pub(crate) fn unknown_id_count(&self) -> u64 {
        self.unknown_ids.load(Ordering::Relaxed)
    }

    pub(crate) fn feedback_loss_count(&self) -> u64 {
        self.unknown_ids.load(Ordering::Relaxed)
            + self.rejected_values.load(Ordering::Relaxed)
            + self.rejected_off_thread_edits.load(Ordering::Relaxed)
    }

    pub(crate) fn record_off_thread_edit(&self) {
        self.rejected_off_thread_edits
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_values_survive_long_drags_and_independent_drains() {
        let mirror = ParameterMirror::new(vec![7, 5]);
        for index in 0..8192 {
            assert!(mirror.publish_editor(7, index as f64 / 8192.0));
        }
        assert!(!mirror.publish_editor(9, 0.5));
        assert_eq!(mirror.unknown_id_count(), 1);
        let mut dsp = Vec::new();
        mirror.drain_dsp(|id, value| {
            dsp.push((id, value));
            true
        });
        assert_eq!(dsp, vec![(7, 8191.0 / 8192.0)]);
        assert_eq!(mirror.take_observer(1), dsp);
        assert_eq!(mirror.take_api(), dsp);
        assert!(mirror.take_observer(128).is_empty());
        assert!(mirror.publish_output(5, 0.25));
        assert_eq!(mirror.take_observer(128), vec![(5, 0.25)]);
        mirror.drain_dsp(|_, _| panic!("processor output must not feed input"));
    }

    #[test]
    fn refresh_finishes_then_restarts_if_another_preset_arrives_mid_sweep() {
        let mirror = ParameterMirror::new((0..300).collect());
        mirror.request_controller_refresh();
        assert_eq!(mirror.next_refresh_ids(128), &(0..128).collect::<Vec<_>>());
        assert_eq!(
            mirror.next_refresh_ids(128),
            &(128..256).collect::<Vec<_>>()
        );
        mirror.request_controller_refresh();
        assert_eq!(
            mirror.next_refresh_ids(128),
            &(256..300).collect::<Vec<_>>()
        );
        assert_eq!(mirror.next_refresh_ids(128), &(0..128).collect::<Vec<_>>());
        assert_eq!(
            mirror.next_refresh_ids(128),
            &(128..256).collect::<Vec<_>>()
        );
        assert_eq!(
            mirror.next_refresh_ids(128),
            &(256..300).collect::<Vec<_>>()
        );
        assert!(mirror.next_refresh_ids(128).is_empty());
    }

    #[test]
    fn output_and_refresh_cannot_replace_pending_editor_dsp_input() {
        let mirror = ParameterMirror::new(vec![5]);
        assert!(mirror.publish_editor(5, 0.25));
        assert!(mirror.publish_output(5, 0.75));
        let baseline = mirror.revision(5).unwrap();
        assert!(mirror.publish_refresh_if_unchanged(5, 0.5, baseline));
        let mut dsp = Vec::new();
        mirror.drain_dsp(|id, value| {
            dsp.push((id, value));
            true
        });
        assert_eq!(dsp, vec![(5, 0.25)]);
        assert_eq!(mirror.take_observer(128), vec![(5, 0.5)]);
    }

    #[test]
    fn hot_low_ids_do_not_starve_high_id_observer_feedback() {
        let mirror = ParameterMirror::new((0..300).collect());
        for id in 0..300 {
            assert!(mirror.publish_output(id, 0.25));
        }
        let first = mirror.take_observer(128);
        assert_eq!(first.len(), 128);
        for id in 0..128 {
            assert!(mirror.publish_output(id, 0.5));
        }
        let second = mirror.take_observer(128);
        assert_eq!(second.first(), Some(&(128, 0.25)));
        assert!(second.iter().any(|(id, _)| *id == 255));
        for id in 0..128 {
            assert!(mirror.publish_output(id, 0.75));
        }
        let third = mirror.take_observer(128);
        assert!(third.iter().any(|(id, _)| *id == 299));
    }

    #[test]
    fn stale_controller_refresh_does_not_overwrite_newer_event() {
        let mirror = ParameterMirror::new(vec![5]);
        let baseline = mirror.revision(5).unwrap();
        assert!(mirror.publish_editor(5, 0.8));
        assert!(!mirror.publish_refresh_if_unchanged(5, 0.2, baseline));
        assert_eq!(mirror.take_observer(128), vec![(5, 0.8)]);
        assert_eq!(mirror.take_api(), vec![(5, 0.8)]);
        let mut dsp = Vec::new();
        mirror.drain_dsp(|id, value| {
            dsp.push((id, value));
            true
        });
        assert_eq!(dsp, vec![(5, 0.8)]);
    }

    #[test]
    fn rejected_refresh_cannot_corrupt_previous_accepted_value() {
        let mirror = ParameterMirror::new(vec![5]);
        assert!(mirror.publish_refresh_if_unchanged(5, 0.3, 0));
        let slot = &mirror.slots[0];
        // Simulate an editor writer between its global revision claim and publication of its
        // source revision. The rejected refresh must not change the earlier accepted cell.
        slot.revision.store(2, Ordering::Release);
        assert!(!mirror.publish_refresh_if_unchanged(5, 0.9, 1));
        assert_eq!(mirror.take_observer(128), vec![(5, 0.3)]);
        slot.editor_value.store(0.8f64.to_bits(), Ordering::Release);
        slot.editor_revision.store(2, Ordering::Release);
        slot.observer_dirty.store(true, Ordering::Release);
        assert_eq!(mirror.take_observer(128), vec![(5, 0.8)]);
    }

    #[test]
    fn interleaved_editor_and_processor_sources_keep_newest_revision() {
        let mirror = ParameterMirror::new(vec![5]);
        let slot = &mirror.slots[0];
        slot.editor_value.store(0.2f64.to_bits(), Ordering::Release);
        assert!(mirror.publish_output(5, 0.7));
        // Editor completes after the output callback, despite storing its value first.
        let revision = slot.revision.fetch_add(1, Ordering::AcqRel) + 1;
        slot.editor_revision.store(revision, Ordering::Release);
        slot.observer_dirty.store(true, Ordering::Release);
        assert_eq!(mirror.take_observer(128), vec![(5, 0.2)]);
    }

    #[test]
    fn rejected_dsp_enqueue_retries_latest_editor_value() {
        let mirror = ParameterMirror::new(vec![5]);
        assert!(mirror.publish_editor(5, 0.2));
        mirror.drain_dsp(|_, _| false);
        assert!(mirror.publish_editor(5, 0.7));
        let mut delivered = Vec::new();
        mirror.drain_dsp(|id, value| {
            delivered.push((id, value));
            true
        });
        assert_eq!(delivered, vec![(5, 0.7)]);
        mirror.drain_dsp(|_, _| panic!("successful value must not repeat"));
    }
}

//! Bounded, non-consuming worker-to-host parameter snapshots.

use std::sync::atomic::{AtomicU64, Ordering};

use sp_protocol::MAX_PLUGINS_PER_RACK;

/// Maximum distinct edited parameters retained for one plug-in instance.
pub const PARAMETER_FEEDBACK_CAPACITY_PER_SLOT: usize = 4096;

#[repr(C)]
pub(crate) struct ParameterFeedbackCell {
    // Zero is empty; every u32 parameter ID maps to a nonzero u64 key.
    key: AtomicU64,
    value_bits: AtomicU64,
}

impl ParameterFeedbackCell {
    pub(crate) const fn new() -> Self {
        Self {
            key: AtomicU64::new(0),
            value_bits: AtomicU64::new(0),
        }
    }
}

#[repr(C, align(64))]
pub(crate) struct ParameterFeedbackSlot {
    // Even nonzero epochs are stable. Odd epochs mark an in-progress reset.
    pub(crate) epoch: AtomicU64,
    pub(crate) revision: AtomicU64,
    pub(crate) overflow_count: AtomicU64,
    pub(crate) restart_counts: [AtomicU64; 32],
    pub(crate) cells: [ParameterFeedbackCell; PARAMETER_FEEDBACK_CAPACITY_PER_SLOT],
}

/// Host-visible latest-value table, with one single-writer partition per rack slot.
///
/// The worker's main thread writes parameter values. Its exclusive runtime owner writes restart
/// counters; runtime handoff serializes those writes with main-thread slot reset. The host reads
/// concurrently from another mapping. The device callback never accesses this table.
#[repr(C, align(64))]
pub struct ParameterFeedbackBank {
    pub(crate) slots: [ParameterFeedbackSlot; MAX_PLUGINS_PER_RACK],
}

/// An epoch-consistent snapshot of one plug-in slot's latest editor values.
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterFeedbackSnapshot {
    /// Zero-based plug-in slot in the rack.
    pub slot_index: usize,
    /// Even, nonzero instance epoch; changes whenever the slot is reset.
    pub epoch: u64,
    /// Completed publications within this epoch. Values may be visible just before this advances.
    pub revision: u64,
    /// Sticky number of table refusals or upstream feedback/container losses.
    pub overflow_count: u64,
    /// All parameter IDs published in this epoch, each with its latest normalized value.
    pub values: Vec<(u32, f64)>,
}

/// Host-local acknowledgements for one slot's restart requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RestartCursor {
    slot_index: Option<usize>,
    epoch: u64,
    acknowledged: [u64; 32],
}

/// Non-consuming restart requests from one plug-in instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartSnapshot {
    /// Zero-based plug-in slot.
    pub slot_index: usize,
    /// Instance epoch; changes when the worker replaces this slot.
    pub epoch: u64,
    /// Pending VST3 restart bits. Each bit has its own publication counter.
    pub flags: u32,
    counts: [u64; 32],
}

impl RestartCursor {
    /// Acknowledge only completed bits from this snapshot. Later publications stay pending.
    pub fn ack(&mut self, snapshot: &RestartSnapshot, completed_flags: u32) {
        if self.slot_index == Some(snapshot.slot_index) && self.epoch > snapshot.epoch {
            return;
        }
        if self.epoch != snapshot.epoch || self.slot_index != Some(snapshot.slot_index) {
            self.slot_index = Some(snapshot.slot_index);
            self.epoch = snapshot.epoch;
            self.acknowledged = [0; 32];
        }
        for bit in 0..32 {
            if completed_flags & snapshot.flags & (1_u32 << bit) != 0 {
                self.acknowledged[bit] = self.acknowledged[bit].max(snapshot.counts[bit]);
            }
        }
    }
}

/// Rejection from a bounded parameter feedback publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterFeedbackError {
    /// Plug-in slot index is outside the fixed rack capacity.
    InvalidSlot,
    /// The slot has not been initialized or is being reset.
    InactiveSlot,
    /// Value is not finite or outside the normalized range.
    InvalidValue,
    /// All 4,096 distinct parameter keys are occupied.
    Full,
}

impl ParameterFeedbackBank {
    pub(crate) fn clear_for_replacement(&mut self) {
        for slot in &mut self.slots {
            slot.epoch.store(0, Ordering::SeqCst);
            slot.revision.store(0, Ordering::Release);
            slot.overflow_count.store(0, Ordering::Release);
            for count in &slot.restart_counts {
                count.store(0, Ordering::Release);
            }
            for cell in &mut slot.cells {
                cell.key.store(0, Ordering::Release);
                cell.value_bits.store(0, Ordering::Release);
            }
        }
    }

    /// Start a fresh plug-in instance. Only the worker's main thread may call this.
    pub fn reset_slot(&self, slot_index: usize) -> Option<u64> {
        let slot = self.slots.get(slot_index)?;
        let prior = slot.epoch.load(Ordering::SeqCst);
        let next = prior.checked_add(2)?;
        if prior & 1 != 0 {
            return None;
        }
        slot.epoch.store(prior + 1, Ordering::SeqCst);
        slot.revision.store(0, Ordering::Release);
        slot.overflow_count.store(0, Ordering::Release);
        for count in &slot.restart_counts {
            count.store(0, Ordering::Release);
        }
        for cell in &slot.cells {
            cell.key.store(0, Ordering::Release);
            cell.value_bits.store(0, Ordering::Release);
        }
        slot.epoch.store(next, Ordering::SeqCst);
        Some(next)
    }

    /// Return the current stable instance epoch, or `None` during reset.
    #[must_use]
    pub fn slot_epoch(&self, slot_index: usize) -> Option<u64> {
        let epoch = self.slots.get(slot_index)?.epoch.load(Ordering::SeqCst);
        (epoch != 0 && epoch & 1 == 0).then_some(epoch)
    }

    /// Publish restart bits for one active slot. The worker's runtime owner is its only writer.
    pub fn publish_restart(&self, slot_index: usize, flags: u32) -> bool {
        let Some(slot) = self.slots.get(slot_index) else {
            return false;
        };
        if self.slot_epoch(slot_index).is_none() {
            return false;
        }
        for bit in 0..32 {
            if flags & (1_u32 << bit) != 0 {
                slot.restart_counts[bit].fetch_add(1, Ordering::Release);
            }
        }
        true
    }

    /// Read pending bits without consuming them. Retry on a concurrent slot reset.
    #[must_use]
    pub fn restart_snapshot(
        &self,
        slot_index: usize,
        cursor: &RestartCursor,
    ) -> Option<RestartSnapshot> {
        let slot = self.slots.get(slot_index)?;
        let epoch = slot.epoch.load(Ordering::SeqCst);
        if epoch == 0 || epoch & 1 != 0 {
            return None;
        }
        let counts = std::array::from_fn(|bit| slot.restart_counts[bit].load(Ordering::Acquire));
        if slot.epoch.load(Ordering::SeqCst) != epoch {
            return None;
        }
        let mut flags = 0_u32;
        for (bit, count) in counts.iter().enumerate() {
            let acknowledged = if cursor.epoch == epoch && cursor.slot_index == Some(slot_index) {
                cursor.acknowledged[bit]
            } else {
                0
            };
            if *count > acknowledged {
                flags |= 1_u32 << bit;
            }
        }
        (flags != 0 || cursor.epoch != epoch || cursor.slot_index != Some(slot_index)).then_some(
            RestartSnapshot {
                slot_index,
                epoch,
                flags,
                counts,
            },
        )
    }

    /// Publish a latest value without allocation. Only the worker's main thread may call this.
    ///
    /// # Errors
    ///
    /// Rejects an invalid slot/value, an uninitialized slot, or a full distinct-key table.
    pub fn publish(
        &self,
        slot_index: usize,
        parameter_id: u32,
        normalized: f64,
    ) -> Result<(), ParameterFeedbackError> {
        let slot = self
            .slots
            .get(slot_index)
            .ok_or(ParameterFeedbackError::InvalidSlot)?;
        if !normalized.is_finite() || !(0.0..=1.0).contains(&normalized) {
            return Err(ParameterFeedbackError::InvalidValue);
        }
        if self.slot_epoch(slot_index).is_none() {
            return Err(ParameterFeedbackError::InactiveSlot);
        }
        let key = u64::from(parameter_id) + 1;
        let start = (parameter_id as usize).wrapping_mul(0x9e37_79b1)
            & (PARAMETER_FEEDBACK_CAPACITY_PER_SLOT - 1);
        for offset in 0..PARAMETER_FEEDBACK_CAPACITY_PER_SLOT {
            let cell = &slot.cells[(start + offset) & (PARAMETER_FEEDBACK_CAPACITY_PER_SLOT - 1)];
            let found = cell.key.load(Ordering::Acquire);
            if found == key || found == 0 {
                cell.value_bits
                    .store(normalized.to_bits(), Ordering::Release);
                if found == 0 {
                    cell.key.store(key, Ordering::Release);
                }
                slot.revision.fetch_add(1, Ordering::Release);
                return Ok(());
            }
        }
        slot.overflow_count.fetch_add(1, Ordering::Release);
        slot.revision.fetch_add(1, Ordering::Release);
        Err(ParameterFeedbackError::Full)
    }

    /// Record upstream feedback loss, so the host knows its mirror may be incomplete.
    pub fn report_overflow(&self, slot_index: usize, lost_values: u64) {
        if lost_values == 0 {
            return;
        }
        let Some(slot) = self.slots.get(slot_index) else {
            return;
        };
        if self.slot_epoch(slot_index).is_none() {
            return;
        }
        slot.overflow_count
            .fetch_add(lost_values, Ordering::Release);
        slot.revision.fetch_add(1, Ordering::Release);
    }

    /// Read a changed slot without consuming its values. Retry next poll on a concurrent reset.
    ///
    /// The returned values include every key published in this epoch, not only keys changed
    /// since `previous_revision`. The host should deduplicate against its last feedback values.
    /// An atomic value can appear one poll before its revision advances; the next poll then
    /// repeats it. This keeps previous values readable if a worker exits during publication.
    #[must_use]
    pub fn snapshot_changed(
        &self,
        slot_index: usize,
        previous_epoch: u64,
        previous_revision: u64,
    ) -> Option<ParameterFeedbackSnapshot> {
        let slot = self.slots.get(slot_index)?;
        let epoch = slot.epoch.load(Ordering::SeqCst);
        if epoch == 0 || epoch & 1 != 0 {
            return None;
        }
        let revision = slot.revision.load(Ordering::Acquire);
        if epoch == previous_epoch && revision == previous_revision {
            return None;
        }
        let mut values = Vec::new();
        for cell in &slot.cells {
            let key = cell.key.load(Ordering::Acquire);
            if key != 0 {
                let parameter_id = u32::try_from(key - 1).ok()?;
                values.push((
                    parameter_id,
                    f64::from_bits(cell.value_bits.load(Ordering::Acquire)),
                ));
            }
        }
        let overflow_count = slot.overflow_count.load(Ordering::Acquire);
        if slot.epoch.load(Ordering::SeqCst) != epoch
            || slot.revision.load(Ordering::Acquire) != revision
        {
            return None;
        }
        Some(ParameterFeedbackSnapshot {
            slot_index,
            epoch,
            revision,
            overflow_count,
            values,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SharedBank;
    use std::sync::Arc;

    #[test]
    fn restart_ack_preserves_new_same_bit_and_other_bits() {
        let bank = SharedBank::new(1).expect("bank");
        let feedback = &bank.feedback;
        feedback.reset_slot(2).expect("slot");
        let mut cursor = RestartCursor::default();
        assert!(feedback.publish_restart(2, 0b1011));
        let first = feedback.restart_snapshot(2, &cursor).expect("first");
        assert_eq!(first.flags, 0b1011);
        assert!(feedback.publish_restart(2, 0b0001));
        cursor.ack(&first, 0b0011);
        let pending = feedback.restart_snapshot(2, &cursor).expect("pending");
        assert_eq!(pending.flags, 0b1001);
        cursor.ack(&pending, pending.flags);
        assert!(feedback.restart_snapshot(2, &cursor).is_none());
    }

    #[test]
    fn restart_cursor_is_bound_to_slot_instance_epoch() {
        let bank = SharedBank::new(1).expect("bank");
        let feedback = &bank.feedback;
        let mut cursor = RestartCursor::default();
        let first_epoch = feedback.reset_slot(0).expect("first epoch");
        assert!(feedback.publish_restart(0, 0b1000));
        let first = feedback.restart_snapshot(0, &cursor).expect("first");
        cursor.ack(&first, first.flags);
        assert!(feedback.restart_snapshot(0, &cursor).is_none());
        let next_epoch = feedback.reset_slot(0).expect("next epoch");
        assert_ne!(first_epoch, next_epoch);
        let empty = feedback.restart_snapshot(0, &cursor).expect("epoch change");
        assert_eq!(empty.flags, 0);
        cursor.ack(&empty, 0);
        assert!(feedback.restart_snapshot(0, &cursor).is_none());
        assert!(feedback.publish_restart(0, 0b1000));
        let next = feedback.restart_snapshot(0, &cursor).expect("new instance");
        assert_eq!(next.flags, 0b1000);
        assert_eq!(next.epoch, next_epoch);
    }

    #[test]
    fn high_ids_and_repeated_edits_keep_latest_values() {
        let bank = SharedBank::new(1).expect("bank");
        let feedback = &bank.feedback;
        let epoch = feedback.reset_slot(3).expect("slot epoch");
        feedback.publish(3, 0, 0.25).expect("ID zero");
        feedback.publish(3, u32::MAX, 0.5).expect("highest ID");
        feedback.publish(3, 0, 0.75).expect("coalesced edit");
        let snapshot = feedback.snapshot_changed(3, 0, 0).expect("snapshot");
        assert_eq!(snapshot.epoch, epoch);
        assert_eq!(snapshot.revision, 3);
        assert!(snapshot.values.contains(&(0, 0.75)));
        assert!(snapshot.values.contains(&(u32::MAX, 0.5)));
        assert_eq!(snapshot.values.len(), 2);
        assert!(feedback.snapshot_changed(3, epoch, 3).is_none());
        feedback.report_overflow(3, 2);
        let incomplete = feedback.snapshot_changed(3, epoch, 3).expect("loss marker");
        assert_eq!(incomplete.overflow_count, 2);
        assert_eq!(incomplete.values.len(), 2);
    }

    #[test]
    fn reset_discards_old_instance_and_rejects_racy_snapshots() {
        let bank = Arc::new(SharedBank::new(1).expect("bank"));
        let feedback = &bank.feedback;
        let first = feedback.reset_slot(0).expect("first epoch");
        feedback.publish(0, 42, 0.2).expect("first edit");
        let reader = Arc::clone(&bank);
        let join = std::thread::spawn(move || {
            for _ in 0..100 {
                if let Some(snapshot) = reader.feedback.snapshot_changed(0, 0, 0) {
                    assert!(snapshot.epoch == first || snapshot.epoch == first + 2);
                    assert!(snapshot.values.iter().all(|(id, _)| *id == 42 || *id == 7));
                    assert!(
                        !(snapshot.epoch == first + 2
                            && snapshot.values.iter().any(|(id, _)| *id == 42))
                    );
                }
            }
        });
        let second = feedback.reset_slot(0).expect("new epoch");
        feedback.publish(0, 7, 0.8).expect("new edit");
        join.join().expect("reader");
        let snapshot = feedback
            .snapshot_changed(0, first, 1)
            .expect("new snapshot");
        assert_eq!(snapshot.epoch, second);
        assert_eq!(snapshot.values, vec![(7, 0.8)]);
    }

    #[test]
    fn values_remain_readable_after_writer_exits() {
        let bank = Arc::new(SharedBank::new(1).expect("bank"));
        let feedback = &bank.feedback;
        feedback.reset_slot(1).expect("slot epoch");
        let writer = Arc::clone(&bank);
        std::thread::spawn(move || writer.feedback.publish(1, 99, 0.625))
            .join()
            .expect("worker thread")
            .expect("publish");
        assert_eq!(
            feedback.snapshot_changed(1, 0, 0).expect("retained").values,
            vec![(99, 0.625)]
        );
    }

    #[test]
    fn full_table_reports_sticky_overflow() {
        let bank = SharedBank::new(1).expect("bank");
        let feedback = &bank.feedback;
        feedback.reset_slot(0).expect("slot epoch");
        for id in 0..u32::try_from(PARAMETER_FEEDBACK_CAPACITY_PER_SLOT).expect("capacity fits u32")
        {
            feedback.publish(0, id, 0.5).expect("capacity");
        }
        assert_eq!(
            feedback.publish(0, u32::MAX, 0.25),
            Err(ParameterFeedbackError::Full)
        );
        let snapshot = feedback.snapshot_changed(0, 0, 0).expect("snapshot");
        assert_eq!(snapshot.values.len(), PARAMETER_FEEDBACK_CAPACITY_PER_SLOT);
        assert_eq!(snapshot.overflow_count, 1);
    }
}

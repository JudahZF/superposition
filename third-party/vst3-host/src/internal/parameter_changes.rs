//! Preallocated VST3 parameter queues. All block-time operations are bounded and nonblocking.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use vst3::{Class, ComWrapper, Steinberg::Vst::*, Steinberg::*};

const MAX_QUEUES: usize = 4096;
const MAX_POINTS: usize = 8192;
const ID_INDEX_SLOTS: usize = 8192;
const END: u32 = u32::MAX;

struct EntryGuard<'a>(&'a AtomicBool);

impl<'a> EntryGuard<'a> {
    fn enter(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for EntryGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct PointNode {
    offset: AtomicI32,
    value: AtomicU64,
    next: AtomicU32,
}

impl PointNode {
    fn empty() -> Self {
        Self {
            offset: AtomicI32::new(0),
            value: AtomicU64::new(0),
            next: AtomicU32::new(END),
        }
    }
}

struct PointPool {
    nodes: Box<[PointNode]>,
    next: AtomicUsize,
    busy: AtomicBool,
}

struct IdIndexEntry {
    generation: AtomicU64,
    id: AtomicU32,
    queue_index: AtomicUsize,
}

impl IdIndexEntry {
    fn empty() -> Self {
        Self {
            generation: AtomicU64::new(0),
            id: AtomicU32::new(0),
            queue_index: AtomicUsize::new(0),
        }
    }
}

impl PointPool {
    fn new() -> Self {
        Self {
            nodes: (0..MAX_POINTS).map(|_| PointNode::empty()).collect(),
            next: AtomicUsize::new(0),
            busy: AtomicBool::new(false),
        }
    }

    fn allocate(&self, offset: i32, value: f64, next: u32) -> Option<u32> {
        let index = self.next.load(Ordering::Relaxed);
        let node = self.nodes.get(index)?;
        self.next
            .compare_exchange(index, index + 1, Ordering::AcqRel, Ordering::Relaxed)
            .ok()?;
        node.offset.store(offset, Ordering::Relaxed);
        node.value.store(value.to_bits(), Ordering::Relaxed);
        node.next.store(next, Ordering::Release);
        Some(index as u32)
    }

    fn clear(&self) {
        self.next.store(0, Ordering::Release);
    }
}

/// COM queue objects are allocated once on the control thread. A plug-in may retain a queue COM
/// reference after the parent collection drops, so each queue owns its pool through an `Arc`.
pub struct ParameterValueQueue {
    param_id: AtomicU32,
    head: AtomicU32,
    tail: AtomicU32,
    count: AtomicUsize,
    last_read_index: AtomicUsize,
    last_read_node: AtomicU32,
    busy: AtomicBool,
    pool: Arc<PointPool>,
    losses: Arc<AtomicU64>,
    #[cfg(test)]
    slow_insert_steps: AtomicUsize,
}

impl ParameterValueQueue {
    fn new(pool: Arc<PointPool>, losses: Arc<AtomicU64>) -> Self {
        Self {
            param_id: AtomicU32::new(0),
            head: AtomicU32::new(END),
            tail: AtomicU32::new(END),
            count: AtomicUsize::new(0),
            last_read_index: AtomicUsize::new(usize::MAX),
            last_read_node: AtomicU32::new(END),
            busy: AtomicBool::new(false),
            pool,
            losses,
            #[cfg(test)]
            slow_insert_steps: AtomicUsize::new(0),
        }
    }

    fn param_id(&self) -> u32 {
        self.param_id.load(Ordering::Acquire)
    }

    fn reset(&self, id: u32) -> bool {
        let Some(_pool_guard) = EntryGuard::enter(&self.pool.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.count.store(0, Ordering::Release);
        self.head.store(END, Ordering::Release);
        self.tail.store(END, Ordering::Release);
        self.last_read_index.store(usize::MAX, Ordering::Relaxed);
        self.last_read_node.store(END, Ordering::Relaxed);
        self.param_id.store(id, Ordering::Release);
        true
    }

    /// Insert in sample-offset order. Equal offsets replace in place, preserving the index.
    fn insert_point(&self, offset: i32, value: f64) -> Option<i32> {
        let Some(_pool_guard) = EntryGuard::enter(&self.pool.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.last_read_index.store(usize::MAX, Ordering::Relaxed);
        self.last_read_node.store(END, Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if tail != END {
            let tail_node = &self.pool.nodes[tail as usize];
            let tail_offset = tail_node.offset.load(Ordering::Relaxed);
            if offset >= tail_offset {
                let position = self.count.load(Ordering::Relaxed) - 1;
                if offset == tail_offset {
                    tail_node.value.store(value.to_bits(), Ordering::Release);
                    return Some(position as i32);
                }
                let Some(new_index) = self.pool.allocate(offset, value, END) else {
                    self.losses.fetch_add(1, Ordering::Relaxed);
                    return None;
                };
                tail_node.next.store(new_index, Ordering::Release);
                self.tail.store(new_index, Ordering::Release);
                self.count.fetch_add(1, Ordering::Release);
                return Some((position + 1) as i32);
            }
        }
        let mut previous = END;
        let mut current = self.head.load(Ordering::Acquire);
        let mut position = 0;
        while current != END {
            #[cfg(test)]
            self.slow_insert_steps.fetch_add(1, Ordering::Relaxed);
            let node = &self.pool.nodes[current as usize];
            let existing_offset = node.offset.load(Ordering::Relaxed);
            if existing_offset >= offset {
                if existing_offset == offset {
                    node.value.store(value.to_bits(), Ordering::Release);
                    return Some(position);
                }
                break;
            }
            previous = current;
            current = node.next.load(Ordering::Acquire);
            position += 1;
        }
        let Some(new_index) = self.pool.allocate(offset, value, current) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if previous == END {
            self.head.store(new_index, Ordering::Release);
        } else {
            self.pool.nodes[previous as usize]
                .next
                .store(new_index, Ordering::Release);
        }
        if current == END {
            self.tail.store(new_index, Ordering::Release);
        }
        self.count.fetch_add(1, Ordering::Release);
        Some(position)
    }

    fn point(&self, index: usize) -> Option<(i32, f64)> {
        let Some(_pool_guard) = EntryGuard::enter(&self.pool.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if index >= self.count.load(Ordering::Acquire) {
            return None;
        }
        let cached_index = self.last_read_index.load(Ordering::Relaxed);
        let (mut current, start) = if cached_index != usize::MAX && index == cached_index + 1 {
            let previous = self.last_read_node.load(Ordering::Relaxed);
            (
                self.pool.nodes[previous as usize]
                    .next
                    .load(Ordering::Acquire),
                index,
            )
        } else {
            (self.head.load(Ordering::Acquire), 0)
        };
        for _ in start..index {
            current = self.pool.nodes[current as usize]
                .next
                .load(Ordering::Acquire);
        }
        let node = &self.pool.nodes[current as usize];
        self.last_read_index.store(index, Ordering::Relaxed);
        self.last_read_node.store(current, Ordering::Relaxed);
        Some((
            node.offset.load(Ordering::Relaxed),
            f64::from_bits(node.value.load(Ordering::Acquire)),
        ))
    }

    fn for_each_point(&self, id: u32, f: &mut impl FnMut(u32, i32, f64)) {
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let mut current = self.head.load(Ordering::Acquire);
        while current != END {
            let node = &self.pool.nodes[current as usize];
            f(
                id,
                node.offset.load(Ordering::Relaxed),
                f64::from_bits(node.value.load(Ordering::Acquire)),
            );
            current = node.next.load(Ordering::Acquire);
        }
    }
}

impl Class for ParameterValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParameterValueQueue {
    unsafe fn getParameterId(&self) -> u32 {
        self.param_id()
    }

    unsafe fn getPointCount(&self) -> i32 {
        self.count.load(Ordering::Acquire) as i32
    }

    unsafe fn getPoint(&self, index: i32, sample_offset: *mut i32, value: *mut f64) -> i32 {
        let Some((offset, normalized)) = usize::try_from(index).ok().and_then(|i| self.point(i))
        else {
            return kResultFalse;
        };
        if !sample_offset.is_null() {
            *sample_offset = offset;
        }
        if !value.is_null() {
            *value = normalized;
        }
        kResultOk
    }

    unsafe fn addPoint(&self, sample_offset: i32, value: f64, index: *mut i32) -> i32 {
        let Some(position) = self.insert_point(sample_offset, value) else {
            return kResultFalse;
        };
        if !index.is_null() {
            *index = position;
        }
        kResultOk
    }
}

pub struct ParameterChanges {
    queues: Box<[ComWrapper<ParameterValueQueue>]>,
    id_index: Box<[IdIndexEntry]>,
    generation: AtomicU64,
    used: AtomicUsize,
    busy: AtomicBool,
    pool: Arc<PointPool>,
    losses: Arc<AtomicU64>,
}

impl ParameterChanges {
    pub fn new(losses: Arc<AtomicU64>) -> Self {
        let pool = Arc::new(PointPool::new());
        let queues = (0..MAX_QUEUES)
            .map(|_| ComWrapper::new(ParameterValueQueue::new(pool.clone(), losses.clone())))
            .collect();
        let id_index = (0..ID_INDEX_SLOTS).map(|_| IdIndexEntry::empty()).collect();
        Self {
            queues,
            id_index,
            generation: AtomicU64::new(1),
            used: AtomicUsize::new(0),
            busy: AtomicBool::new(false),
            pool,
            losses,
        }
    }

    /// Caller holds `busy`, so entry ID/index publication cannot race another lookup. The
    /// generation changes at `clear_all`, leaving the table reusable without 8192 writes per
    /// audio block. The table is half-full at the 4096-queue limit.
    fn find_id_slot(&self, id: u32) -> (Option<usize>, usize, usize) {
        let generation = self.generation.load(Ordering::Relaxed);
        // Mix high bits too: VST3 parameter IDs need not be dense or sequential.
        let mixed = (id ^ (id >> 16)).wrapping_mul(0x7FEB_352D);
        let hash = (mixed ^ (mixed >> 15)) as usize;
        let start = hash & (ID_INDEX_SLOTS - 1);
        for probe in 0..ID_INDEX_SLOTS {
            let slot_index = (start + probe) & (ID_INDEX_SLOTS - 1);
            let slot = &self.id_index[slot_index];
            if slot.generation.load(Ordering::Acquire) != generation {
                return (None, slot_index, probe + 1);
            }
            if slot.id.load(Ordering::Relaxed) == id {
                return (
                    Some(slot.queue_index.load(Ordering::Relaxed)),
                    slot_index,
                    probe + 1,
                );
            }
        }
        (None, ID_INDEX_SLOTS, ID_INDEX_SLOTS)
    }

    fn publish_id_slot(&self, id: u32, queue_index: usize, slot_index: usize) {
        let slot = &self.id_index[slot_index];
        slot.id.store(id, Ordering::Relaxed);
        slot.queue_index.store(queue_index, Ordering::Relaxed);
        slot.generation
            .store(self.generation.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Returns false when the bounded pool or a nonblocking entry guard refuses a point.
    pub fn enqueue(&self, id: u32, offset: i32, value: f64) -> bool {
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let used = self.used.load(Ordering::Acquire);
        let (existing, slot_index, _) = self.find_id_slot(id);
        if let Some(index) = existing {
            return self.queues[index].insert_point(offset, value).is_some();
        }
        let Some(queue) = self.queues.get(used) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if !queue.reset(id) || queue.insert_point(offset, value).is_none() {
            return false;
        }
        self.publish_id_slot(id, used, slot_index);
        self.used.store(used + 1, Ordering::Release);
        true
    }

    pub fn clear_all(&self) -> bool {
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let Some(_pool_guard) = EntryGuard::enter(&self.pool.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.used.store(0, Ordering::Release);
        self.pool.clear();
        let previous = self.generation.fetch_add(1, Ordering::AcqRel);
        if previous == u64::MAX {
            // Practically unreachable, but avoid stale generation-1 entries after wrap.
            for slot in &self.id_index {
                slot.generation.store(0, Ordering::Relaxed);
            }
            self.generation.store(1, Ordering::Release);
        }
        true
    }

    pub fn for_each_active_point(&self, mut f: impl FnMut(u32, i32, f64)) {
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(_pool_guard) = EntryGuard::enter(&self.pool.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let used = self.used.load(Ordering::Acquire);
        for queue in &self.queues[..used] {
            queue.for_each_point(queue.param_id(), &mut f);
        }
    }
}

impl Default for ParameterChanges {
    fn default() -> Self {
        Self::new(Arc::new(AtomicU64::new(0)))
    }
}

impl Class for ParameterChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParameterChanges {
    unsafe fn getParameterCount(&self) -> i32 {
        self.used.load(Ordering::Acquire) as i32
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        let Ok(index) = usize::try_from(index) else {
            return ptr::null_mut();
        };
        if index >= self.used.load(Ordering::Acquire) {
            return ptr::null_mut();
        }
        self.queues[index]
            .as_com_ref::<IParamValueQueue>()
            .map_or(ptr::null_mut(), |queue| queue.as_ptr())
    }

    unsafe fn addParameterData(&self, id: *const u32, index: *mut i32) -> *mut IParamValueQueue {
        if id.is_null() {
            return ptr::null_mut();
        }
        let Some(_guard) = EntryGuard::enter(&self.busy) else {
            self.losses.fetch_add(1, Ordering::Relaxed);
            return ptr::null_mut();
        };
        let id = *id;
        let used = self.used.load(Ordering::Acquire);
        let (existing, slot_index, _) = self.find_id_slot(id);
        let (position, queue) = if let Some(position) = existing {
            (position, &self.queues[position])
        } else {
            let Some(queue) = self.queues.get(used) else {
                self.losses.fetch_add(1, Ordering::Relaxed);
                return ptr::null_mut();
            };
            if !queue.reset(id) {
                return ptr::null_mut();
            }
            self.publish_id_slot(id, used, slot_index);
            self.used.store(used + 1, Ordering::Release);
            (used, queue)
        };
        if !index.is_null() {
            *index = position as i32;
        }
        queue
            .as_com_ref::<IParamValueQueue>()
            .map_or(ptr::null_mut(), |queue| queue.as_ptr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct TrackingAllocator;

    thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    }
    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe impl GlobalAlloc for TrackingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if TRACK_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: TrackingAllocator = TrackingAllocator;

    #[test]
    fn groups_orders_replaces_and_reuses_points_across_blocks() {
        let changes = ParameterChanges::default();
        assert!(changes.enqueue(7, 64, 0.9));
        assert!(changes.enqueue(7, 0, 0.5));
        assert!(changes.enqueue(7, 64, 0.8));
        assert!(changes.enqueue(3, 0, 0.1));
        assert_eq!(unsafe { changes.getParameterCount() }, 2);
        let mut points = Vec::new();
        changes.for_each_active_point(|id, offset, value| points.push((id, offset, value)));
        assert_eq!(points, vec![(7, 0, 0.5), (7, 64, 0.8), (3, 0, 0.1)]);
        assert_eq!(changes.pool.next.load(Ordering::Relaxed), 3);
        assert!(changes.clear_all());
        assert_eq!(unsafe { changes.getParameterCount() }, 0);
        assert!(changes.enqueue(9, 5, 0.25));
        assert_eq!(changes.pool.next.load(Ordering::Relaxed), 1);
        points.clear();
        changes.for_each_active_point(|id, offset, value| points.push((id, offset, value)));
        assert_eq!(points, vec![(9, 5, 0.25)]);
    }

    #[test]
    fn com_add_point_reports_index_and_overload_without_partial_insert() {
        let changes = ParameterChanges::default();
        let id = 42;
        let mut queue_index = -1;
        let queue_ptr = unsafe { changes.addParameterData(&id, &mut queue_index) };
        assert!(!queue_ptr.is_null());
        assert_eq!(queue_index, 0);
        let queue = &changes.queues[0];
        let mut point_index = -1;
        assert_eq!(
            unsafe { queue.addPoint(64, 0.6, &mut point_index) },
            kResultOk
        );
        assert_eq!(point_index, 0);
        assert_eq!(
            unsafe { queue.addPoint(0, 0.2, &mut point_index) },
            kResultOk
        );
        assert_eq!(point_index, 0);
        assert_eq!(
            unsafe { queue.addPoint(64, 0.8, &mut point_index) },
            kResultOk
        );
        assert_eq!(point_index, 1);
        assert_eq!(unsafe { queue.getPointCount() }, 2);
        assert_eq!(queue.point(0), Some((0, 0.2)));
        assert_eq!(queue.point(1), Some((64, 0.8)));
        assert!(changes.clear_all());
        // Descending offsets insert at the head in O(1), filling only the shared pool.
        for offset in (0..MAX_POINTS as i32).rev() {
            assert!(changes.enqueue(id, offset, 0.5));
        }
        assert!(!changes.enqueue(id, -1, 0.5));
        assert_eq!(changes.losses.load(Ordering::Relaxed), 1);
        assert_eq!(unsafe { changes.getParameterCount() }, 1);
        assert_eq!(
            unsafe { changes.queues[0].getPointCount() },
            MAX_POINTS as i32
        );
    }

    #[test]
    fn nonblocking_guards_refuse_reentry_and_count_loss() {
        let changes = ParameterChanges::default();
        changes.busy.store(true, Ordering::Release);
        assert!(!changes.enqueue(1, 0, 0.5));
        assert!(!changes.clear_all());
        changes.busy.store(false, Ordering::Release);
        assert!(changes.enqueue(1, 0, 0.5));
        changes.busy.store(true, Ordering::Release);
        assert!(!changes.clear_all());
        changes.busy.store(false, Ordering::Release);
        assert!(changes.clear_all());
        assert_eq!(unsafe { changes.getParameterCount() }, 0);
        assert!(changes.enqueue(1, 0, 0.5));
        let queue = &changes.queues[0];
        queue.busy.store(true, Ordering::Release);
        assert_eq!(
            unsafe { queue.addPoint(1, 0.7, ptr::null_mut()) },
            kResultFalse
        );
        assert_eq!(unsafe { queue.getPointCount() }, 1);
        queue.busy.store(false, Ordering::Release);
        assert_eq!(changes.losses.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn prepared_host_and_com_operations_do_not_allocate() {
        let changes = ParameterChanges::default();
        TRACK_ALLOCATIONS.with(|flag| flag.set(false));
        ALLOCATIONS.store(0, Ordering::Relaxed);
        TRACK_ALLOCATIONS.with(|flag| flag.set(true));
        let inserted = changes.enqueue(7, 64, 0.5);
        let mut queue_index = -1;
        let id = 7;
        let queue_ptr = unsafe { changes.addParameterData(&id, &mut queue_index) };
        let queue = &changes.queues[0];
        let mut point_index = -1;
        let result = unsafe { queue.addPoint(0, 0.25, &mut point_index) };
        let point = queue.point(0);
        let cleared = changes.clear_all();
        TRACK_ALLOCATIONS.with(|flag| flag.set(false));
        assert!(inserted && !queue_ptr.is_null() && cleared);
        assert_eq!(result, kResultOk);
        assert_eq!(point, Some((0, 0.25)));
        assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn sequential_index_reads_use_cursor_and_input_output_stay_independent() {
        let input = ParameterChanges::default();
        let output = ParameterChanges::default();
        for offset in 0..256 {
            assert!(input.enqueue(4, offset, f64::from(offset) / 256.0));
        }
        assert!(output.enqueue(4, 0, 0.75));
        let queue = &input.queues[0];
        for index in 0..256 {
            assert_eq!(
                queue.point(index),
                Some((index as i32, index as f64 / 256.0))
            );
        }
        assert_eq!(queue.last_read_index.load(Ordering::Relaxed), 255);
        assert_eq!(queue.point(3), Some((3, 3.0 / 256.0)));
        assert_eq!(queue.last_read_index.load(Ordering::Relaxed), 3);
        assert!(output.clear_all());
        assert_eq!(unsafe { input.getParameterCount() }, 1);
        assert_eq!(unsafe { output.getParameterCount() }, 0);
    }

    #[test]
    fn ascending_points_and_distinct_ids_have_bounded_hot_path_work() {
        let changes = ParameterChanges::default();
        for offset in 0..MAX_POINTS as i32 {
            assert!(changes.enqueue(17, offset, 0.5));
        }
        let queue = &changes.queues[0];
        assert_eq!(queue.slow_insert_steps.load(Ordering::Relaxed), 0);
        assert_eq!(queue.count.load(Ordering::Acquire), MAX_POINTS);
        assert!(changes.clear_all());

        let mut total_probes = 0;
        for index in 0..MAX_QUEUES as u32 {
            // A stride of 8192 collided completely under a low-bit-only hash.
            let id = index * ID_INDEX_SLOTS as u32;
            let (existing, _, probes) = changes.find_id_slot(id);
            assert!(existing.is_none());
            total_probes += probes;
            assert!(changes.enqueue(id, 0, 0.5));
            let (existing, _, probes) = changes.find_id_slot(id);
            assert_eq!(existing, Some(index as usize));
            total_probes += probes;
        }
        assert!(total_probes < MAX_QUEUES * 8, "hash probes: {total_probes}");
        assert_eq!(unsafe { changes.getParameterCount() }, MAX_QUEUES as i32);
        assert!(!changes.enqueue(u32::MAX, 0, 0.5));
        assert_eq!(changes.losses.load(Ordering::Relaxed), 1);
        assert!(changes.clear_all());
        assert!(changes.enqueue(17, 0, 0.7));
        assert_eq!(unsafe { changes.getParameterCount() }, 1);
        assert_eq!(changes.queues[0].point(0), Some((0, 0.7)));
    }
}

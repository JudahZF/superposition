//! Bounded host implementation of VST3's realtime data-exchange API.

use crossbeam_queue::ArrayQueue;
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::collections::VecDeque;
use std::ptr::{self, NonNull};
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread, ThreadId};
use vst3::{ComPtr, Steinberg::Vst::*, Steinberg::*};

use crate::plugin::{DataExchangeBlock as OwnedDataExchangeBlock, MainThreadServiceStats};

const MAX_QUEUES: usize = 32;
const QUEUE_INDEX_BITS: u32 = 5;
const MAX_BLOCK_SIZE: usize = 16 * 1024 * 1024;
const MAX_BLOCKS_PER_QUEUE: usize = 256;
const MAX_TOTAL_QUEUE_BYTES: usize = 256 * 1024 * 1024;
const MAX_SNAPSHOT_BLOCKS: usize = 1024;
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_FOREGROUND_BLOCKS_PER_PUMP: usize = 32;

const BLOCK_AVAILABLE: u8 = 0;
const BLOCK_LOCKED: u8 = 1;
const BLOCK_PENDING: u8 = 2;

struct AlignedBlock {
    ptr: NonNull<u8>,
    layout: Layout,
    state: AtomicU8,
}

unsafe impl Send for AlignedBlock {}
unsafe impl Sync for AlignedBlock {}

impl AlignedBlock {
    fn new(size: usize, alignment: usize) -> Option<Self> {
        if size == 0 {
            return None;
        }
        let layout = Layout::from_size_align(size, alignment).ok()?;
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })?;
        Some(Self {
            ptr,
            layout,
            state: AtomicU8::new(BLOCK_AVAILABLE),
        })
    }
}

impl Drop for AlignedBlock {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

struct ExchangeQueue {
    id: u32,
    user_context_id: u32,
    block_size: usize,
    total_bytes: usize,
    background: bool,
    blocks: Box<[AlignedBlock]>,
    available: ArrayQueue<u32>,
    pending: ArrayQueue<u32>,
}

impl ExchangeQueue {
    fn new(
        id: u32,
        user_context_id: u32,
        block_size: usize,
        num_blocks: usize,
        alignment: usize,
        background: bool,
    ) -> Option<Self> {
        let mut blocks = Vec::with_capacity(num_blocks);
        for _ in 0..num_blocks {
            blocks.push(AlignedBlock::new(block_size, alignment)?);
        }
        let available = ArrayQueue::new(num_blocks);
        for id in 0..num_blocks as u32 {
            available.push(id).ok()?;
        }
        Some(Self {
            id,
            user_context_id,
            block_size,
            total_bytes: block_size.checked_mul(num_blocks)?,
            background,
            blocks: blocks.into_boxed_slice(),
            available,
            pending: ArrayQueue::new(num_blocks),
        })
    }

    fn lock(&self, out: *mut vst3::Steinberg::Vst::DataExchangeBlock) -> tresult {
        let Some(block_id) = self.available.pop() else {
            return kOutOfMemory;
        };
        let Some(block) = self.blocks.get(block_id as usize) else {
            return kInternalError;
        };
        if block
            .state
            .compare_exchange(
                BLOCK_AVAILABLE,
                BLOCK_LOCKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return kInternalError;
        }
        unsafe {
            (*out).data = block.ptr.as_ptr().cast();
            (*out).size = self.block_size as u32;
            (*out).blockID = block_id;
        }
        kResultTrue
    }

    fn free(&self, block_id: u32, send: bool) -> tresult {
        let Some(block) = self.blocks.get(block_id as usize) else {
            return kInvalidArgument;
        };
        if block
            .state
            .compare_exchange(
                BLOCK_LOCKED,
                if send { BLOCK_PENDING } else { BLOCK_AVAILABLE },
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return kInvalidArgument;
        }
        let pushed = if send {
            self.pending.push(block_id)
        } else {
            self.available.push(block_id)
        };
        if pushed.is_err() {
            block.state.store(BLOCK_LOCKED, Ordering::Release);
            return kInternalError;
        }
        kResultTrue
    }

    fn recycle(&self, block_id: u32) {
        if let Some(block) = self.blocks.get(block_id as usize) {
            block.state.store(BLOCK_AVAILABLE, Ordering::Release);
            let _ = self.available.push(block_id);
        }
    }
}

#[derive(Default)]
struct SnapshotSink {
    blocks: VecDeque<OwnedDataExchangeBlock>,
    bytes: usize,
}

/// State shared by the host context, the audio callback, and one background dispatcher.
pub struct DataExchangeState {
    queues: [AtomicPtr<ExchangeQueue>; MAX_QUEUES],
    generations: [AtomicU32; MAX_QUEUES],
    lifecycle: Mutex<()>,
    receiver: Mutex<Option<ComPtr<IDataExchangeReceiver>>>,
    processor: AtomicPtr<IAudioProcessor>,
    active: AtomicBool,
    in_process: AtomicBool,
    allocated_bytes: AtomicUsize,
    snapshots: Mutex<SnapshotSink>,
    control_thread: ThreadId,
    worker_stop: AtomicBool,
    worker_thread: OnceLock<Thread>,
    worker_join: Mutex<Option<JoinHandle<()>>>,
    worker_progress: (Mutex<u64>, Condvar),
    dispatch_progress: (Mutex<usize>, Condvar),
    shutting_down: AtomicBool,
    receiver_supported: AtomicBool,
    queues_opened: AtomicU64,
    blocks_sent: AtomicU64,
    foreground_blocks_delivered: AtomicU64,
    background_blocks_delivered: AtomicU64,
    next_foreground_queue: AtomicUsize,
}

unsafe impl Send for DataExchangeState {}
unsafe impl Sync for DataExchangeState {}

struct DispatchLease<'a>(&'a DataExchangeState);

impl Drop for DispatchLease<'_> {
    fn drop(&mut self) {
        let (count, wake) = &self.0.dispatch_progress;
        let mut count = count.lock().unwrap_or_else(|p| p.into_inner());
        *count -= 1;
        wake.notify_all();
    }
}

impl DataExchangeState {
    fn begin_dispatch(&self) -> DispatchLease<'_> {
        let (count, _) = &self.dispatch_progress;
        *count.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        DispatchLease(self)
    }

    fn wait_for_dispatches(&self) {
        let (count, wake) = &self.dispatch_progress;
        let mut count = count.lock().unwrap_or_else(|p| p.into_inner());
        while *count != 0 {
            count = wake.wait(count).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Acquire a queue lease while the registry is protected. Dispatch can then call plugin
    /// code without holding `lifecycle`, even if that code closes or opens a queue.
    fn lease_queue(ptr: *mut ExchangeQueue) -> Arc<ExchangeQueue> {
        unsafe {
            Arc::increment_strong_count(ptr);
            Arc::from_raw(ptr)
        }
    }
    pub fn new() -> Arc<Self> {
        let state = Arc::new(Self {
            queues: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            generations: std::array::from_fn(|_| AtomicU32::new(0)),
            lifecycle: Mutex::new(()),
            receiver: Mutex::new(None),
            processor: AtomicPtr::new(ptr::null_mut()),
            active: AtomicBool::new(false),
            in_process: AtomicBool::new(false),
            allocated_bytes: AtomicUsize::new(0),
            snapshots: Mutex::new(SnapshotSink::default()),
            control_thread: thread::current().id(),
            worker_stop: AtomicBool::new(false),
            worker_thread: OnceLock::new(),
            worker_join: Mutex::new(None),
            worker_progress: (Mutex::new(0), Condvar::new()),
            dispatch_progress: (Mutex::new(0), Condvar::new()),
            shutting_down: AtomicBool::new(false),
            receiver_supported: AtomicBool::new(false),
            queues_opened: AtomicU64::new(0),
            blocks_sent: AtomicU64::new(0),
            foreground_blocks_delivered: AtomicU64::new(0),
            background_blocks_delivered: AtomicU64::new(0),
            next_foreground_queue: AtomicUsize::new(0),
        });
        let weak = Arc::downgrade(&state);
        let join = thread::Builder::new()
            .name("vst3-data-exchange".to_string())
            .spawn(move || {
                loop {
                    thread::park();
                    let Some(state) = weak.upgrade() else {
                        break;
                    };
                    if state.worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    state.dispatch_matching(true, true);
                    let (sequence, wake) = &state.worker_progress;
                    let mut sequence = sequence.lock().unwrap_or_else(|p| p.into_inner());
                    *sequence = sequence.wrapping_add(1);
                    wake.notify_all();
                }
            })
            .expect("failed to spawn VST3 data-exchange dispatcher");
        let _ = state.worker_thread.set(join.thread().clone());
        *state.worker_join.lock().unwrap_or_else(|p| p.into_inner()) = Some(join);
        state
    }

    pub fn configure(
        &self,
        processor: *mut IAudioProcessor,
        receiver: Option<ComPtr<IDataExchangeReceiver>>,
    ) {
        self.processor.store(processor, Ordering::Release);
        self.receiver_supported
            .store(receiver.is_some(), Ordering::Release);
        *self.receiver.lock().unwrap_or_else(|p| p.into_inner()) = receiver;
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub fn enter_process(&self) {
        self.in_process.store(true, Ordering::Release);
    }

    pub fn leave_process(&self) {
        self.in_process.store(false, Ordering::Release);
    }

    fn queue(&self, id: u32) -> Option<&ExchangeQueue> {
        let slot = (id & ((1 << QUEUE_INDEX_BITS) - 1)) as usize;
        let ptr = self.queues.get(slot)?.load(Ordering::Acquire);
        let queue = unsafe { ptr.as_ref()? };
        (queue.id == id).then_some(queue)
    }

    fn reserve_bytes(&self, bytes: usize) -> bool {
        self.allocated_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= MAX_TOTAL_QUEUE_BYTES)
            })
            .is_ok()
    }

    pub unsafe fn open_queue(
        &self,
        processor: *mut IAudioProcessor,
        block_size: u32,
        num_blocks: u32,
        alignment: u32,
        user_context_id: u32,
        out_id: *mut u32,
    ) -> tresult {
        if out_id.is_null() {
            return kInvalidArgument;
        }
        *out_id = InvalidDataExchangeQueueID;
        if thread::current().id() != self.control_thread
            || self.active.load(Ordering::Acquire)
            || processor.is_null()
            || processor != self.processor.load(Ordering::Acquire)
        {
            return kInvalidArgument;
        }
        let (block_size, num_blocks) = (block_size as usize, num_blocks as usize);
        if block_size == 0
            || block_size > MAX_BLOCK_SIZE
            || num_blocks == 0
            || num_blocks > MAX_BLOCKS_PER_QUEUE
        {
            return kInvalidArgument;
        }
        let alignment = if alignment == 0 {
            std::mem::align_of::<u128>()
        } else {
            alignment as usize
        };
        if !alignment.is_power_of_two() {
            return kInvalidArgument;
        }
        let Some(total_bytes) = block_size.checked_mul(num_blocks) else {
            return kInvalidArgument;
        };
        if !self.reserve_bytes(total_bytes) {
            return kOutOfMemory;
        }

        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(receiver) = receiver else {
            self.allocated_bytes
                .fetch_sub(total_bytes, Ordering::AcqRel);
            return kNoInterface;
        };
        let Some(mut queue) =
            ExchangeQueue::new(0, user_context_id, block_size, num_blocks, alignment, false)
        else {
            self.allocated_bytes
                .fetch_sub(total_bytes, Ordering::AcqRel);
            return kOutOfMemory;
        };
        let mut background = 0;
        receiver.queueOpened(user_context_id, block_size as u32, &mut background);
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        if self.shutting_down.load(Ordering::Acquire) {
            drop(_lifecycle);
            receiver.queueClosed(user_context_id);
            self.allocated_bytes
                .fetch_sub(total_bytes, Ordering::AcqRel);
            return kInvalidArgument;
        }
        let Some(slot) = self
            .queues
            .iter()
            .position(|slot| slot.load(Ordering::Acquire).is_null())
        else {
            drop(_lifecycle);
            receiver.queueClosed(user_context_id);
            self.allocated_bytes
                .fetch_sub(total_bytes, Ordering::AcqRel);
            return kOutOfMemory;
        };
        let generation = self.generations[slot]
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
            & ((1 << (31 - QUEUE_INDEX_BITS)) - 1);
        let id = (generation << QUEUE_INDEX_BITS) | slot as u32;
        queue.id = id;
        queue.background = background != 0;
        self.queues[slot].store(Arc::into_raw(Arc::new(queue)).cast_mut(), Ordering::Release);
        self.queues_opened.fetch_add(1, Ordering::Relaxed);
        *out_id = id;
        kResultTrue
    }

    pub unsafe fn close_queue(&self, id: u32) -> tresult {
        if thread::current().id() != self.control_thread || self.active.load(Ordering::Acquire) {
            return kInvalidArgument;
        }
        let (queue, _dispatch) = {
            let _lifecycle = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
            let slot = (id & ((1 << QUEUE_INDEX_BITS) - 1)) as usize;
            let Some(cell) = self.queues.get(slot) else {
                return kInvalidArgument;
            };
            let ptr = cell.load(Ordering::Acquire);
            let Some(found) = ptr.as_ref() else {
                return kInvalidArgument;
            };
            if found.id != id {
                return kInvalidArgument;
            }
            cell.store(ptr::null_mut(), Ordering::Release);
            (Arc::from_raw(ptr), self.begin_dispatch())
        };
        self.dispatch_queue(&queue, queue.background, usize::MAX, true);
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(receiver) = receiver {
            receiver.queueClosed(queue.user_context_id);
        }
        self.allocated_bytes
            .fetch_sub(queue.total_bytes, Ordering::AcqRel);
        kResultTrue
    }

    pub unsafe fn lock_block(
        &self,
        id: u32,
        out: *mut vst3::Steinberg::Vst::DataExchangeBlock,
    ) -> tresult {
        if out.is_null() {
            return kInvalidArgument;
        }
        (*out).data = ptr::null_mut();
        (*out).size = 0;
        (*out).blockID = InvalidDataExchangeBlockID;
        if !self.active.load(Ordering::Acquire) || !self.in_process.load(Ordering::Acquire) {
            return kInvalidArgument;
        }
        self.queue(id)
            .map_or(kInvalidArgument, |queue| queue.lock(out))
    }

    pub fn free_block(&self, id: u32, block_id: u32, send: bool) -> tresult {
        if !self.active.load(Ordering::Acquire) || !self.in_process.load(Ordering::Acquire) {
            return kInvalidArgument;
        }
        let Some(queue) = self.queue(id) else {
            return kInvalidArgument;
        };
        let result = queue.free(block_id, send);
        if result == kResultTrue && send {
            self.blocks_sent.fetch_add(1, Ordering::Relaxed);
        }
        if result == kResultTrue && send && queue.background {
            if let Some(worker) = self.worker_thread.get() {
                worker.unpark();
            }
        }
        result
    }

    fn dispatch_queue(
        &self,
        queue: &ExchangeQueue,
        on_background_thread: bool,
        limit: usize,
        snapshot: bool,
    ) -> usize {
        let mut delivered = 0;
        while delivered < limit {
            let Some(block_id) = queue.pending.pop() else {
                break;
            };
            let Some(block) = queue.blocks.get(block_id as usize) else {
                continue;
            };
            if snapshot {
                let bytes =
                    unsafe { std::slice::from_raw_parts(block.ptr.as_ptr(), queue.block_size) };
                self.push_snapshot(OwnedDataExchangeBlock {
                    queue_id: queue.id,
                    user_context_id: queue.user_context_id,
                    block_id,
                    data: bytes.to_vec(),
                });
            }
            let receiver = self
                .receiver
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(receiver) = receiver {
                let mut raw = vst3::Steinberg::Vst::DataExchangeBlock {
                    data: block.ptr.as_ptr().cast(),
                    size: queue.block_size as u32,
                    blockID: block_id,
                };
                unsafe {
                    receiver.onDataExchangeBlocksReceived(
                        queue.user_context_id,
                        1,
                        &mut raw,
                        u8::from(on_background_thread),
                    );
                }
                if on_background_thread {
                    self.background_blocks_delivered
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    self.foreground_blocks_delivered
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            queue.recycle(block_id);
            delivered += 1;
        }
        delivered
    }

    fn push_snapshot(&self, block: OwnedDataExchangeBlock) {
        let mut sink = self.snapshots.lock().unwrap_or_else(|p| p.into_inner());
        while sink.blocks.len() >= MAX_SNAPSHOT_BLOCKS
            || sink
                .bytes
                .checked_add(block.data.len())
                .is_none_or(|bytes| bytes > MAX_SNAPSHOT_BYTES)
        {
            let Some(oldest) = sink.blocks.pop_front() else {
                return;
            };
            sink.bytes = sink.bytes.saturating_sub(oldest.data.len());
        }
        sink.bytes += block.data.len();
        sink.blocks.push_back(block);
    }

    fn dispatch_matching(&self, background: bool, during_shutdown: bool) {
        let (queues, _dispatch) = {
            let _lifecycle = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
            if self.shutting_down.load(Ordering::Acquire) && !during_shutdown {
                return;
            }
            let queues: Vec<_> = self
                .queues
                .iter()
                .filter_map(|cell| {
                    let ptr = cell.load(Ordering::Acquire);
                    let queue = unsafe { ptr.as_ref()? };
                    (queue.background == background).then(|| Self::lease_queue(ptr))
                })
                .collect();
            (queues, self.begin_dispatch())
        };
        for queue in queues {
            self.dispatch_queue(&queue, background, usize::MAX, true);
        }
    }

    /// Deliver a bounded foreground batch without copying host-owned snapshots.
    pub fn pump_foreground(&self) {
        if !self.receiver_supported.load(Ordering::Acquire) {
            return;
        }
        let start = self.next_foreground_queue.fetch_add(1, Ordering::Relaxed) % MAX_QUEUES;
        let (queues, _dispatch) = {
            let _lifecycle = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
            if self.shutting_down.load(Ordering::Acquire) {
                return;
            }
            let queues: Vec<_> = (0..MAX_QUEUES)
                .filter_map(|offset| {
                    let cell = &self.queues[(start + offset) % MAX_QUEUES];
                    let ptr = cell.load(Ordering::Acquire);
                    let queue = unsafe { ptr.as_ref()? };
                    (!queue.background).then(|| Self::lease_queue(ptr))
                })
                .collect();
            (queues, self.begin_dispatch())
        };
        let mut remaining = MAX_FOREGROUND_BLOCKS_PER_PUMP;
        for queue in queues {
            if remaining == 0 {
                break;
            }
            remaining -= self.dispatch_queue(&queue, false, remaining, false);
        }
    }

    pub fn stats(&self) -> MainThreadServiceStats {
        MainThreadServiceStats {
            receiver_supported: self.receiver_supported.load(Ordering::Relaxed),
            queues_opened: self.queues_opened.load(Ordering::Relaxed),
            blocks_sent: self.blocks_sent.load(Ordering::Relaxed),
            foreground_blocks_delivered: self.foreground_blocks_delivered.load(Ordering::Relaxed),
            background_blocks_delivered: self.background_blocks_delivered.load(Ordering::Relaxed),
            off_thread_messages_dropped: 0,
            ..MainThreadServiceStats::default()
        }
    }

    fn has_background_pending(&self) -> bool {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
        self.queues.iter().any(|cell| {
            let ptr = cell.load(Ordering::Acquire);
            unsafe { ptr.as_ref() }
                .is_some_and(|queue| queue.background && !queue.pending.is_empty())
        })
    }

    /// Deliver all blocks before deactivation, as required by the VST3 contract.
    pub fn flush(&self) {
        self.dispatch_matching(false, true);
        while self.has_background_pending() {
            let Some(worker) = self.worker_thread.get() else {
                break;
            };
            let (sequence, wake) = &self.worker_progress;
            let sequence = sequence.lock().unwrap_or_else(|p| p.into_inner());
            worker.unpark();
            let _ = wake
                .wait_timeout(sequence, std::time::Duration::from_millis(10))
                .unwrap_or_else(|p| p.into_inner());
        }
        self.wait_for_dispatches();
    }

    pub fn take_blocks(&self) -> Vec<OwnedDataExchangeBlock> {
        self.dispatch_matching(false, false);
        let mut sink = self.snapshots.lock().unwrap_or_else(|p| p.into_inner());
        sink.bytes = 0;
        sink.blocks.drain(..).collect()
    }

    /// Close every open queue and drop the plugin-side references, undoing [`Self::configure`].
    ///
    /// Releasing the receiver here is what makes the teardown sound. The receiver is the
    /// plugin's own edit controller, held as an owning COM reference, but this state lives in
    /// the `HostApplication` — which `PluginImpl` deliberately drops *after* the module (see the
    /// `_host_app` field). Leaving the reference in place defers its `release` until after
    /// `dlclose`, dispatching through a vtable in unmapped memory. The last reference the host
    /// holds into the plugin has to go while the module is still loaded.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.flush();
        self.worker_stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker_thread.get() {
            worker.unpark();
        }
        if let Some(join) = self
            .worker_join
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = join.join();
        }
        self.wait_for_dispatches();
        let ids: Vec<u32> = {
            let _lifecycle = self.lifecycle.lock().unwrap_or_else(|p| p.into_inner());
            self.queues
                .iter()
                .filter_map(|cell| unsafe { cell.load(Ordering::Acquire).as_ref() }.map(|q| q.id))
                .collect()
        };
        for id in ids {
            unsafe {
                let _ = self.close_queue(id);
            }
        }
        self.processor.store(ptr::null_mut(), Ordering::Release);
        // Taken out of the mutex first: `release` runs plugin code, which is free to re-enter
        // the host, and must not do so while this lock is held.
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        drop(receiver);
    }
}

impl Drop for DataExchangeState {
    fn drop(&mut self) {
        self.worker_stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker_thread.get() {
            worker.unpark();
        }
        if let Some(join) = self
            .worker_join
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = join.join();
        }
        for cell in &self.queues {
            let ptr = cell.swap(ptr::null_mut(), Ordering::AcqRel);
            if !ptr.is_null() {
                unsafe { drop(Arc::from_raw(ptr)) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::marker::PhantomData;
    use std::sync::Weak;
    use vst3::{Class, ComWrapper};

    struct ClosingReceiver {
        state: Weak<DataExchangeState>,
        close_succeeded: AtomicBool,
        block_survived_close: AtomicBool,
    }

    impl Class for ClosingReceiver {
        type Interfaces = (IDataExchangeReceiver,);
    }

    impl IDataExchangeReceiverTrait for ClosingReceiver {
        unsafe fn queueOpened(&self, _: u32, _: u32, _: *mut u8) {}
        unsafe fn queueClosed(&self, _: u32) {}
        unsafe fn onDataExchangeBlocksReceived(
            &self,
            _: u32,
            _: u32,
            blocks: *mut vst3::Steinberg::Vst::DataExchangeBlock,
            _: u8,
        ) {
            if let Some(state) = self.state.upgrade() {
                let before = *((*blocks).data as *const u8);
                self.close_succeeded
                    .store(state.close_queue(32) == kResultTrue, Ordering::Release);
                let after = *((*blocks).data as *const u8);
                self.block_survived_close
                    .store(before == after, Ordering::Release);
            }
        }
    }

    #[test]
    fn exchange_queue_exhausts_and_recycles_without_allocating() {
        let queue = ExchangeQueue::new(32, 7, 64, 2, 32, false).unwrap();
        let mut first = vst3::Steinberg::Vst::DataExchangeBlock {
            data: ptr::null_mut(),
            size: 0,
            blockID: InvalidDataExchangeBlockID,
        };
        let mut second = first;
        let mut exhausted = first;
        assert_eq!(queue.lock(&mut first), kResultTrue);
        assert_eq!(queue.lock(&mut second), kResultTrue);
        assert_eq!(queue.lock(&mut exhausted), kOutOfMemory);
        assert_eq!((first.data as usize) % 32, 0);
        assert_eq!(first.size, 64);

        assert_eq!(queue.free(first.blockID, true), kResultTrue);
        assert_eq!(queue.free(first.blockID, true), kInvalidArgument);
        let pending = queue.pending.pop().unwrap();
        assert_eq!(pending, first.blockID);
        queue.recycle(pending);

        let mut recycled = exhausted;
        assert_eq!(queue.lock(&mut recycled), kResultTrue);
        assert_eq!(recycled.blockID, first.blockID);
        assert_eq!(queue.free(recycled.blockID, false), kResultTrue);
        assert_eq!(queue.free(second.blockID, false), kResultTrue);
    }

    #[test]
    fn exchange_queue_rejects_invalid_layouts() {
        assert!(ExchangeQueue::new(1, 0, 0, 1, 8, false).is_none());
        assert!(ExchangeQueue::new(1, 0, 8, 1, 3, false).is_none());
    }

    #[test]
    fn foreground_service_is_bounded_and_inert_after_shutdown() {
        let state = DataExchangeState::new();
        state.receiver_supported.store(true, Ordering::Release);
        let queue = ExchangeQueue::new(32, 7, 64, 64, 8, false).unwrap();
        for _ in 0..40 {
            let mut block = vst3::Steinberg::Vst::DataExchangeBlock {
                data: ptr::null_mut(),
                size: 0,
                blockID: InvalidDataExchangeBlockID,
            };
            assert_eq!(queue.lock(&mut block), kResultTrue);
            assert_eq!(queue.free(block.blockID, true), kResultTrue);
        }
        state.queues[0].store(Arc::into_raw(Arc::new(queue)).cast_mut(), Ordering::Release);
        let service = crate::plugin::MainThreadService {
            exchange: Arc::downgrade(&state),
            controller_sync: std::sync::Weak::new(),
            parameter_mirror: std::sync::Weak::new(),
            parameter_container_loss: std::sync::Weak::new(),
            editor_resize: std::sync::Weak::new(),
            dropped_messages: None,
            message_counters: None,
            message_deliveries: None,
            control_thread: thread::current().id(),
            _main_thread_only: PhantomData,
        };
        assert!(service.pump_foreground());
        let queue = state.queue(32).unwrap();
        assert_eq!(queue.pending.len(), 8);
        assert_eq!(queue.available.len(), 56);
        assert_eq!(state.snapshots.lock().unwrap().blocks.len(), 0);
        state.shutdown();
        assert!(service.pump_foreground());
        drop(state);
        assert!(service.pump_foreground());
        assert_eq!(service.stats().foreground_blocks_delivered, 0);
    }

    #[test]
    fn editor_resize_service_takes_latest_request_and_ignores_stale_or_off_thread_slot() {
        let resize = Arc::new(Mutex::new(Some((800, 500))));
        let service = crate::plugin::MainThreadService {
            exchange: std::sync::Weak::new(),
            controller_sync: std::sync::Weak::new(),
            parameter_mirror: std::sync::Weak::new(),
            parameter_container_loss: std::sync::Weak::new(),
            editor_resize: Arc::downgrade(&resize),
            dropped_messages: None,
            message_counters: None,
            message_deliveries: None,
            control_thread: thread::current().id(),
            _main_thread_only: PhantomData,
        };
        *resize.lock().expect("resize slot") = Some((900, 600));
        assert_eq!(service.take_editor_resize_request(), Some((900, 600)));
        assert_eq!(service.take_editor_resize_request(), None);
        drop(resize);
        assert_eq!(service.take_editor_resize_request(), None);

        let origin = thread::current().id();
        let ignored = thread::spawn(move || {
            let resize = Arc::new(Mutex::new(Some((700, 400))));
            let service = crate::plugin::MainThreadService {
                exchange: std::sync::Weak::new(),
                controller_sync: std::sync::Weak::new(),
                parameter_mirror: std::sync::Weak::new(),
                parameter_container_loss: std::sync::Weak::new(),
                editor_resize: Arc::downgrade(&resize),
                dropped_messages: None,
                message_counters: None,
                message_deliveries: None,
                control_thread: origin,
                _main_thread_only: PhantomData,
            };
            service.take_editor_resize_request()
        })
        .join()
        .expect("off-thread check");
        assert_eq!(ignored, None);
    }

    #[test]
    fn foreground_callback_can_close_its_queue() {
        let state = DataExchangeState::new();
        state.receiver_supported.store(true, Ordering::Release);
        let receiver = ComWrapper::new(ClosingReceiver {
            state: Arc::downgrade(&state),
            close_succeeded: AtomicBool::new(false),
            block_survived_close: AtomicBool::new(false),
        });
        *state.receiver.lock().unwrap() = receiver.to_com_ptr::<IDataExchangeReceiver>();
        let queue = ExchangeQueue::new(32, 7, 64, 1, 8, false).unwrap();
        let mut block = vst3::Steinberg::Vst::DataExchangeBlock {
            data: ptr::null_mut(),
            size: 0,
            blockID: InvalidDataExchangeBlockID,
        };
        assert_eq!(queue.lock(&mut block), kResultTrue);
        assert_eq!(queue.free(block.blockID, true), kResultTrue);
        state.queues[0].store(Arc::into_raw(Arc::new(queue)).cast_mut(), Ordering::Release);
        state.pump_foreground();
        assert!(receiver.close_succeeded.load(Ordering::Acquire));
        assert!(receiver.block_survived_close.load(Ordering::Acquire));
        assert!(state.queue(32).is_none());
        state.shutdown();
    }

    #[test]
    fn shutdown_drains_pending_background_blocks() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let state = DataExchangeState::new();
            let queue = ExchangeQueue::new(32, 7, 64, 1, 8, true).unwrap();
            let mut block = vst3::Steinberg::Vst::DataExchangeBlock {
                data: ptr::null_mut(),
                size: 0,
                blockID: InvalidDataExchangeBlockID,
            };
            assert_eq!(queue.lock(&mut block), kResultTrue);
            assert_eq!(queue.free(block.blockID, true), kResultTrue);
            state.queues[0].store(Arc::into_raw(Arc::new(queue)).cast_mut(), Ordering::Release);
            state.shutdown();
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown stalled with a background block pending");
        worker.join().unwrap();
    }
}

//! Internal COM interface implementations for VST3

use crate::midi::{MAX_EVENT_PAYLOAD_BYTES, MAX_EVENT_TEXT_UNITS, PluginEvent, PluginEventData};
use crate::plugin::StateContext;
use crossbeam_queue::ArrayQueue;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use vst3::com_scrape_types::{Construct, Header, Inherits, InterfaceList, Unknown, Wrapper};
use vst3::{Class, ComPtr, ComRef, ComWrapper, Interface, Steinberg::Vst::*, Steinberg::*};

// Host Application implementation.
//
// Many plugins (u-he, Waves, ...) query the context passed to `IComponent::initialize`
// for `IHostApplication` and dereference it. Passing a null context makes them crash.
// Providing a real host-application object that at least answers `getName` lets them
// initialize. `createInstance` below also vends the host-created objects they ask for
// (IMessage/IAttributeList), used to pass data between a plugin's component and controller.
struct ProgressState {
    notifications: Vec<crate::plugin::HostNotification>,
    active: HashSet<u64>,
    next_id: u64,
}

impl Default for ProgressState {
    fn default() -> Self {
        Self {
            notifications: Vec::with_capacity(MAX_HOST_NOTIFICATIONS),
            active: HashSet::with_capacity(MAX_HOST_NOTIFICATIONS),
            next_id: 1,
        }
    }
}

pub struct HostApplication {
    progress: Mutex<ProgressState>,
    data_exchange: Arc<super::data_exchange::DataExchangeState>,
}

impl Default for HostApplication {
    fn default() -> Self {
        Self {
            progress: Mutex::new(ProgressState::default()),
            data_exchange: super::data_exchange::DataExchangeState::new(),
        }
    }
}

impl HostApplication {
    pub fn take_progress_notifications(&self) -> Vec<crate::plugin::HostNotification> {
        let mut state = self
            .progress
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.notifications.drain(..).collect()
    }

    pub fn configure_data_exchange(
        &self,
        processor: *mut IAudioProcessor,
        receiver: Option<ComPtr<IDataExchangeReceiver>>,
    ) {
        self.data_exchange.configure(processor, receiver);
    }

    pub fn set_data_exchange_active(&self, active: bool) {
        self.data_exchange.set_active(active);
    }

    pub fn enter_data_exchange_process(&self) {
        self.data_exchange.enter_process();
    }

    pub fn leave_data_exchange_process(&self) {
        self.data_exchange.leave_process();
    }

    pub fn flush_data_exchange(&self) {
        self.data_exchange.flush();
    }

    pub fn take_data_exchange_blocks(&self) -> Vec<crate::plugin::DataExchangeBlock> {
        self.data_exchange.take_blocks()
    }

    pub fn data_exchange_state(&self) -> Arc<super::data_exchange::DataExchangeState> {
        Arc::clone(&self.data_exchange)
    }

    pub fn shutdown_data_exchange(&self) {
        self.data_exchange.shutdown();
    }
}

impl Class for HostApplication {
    // The standard SDK host context implements both IHostApplication and
    // IPlugInterfaceSupport; plugins query the context for either.
    type Interfaces = (
        IHostApplication,
        IPlugInterfaceSupport,
        IProgress,
        IDataExchangeHandler,
    );
}

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, iid: *const TUID) -> tresult {
        if iid.is_null() {
            return kInvalidArgument;
        }
        // This interface describes plug-in-side interfaces the host knows how to consume.
        // Host callbacks such as IComponentHandler are deliberately absent: advertising
        // those reverses the direction of the contract and makes plug-ins enable features
        // whose corresponding plug-in interface the host may never query.
        let bytes = std::slice::from_raw_parts(iid as *const u8, 16);
        let supported = [
            &IConnectionPoint::IID,
            &IMidiMapping::IID,
            &IUnitInfo::IID,
            &IProgramListData::IID,
            &IUnitData::IID,
            &IEditControllerHostEditing::IID,
            &IMidiLearn::IID,
            &IAutomationState::IID,
            &INoteExpressionController::IID,
            &IPlugViewContentScaleSupport::IID,
            &IProcessContextRequirements::IID,
            &IPrefetchableSupport::IID,
            &IRemapParamID::IID,
            &IDataExchangeReceiver::IID,
        ];
        if supported.iter().any(|supported| bytes == &supported[..]) {
            kResultTrue
        } else {
            kResultFalse
        }
    }
}

impl IDataExchangeHandlerTrait for HostApplication {
    unsafe fn openQueue(
        &self,
        processor: *mut IAudioProcessor,
        block_size: u32,
        num_blocks: u32,
        alignment: u32,
        user_context_id: u32,
        out_id: *mut u32,
    ) -> tresult {
        self.data_exchange.open_queue(
            processor,
            block_size,
            num_blocks,
            alignment,
            user_context_id,
            out_id,
        )
    }

    unsafe fn closeQueue(&self, queue_id: u32) -> tresult {
        self.data_exchange.close_queue(queue_id)
    }

    unsafe fn lockBlock(
        &self,
        queue_id: u32,
        block: *mut vst3::Steinberg::Vst::DataExchangeBlock,
    ) -> tresult {
        self.data_exchange.lock_block(queue_id, block)
    }

    unsafe fn freeBlock(&self, queue_id: u32, block_id: u32, send_to_controller: TBool) -> tresult {
        self.data_exchange
            .free_block(queue_id, block_id, send_to_controller != 0)
    }
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kResultFalse;
        }
        let dst = &mut *name;
        let mut i = 0;
        for ch in "vst3-host".encode_utf16() {
            if i + 1 >= dst.len() {
                break;
            }
            dst[i] = ch;
            i += 1;
        }
        dst[i] = 0;
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut std::ffi::c_void,
    ) -> tresult {
        // Vend the host-created objects plugins ask for (the SDK's HostApplication does
        // this): IMessage and IAttributeList, used to pass data between a plugin's
        // component and controller halves. Anything else fails cleanly.
        if obj.is_null() || cid.is_null() || iid.is_null() {
            return kInvalidArgument;
        }
        *obj = ptr::null_mut();

        // IMessage and IAttributeList use their interface UID as their host-created class UID.
        // Honour both inputs: returning an IMessage pointer for an unrelated requested IID is
        // a COM type confusion bug even when the class id itself is valid.
        let cid_bytes = std::slice::from_raw_parts(cid as *const u8, 16);
        let iid_bytes = std::slice::from_raw_parts(iid as *const u8, 16);
        let matches =
            |expected: &[u8; 16]| cid_bytes == &expected[..] && iid_bytes == &expected[..];

        if matches(&IMessage::IID) {
            if let Some(p) = create_host_message().to_com_ptr::<IMessage>() {
                *obj = p.into_raw() as *mut std::ffi::c_void;
                return kResultTrue;
            }
        } else if matches(&IAttributeList::IID) {
            if let Some(p) = create_host_attribute_list().to_com_ptr::<IAttributeList>() {
                *obj = p.into_raw() as *mut std::ffi::c_void;
                return kResultTrue;
            }
        }
        kNoInterface
    }
}

impl IProgressTrait for HostApplication {
    unsafe fn start(
        &self,
        r#type: IProgress_::ProgressType,
        optional_description: *const tchar,
        out_id: *mut IProgress_::ID,
    ) -> tresult {
        if out_id.is_null() {
            return kInvalidArgument;
        }
        let description = if optional_description.is_null() {
            None
        } else {
            const MAX_PROGRESS_DESCRIPTION_UNITS: usize = 1024;
            let mut units = Vec::with_capacity(64);
            for index in 0..MAX_PROGRESS_DESCRIPTION_UNITS {
                let unit = *optional_description.add(index);
                if unit == 0 {
                    break;
                }
                units.push(unit);
            }
            if units.len() == MAX_PROGRESS_DESCRIPTION_UNITS {
                return kInvalidArgument;
            }
            Some(String::from_utf16_lossy(&units))
        };
        let kind = match r#type {
            IProgress_::ProgressType_::AsyncStateRestoration => {
                crate::plugin::ProgressKind::AsyncStateRestoration
            }
            IProgress_::ProgressType_::UIBackgroundTask => {
                crate::plugin::ProgressKind::UiBackgroundTask
            }
            other => crate::plugin::ProgressKind::Other(other),
        };

        let mut state = self
            .progress
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.notifications.len() >= MAX_HOST_NOTIFICATIONS
            || state.active.len() >= MAX_HOST_NOTIFICATIONS
        {
            return kResultFalse;
        }
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).unwrap_or(1);
        state.active.insert(id);
        state
            .notifications
            .push(crate::plugin::HostNotification::ProgressStarted {
                id,
                kind,
                description,
            });
        *out_id = id;
        kResultOk
    }

    unsafe fn update(&self, id: IProgress_::ID, norm_value: ParamValue) -> tresult {
        let Some(value) = crate::plugin::ProgressValue::new(norm_value) else {
            return kInvalidArgument;
        };
        let mut state = self
            .progress
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.active.contains(&id) || state.notifications.len() >= MAX_HOST_NOTIFICATIONS {
            return kResultFalse;
        }
        state
            .notifications
            .push(crate::plugin::HostNotification::ProgressUpdated { id, value });
        kResultOk
    }

    unsafe fn finish(&self, id: IProgress_::ID) -> tresult {
        let mut state = self
            .progress
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.active.contains(&id) || state.notifications.len() >= MAX_HOST_NOTIFICATIONS {
            return kResultFalse;
        }
        state.active.remove(&id);
        state
            .notifications
            .push(crate::plugin::HostNotification::ProgressFinished { id });
        kResultOk
    }
}

/// Create a host-application context to pass to `IComponent::initialize`.
pub fn create_host_application() -> ComWrapper<HostApplication> {
    ComWrapper::new(HostApplication::default())
}

/// Log the first off-thread drop, then every `DROP_LOG_INTERVAL`-th one. A plugin that
/// notifies from its processor thread does so per block, so logging unconditionally would
/// emit thousands of lines a second.
const DROP_LOG_INTERVAL: u64 = 256;
const MESSAGE_PACKET_COUNT: usize = 4;
const MESSAGE_PACKET_BYTES: usize = 256 * 1024;
const MESSAGE_MAX_ATTRIBUTES: usize = 16;
const MESSAGE_MAX_ID_BYTES: usize = 128;
const MESSAGE_MAX_KEY_BYTES: usize = 128;
const MESSAGE_PUMP_LIMIT: usize = 16;

#[derive(Clone, Copy)]
enum PacketKind {
    Int,
    Float,
    String,
    Binary,
}

#[derive(Clone, Copy)]
struct PacketAttribute {
    kind: PacketKind,
    key_offset: usize,
    key_len: usize,
    value_offset: usize,
    value_len: usize,
    scalar: u64,
}

struct MessagePacket {
    id: [u8; MESSAGE_MAX_ID_BYTES],
    id_len: usize,
    attributes: Vec<PacketAttribute>,
    bytes: Box<[u8]>,
    used: usize,
}

impl MessagePacket {
    fn new() -> Self {
        Self {
            id: [0; MESSAGE_MAX_ID_BYTES],
            id_len: 0,
            attributes: Vec::with_capacity(MESSAGE_MAX_ATTRIBUTES),
            bytes: vec![0; MESSAGE_PACKET_BYTES].into_boxed_slice(),
            used: 0,
        }
    }

    fn clear(&mut self) {
        self.id_len = 0;
        self.attributes.clear();
        self.used = 0;
    }

    fn copy_bytes(&mut self, bytes: &[u8]) -> Option<(usize, usize)> {
        let end = self.used.checked_add(bytes.len())?;
        if end > self.bytes.len() {
            return None;
        }
        let start = self.used;
        self.bytes[start..end].copy_from_slice(bytes);
        self.used = end;
        Some((start, bytes.len()))
    }

    fn to_message(&self) -> Option<ComWrapper<HostMessage>> {
        let message = create_host_message();
        let id = std::ffi::CString::new(&self.id[..self.id_len]).ok()?;
        *message.id.lock().ok()? = Some(id);
        let mut map = message.attributes.attrs.lock().ok()?;
        for attr in &self.attributes {
            let key = String::from_utf8(
                self.bytes[attr.key_offset..attr.key_offset + attr.key_len].to_vec(),
            )
            .ok()?;
            let value = match attr.kind {
                PacketKind::Int => AttrValue::Int(attr.scalar as i64),
                PacketKind::Float => AttrValue::Float(f64::from_bits(attr.scalar)),
                PacketKind::Binary => AttrValue::Bin(
                    self.bytes[attr.value_offset..attr.value_offset + attr.value_len].to_vec(),
                ),
                PacketKind::String => {
                    let mut units = Vec::with_capacity(attr.value_len / 2);
                    for pair in self.bytes[attr.value_offset..attr.value_offset + attr.value_len]
                        .chunks_exact(2)
                    {
                        units.push(u16::from_ne_bytes([pair[0], pair[1]]));
                    }
                    AttrValue::Str(units)
                }
            };
            map.insert(key, value);
        }
        drop(map);
        Some(message)
    }
}

#[derive(Default)]
pub struct ConnectionMessageCounters {
    pub queued: AtomicU64,
    pub delivered: AtomicU64,
    pub unsupported: AtomicU64,
    pub contended: AtomicU64,
    pub oversized: AtomicU64,
    pub full: AtomicU64,
    pub rejected_by_destination: AtomicU64,
    pub reconstruction_failed: AtomicU64,
    pub discarded_on_close: AtomicU64,
}

pub struct ConnectionDelivery {
    destination: Mutex<Option<ComPtr<IConnectionPoint>>>,
    free: ArrayQueue<MessagePacket>,
    pending: ArrayQueue<MessagePacket>,
    closed: AtomicBool,
    dropped: Arc<AtomicU64>,
    counters: Arc<ConnectionMessageCounters>,
}

impl ConnectionDelivery {
    fn new(
        destination: ComPtr<IConnectionPoint>,
        dropped: Arc<AtomicU64>,
        counters: Arc<ConnectionMessageCounters>,
    ) -> Arc<Self> {
        let free = ArrayQueue::new(MESSAGE_PACKET_COUNT);
        for _ in 0..MESSAGE_PACKET_COUNT {
            free.push(MessagePacket::new()).ok();
        }
        Arc::new(Self {
            destination: Mutex::new(Some(destination)),
            free,
            pending: ArrayQueue::new(MESSAGE_PACKET_COUNT),
            closed: AtomicBool::new(false),
            dropped,
            counters,
        })
    }

    fn reject(&self, counter: &AtomicU64) -> tresult {
        counter.fetch_add(1, Ordering::Relaxed);
        self.dropped.fetch_add(1, Ordering::Relaxed);
        kResultFalse
    }

    unsafe fn enqueue(&self, message: *mut IMessage) -> tresult {
        if self.closed.load(Ordering::Acquire) {
            return self.reject(&self.counters.unsupported);
        }
        let Some(mut packet) = self.free.pop() else {
            return self.reject(&self.counters.full);
        };
        packet.clear();
        let Some(message) = ComRef::<IMessage>::from_raw(message) else {
            self.free.push(packet).ok();
            return self.reject(&self.counters.unsupported);
        };
        let Some(snapshot) = message.cast::<IHostMessageSnapshot>() else {
            self.free.push(packet).ok();
            return self.reject(&self.counters.unsupported);
        };
        let result = ((*(*snapshot.as_ptr()).vtbl).snapshot)(snapshot.as_ptr(), &mut packet);
        if result != kResultOk {
            self.free.push(packet).ok();
            return if result == kOutOfMemory {
                self.reject(&self.counters.oversized)
            } else if result == kResultFalse {
                self.reject(&self.counters.contended)
            } else {
                self.reject(&self.counters.unsupported)
            };
        }
        if self.closed.load(Ordering::Acquire) {
            self.free.push(packet).ok();
            return self.reject(&self.counters.unsupported);
        }
        match self.pending.push(packet) {
            Ok(()) => {
                self.counters.queued.fetch_add(1, Ordering::Relaxed);
                if self.closed.load(Ordering::Acquire) {
                    self.discard_pending();
                    self.reject(&self.counters.unsupported)
                } else {
                    kResultOk
                }
            }
            Err(packet) => {
                self.free.push(packet).ok();
                self.reject(&self.counters.full)
            }
        }
    }

    pub fn pump(&self) {
        for _ in 0..MESSAGE_PUMP_LIMIT {
            let Some(mut packet) = self.pending.pop() else {
                break;
            };
            let destination = self
                .destination
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if !self.closed.load(Ordering::Acquire) {
                match (destination, packet.to_message()) {
                    (Some(destination), Some(message)) => {
                        if let Some(message) = message.to_com_ptr::<IMessage>() {
                            let result = unsafe { destination.notify(message.as_ptr()) };
                            if result == kResultOk || result == kResultTrue {
                                self.counters.delivered.fetch_add(1, Ordering::Relaxed);
                            } else {
                                self.counters
                                    .rejected_by_destination
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            self.counters
                                .reconstruction_failed
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => {
                        self.counters
                            .reconstruction_failed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            packet.clear();
            self.free.push(packet).ok();
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.destination
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.discard_pending();
    }

    fn discard_pending(&self) {
        for _ in 0..MESSAGE_PACKET_COUNT {
            let Some(packet) = self.pending.pop() else {
                break;
            };
            self.counters
                .discarded_on_close
                .fetch_add(1, Ordering::Relaxed);
            // One popped packet leaves exactly one free slot in the fixed pool.
            self.free.push(packet).ok();
        }
    }
}

/// Host-side connection point which delivers processor-thread messages on the main thread.
///
/// # Which thread is the "UI thread"
///
/// The gate is `ConnectionPair::connect`'s calling thread — in practice the thread that
/// loaded the plugin, because the pair is built during load. That matches this library's
/// documented threading model: `load_plugin`, `open_editor` and every controller call belong
/// on the host's GUI thread (see `docs/explanation/threading.md`). Load on a worker thread
/// and the gate follows that worker, not your GUI thread.
///
/// An off-thread `notify` snapshots host-created message contents into a bounded packet queue.
/// The main-thread service reconstructs a fresh message and invokes the destination. Unsupported
/// messages, contention, oversized payloads, and a full queue return `kResultFalse`; those
/// failures are counted. The snapshot path does bounded copying and nonwaiting lock attempts.
pub struct ConnectionProxy {
    delivery: Arc<ConnectionDelivery>,
    control_thread: ThreadId,
    /// Off-thread messages refused by the bounded bridge.
    dropped: Arc<AtomicU64>,
    /// Which direction this proxy carries, for the drop log ("component→controller").
    direction: &'static str,
}

impl ConnectionProxy {
    fn new(
        delivery: Arc<ConnectionDelivery>,
        control_thread: ThreadId,
        direction: &'static str,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            delivery,
            control_thread,
            dropped,
            direction,
        }
    }

    fn clear(&self) {
        self.delivery.close();
    }

    /// Count an off-thread `notify` and log the first one plus every
    /// [`DROP_LOG_INTERVAL`]-th one after it.
    fn record_off_thread_drop(&self) {
        let count = self.dropped.load(Ordering::Relaxed);
        if count == 1 || count % DROP_LOG_INTERVAL == 0 {
            log::warn!(
                "ConnectionProxy ({}): refused an off-thread IConnectionPoint::notify \
                 ({count} so far). The bounded main-thread message bridge could not accept it.",
                self.direction
            );
        }
    }

    #[cfg(test)]
    fn dropped_message_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Class for ConnectionProxy {
    type Interfaces = (IConnectionPoint,);
}

impl IConnectionPointTrait for ConnectionProxy {
    unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
        // The endpoints are fixed by `ConnectionPair`; accepting an arbitrary replacement
        // would let a plugin bypass the thread gate.
        kResultFalse
    }

    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultFalse
    }

    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        if message.is_null() {
            return kResultFalse;
        }
        if thread::current().id() != self.control_thread {
            let result = self.delivery.enqueue(message);
            if result != kResultOk {
                self.record_off_thread_drop();
            }
            return result;
        }

        // Clone under the lock, then release it before calling plugin code. `notify` is allowed
        // to re-enter the host (including disconnect/teardown).
        let destination = self
            .delivery
            .destination
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        match destination {
            Some(destination) => destination.notify(message),
            None => kResultFalse,
        }
    }
}

/// The two proxy connections between a separate component and edit controller.
///
/// Both directions deliver on the thread that called [`Self::connect`] — see
/// [`ConnectionProxy`] for snapshot and rejection behavior.
pub struct ConnectionPair {
    component: ComPtr<IConnectionPoint>,
    controller: ComPtr<IConnectionPoint>,
    component_to_controller: ComWrapper<ConnectionProxy>,
    controller_to_component: ComWrapper<ConnectionProxy>,
    component_connected: AtomicBool,
    controller_connected: AtomicBool,
    dropped: Arc<AtomicU64>,
    counters: Arc<ConnectionMessageCounters>,
}

impl ConnectionPair {
    /// Connect both directions, rolling the first direction back if the second fails.
    pub unsafe fn connect(
        component: ComPtr<IConnectionPoint>,
        controller: ComPtr<IConnectionPoint>,
    ) -> Option<Self> {
        let control_thread = thread::current().id();
        let dropped = Arc::new(AtomicU64::new(0));
        let counters = Arc::new(ConnectionMessageCounters::default());
        let component_delivery = ConnectionDelivery::new(
            controller.clone(),
            Arc::clone(&dropped),
            Arc::clone(&counters),
        );
        let controller_delivery = ConnectionDelivery::new(
            component.clone(),
            Arc::clone(&dropped),
            Arc::clone(&counters),
        );
        let component_to_controller = ComWrapper::new(ConnectionProxy::new(
            Arc::clone(&component_delivery),
            control_thread,
            "component→controller",
            Arc::clone(&dropped),
        ));
        let controller_to_component = ComWrapper::new(ConnectionProxy::new(
            Arc::clone(&controller_delivery),
            control_thread,
            "controller→component",
            Arc::clone(&dropped),
        ));
        let component_proxy = component_to_controller.to_com_ptr::<IConnectionPoint>()?;
        let controller_proxy = controller_to_component.to_com_ptr::<IConnectionPoint>()?;

        let component_result = component.connect(component_proxy.as_ptr());
        if component_result != kResultOk && component_result != kResultTrue {
            log::warn!("component refused host connection proxy: {component_result:#x}");
            return None;
        }

        let controller_result = controller.connect(controller_proxy.as_ptr());
        if controller_result != kResultOk && controller_result != kResultTrue {
            component.disconnect(component_proxy.as_ptr());
            log::warn!(
                "controller refused host connection proxy; rolled component connection back: \
                 {controller_result:#x}"
            );
            return None;
        }

        Some(Self {
            component,
            controller,
            component_to_controller,
            controller_to_component,
            component_connected: AtomicBool::new(true),
            controller_connected: AtomicBool::new(true),
            dropped,
            counters,
        })
    }

    /// Total off-thread `notify` calls refused by the bounded snapshot bridge.
    pub fn dropped_message_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn dropped_message_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }

    pub fn message_counters(&self) -> Arc<ConnectionMessageCounters> {
        Arc::clone(&self.counters)
    }

    pub fn message_deliveries(&self) -> [std::sync::Weak<ConnectionDelivery>; 2] {
        [
            Arc::downgrade(&self.component_to_controller.delivery),
            Arc::downgrade(&self.controller_to_component.delivery),
        ]
    }

    /// Disconnect both plugin endpoints. Safe to call more than once.
    pub unsafe fn disconnect(&self) {
        if self.component_connected.swap(false, Ordering::AcqRel) {
            if let Some(proxy) = self
                .component_to_controller
                .as_com_ref::<IConnectionPoint>()
            {
                self.component.disconnect(proxy.as_ptr());
            }
        }
        if self.controller_connected.swap(false, Ordering::AcqRel) {
            if let Some(proxy) = self
                .controller_to_component
                .as_com_ref::<IConnectionPoint>()
            {
                self.controller.disconnect(proxy.as_ptr());
            }
        }
        self.component_to_controller.clear();
        self.controller_to_component.clear();
    }
}

impl Drop for ConnectionPair {
    fn drop(&mut self) {
        // A closing summary, so the drops are visible even to a host that never polls the
        // counter. The per-drop log is rate-limited and easy to miss in a long session.
        let dropped = self.dropped_message_count();
        if dropped > 0 {
            log::warn!(
                "ConnectionPair: {dropped} off-thread component↔controller message(s) were \
                 refused by the bounded bridge over this plugin's lifetime"
            );
        }
        unsafe {
            self.disconnect();
        }
    }
}

// A host-side IAttributeList: a typed key/value bag plugins use (via the host's
// createInstance) to pass data between their component and controller halves.
#[derive(Debug, Clone, PartialEq)]
enum AttrValue {
    Int(i64),
    Float(f64),
    /// UTF-16 (TChar) string, not null-terminated.
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

/// Host implementation of `IAttributeList`.
#[derive(Default)]
pub struct HostAttributeList {
    attrs: Mutex<HashMap<String, AttrValue>>,
}

impl HostAttributeList {
    pub fn new() -> Self {
        Self::default()
    }

    // Safe inner API (also the unit-test surface).
    fn put(&self, key: String, value: AttrValue) {
        if let Ok(mut m) = self.attrs.lock() {
            m.insert(key, value);
        }
    }
    fn get_value(&self, key: &str) -> Option<AttrValue> {
        self.attrs.lock().ok().and_then(|m| m.get(key).cloned())
    }
}

/// Decode an `AttrID` (a C string) into an owned key.
unsafe fn attr_key(id: *const std::os::raw::c_char) -> String {
    if id.is_null() {
        return String::new();
    }
    CStr::from_ptr(id).to_string_lossy().into_owned()
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: *const std::os::raw::c_char, value: i64) -> tresult {
        self.put(attr_key(id), AttrValue::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: *const std::os::raw::c_char, value: *mut i64) -> tresult {
        match self.get_value(&attr_key(id)) {
            Some(AttrValue::Int(v)) if !value.is_null() => {
                *value = v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: *const std::os::raw::c_char, value: f64) -> tresult {
        self.put(attr_key(id), AttrValue::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: *const std::os::raw::c_char, value: *mut f64) -> tresult {
        match self.get_value(&attr_key(id)) {
            Some(AttrValue::Float(v)) if !value.is_null() => {
                *value = v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: *const std::os::raw::c_char, string: *const u16) -> tresult {
        if string.is_null() {
            return kResultFalse;
        }
        let mut buf = Vec::new();
        let mut p = string;
        while *p != 0 {
            buf.push(*p);
            p = p.add(1);
        }
        self.put(attr_key(id), AttrValue::Str(buf));
        kResultOk
    }
    unsafe fn getString(
        &self,
        id: *const std::os::raw::c_char,
        string: *mut u16,
        size_in_bytes: u32,
    ) -> tresult {
        match self.get_value(&attr_key(id)) {
            Some(AttrValue::Str(v)) if !string.is_null() => {
                // Copy up to capacity-1 chars, then null-terminate.
                let cap_chars = (size_in_bytes as usize / 2).saturating_sub(1);
                let n = v.len().min(cap_chars);
                for (i, &ch) in v.iter().take(n).enumerate() {
                    *string.add(i) = ch;
                }
                *string.add(n) = 0;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setBinary(
        &self,
        id: *const std::os::raw::c_char,
        data: *const std::ffi::c_void,
        size_in_bytes: u32,
    ) -> tresult {
        if data.is_null() {
            return kResultFalse;
        }
        let bytes = std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize).to_vec();
        self.put(attr_key(id), AttrValue::Bin(bytes));
        kResultOk
    }
    unsafe fn getBinary(
        &self,
        id: *const std::os::raw::c_char,
        data: *mut *const std::ffi::c_void,
        size_in_bytes: *mut u32,
    ) -> tresult {
        // Note: returns a pointer into the stored buffer; valid until the entry is
        // replaced. VST3 plugins read it synchronously during init, which is safe here.
        if data.is_null() || size_in_bytes.is_null() {
            return kResultFalse;
        }
        if let Ok(m) = self.attrs.lock() {
            if let Some(AttrValue::Bin(v)) = m.get(&attr_key(id)) {
                *data = v.as_ptr() as *const std::ffi::c_void;
                *size_in_bytes = v.len() as u32;
                return kResultOk;
            }
        }
        kResultFalse
    }
}

/// Create a host attribute list.
pub fn create_host_attribute_list() -> ComWrapper<HostAttributeList> {
    ComWrapper::new(HostAttributeList::new())
}

/// Host implementation of `IMessage` (an id + an attribute list), used for
/// component<->controller communication that plugins allocate via the host.
#[repr(C)]
pub struct IHostMessageSnapshot {
    vtbl: *const IHostMessageSnapshotVtbl,
}

#[repr(C)]
pub struct IHostMessageSnapshotVtbl {
    base: FUnknownVtbl,
    snapshot: unsafe extern "system" fn(*mut IHostMessageSnapshot, *mut MessagePacket) -> tresult,
}

impl Unknown for IHostMessageSnapshot {
    unsafe fn query_interface(
        this: *mut Self,
        iid: &vst3::com_scrape_types::Guid,
    ) -> Option<*mut std::ffi::c_void> {
        <FUnknown as Unknown>::query_interface(this.cast(), iid)
    }
    unsafe fn add_ref(this: *mut Self) -> usize {
        <FUnknown as Unknown>::add_ref(this.cast())
    }
    unsafe fn release(this: *mut Self) -> usize {
        <FUnknown as Unknown>::release(this.cast())
    }
}

unsafe impl Interface for IHostMessageSnapshot {
    type Vtbl = IHostMessageSnapshotVtbl;
    const IID: vst3::com_scrape_types::Guid = *b"SPMsgSnapshotV01";
    fn inherits(iid: &vst3::com_scrape_types::Guid) -> bool {
        iid == &Self::IID || FUnknown::inherits(iid)
    }
}

unsafe impl Inherits<FUnknown> for IHostMessageSnapshot {}

impl IHostMessageSnapshot {
    const fn make_vtbl<C, W, const OFFSET: isize>() -> IHostMessageSnapshotVtbl
    where
        C: HostMessageSnapshotTrait + Class,
        W: Wrapper<C>,
    {
        unsafe extern "system" fn query<C, W, const OFFSET: isize>(
            this: *mut FUnknown,
            iid: *const TUID,
            out: *mut *mut std::ffi::c_void,
        ) -> tresult
        where
            C: HostMessageSnapshotTrait + Class,
            W: Wrapper<C>,
        {
            if iid.is_null() || out.is_null() {
                return kInvalidArgument;
            }
            let header = (this as *mut u8).offset(-OFFSET).cast::<Header<C>>();
            if let Some(offset) = C::Interfaces::query(&*(iid as *const [u8; 16])) {
                W::add_ref(W::data_from_header(header));
                *out = (header as *mut u8).offset(offset).cast();
                kResultOk
            } else {
                *out = ptr::null_mut();
                kNoInterface
            }
        }
        unsafe extern "system" fn add_ref<C, W, const OFFSET: isize>(this: *mut FUnknown) -> u32
        where
            C: HostMessageSnapshotTrait + Class,
            W: Wrapper<C>,
        {
            let header = (this as *mut u8).offset(-OFFSET).cast::<Header<C>>();
            W::add_ref(W::data_from_header(header)) as u32
        }
        unsafe extern "system" fn release<C, W, const OFFSET: isize>(this: *mut FUnknown) -> u32
        where
            C: HostMessageSnapshotTrait + Class,
            W: Wrapper<C>,
        {
            let header = (this as *mut u8).offset(-OFFSET).cast::<Header<C>>();
            W::release(W::data_from_header(header)) as u32
        }
        unsafe extern "system" fn snapshot<C, W, const OFFSET: isize>(
            this: *mut IHostMessageSnapshot,
            out: *mut MessagePacket,
        ) -> tresult
        where
            C: HostMessageSnapshotTrait + Class,
            W: Wrapper<C>,
        {
            if out.is_null() {
                return kInvalidArgument;
            }
            let header = (this as *mut u8).offset(-OFFSET).cast::<Header<C>>();
            (*W::data_from_header(header)).snapshot_into(&mut *out)
        }
        IHostMessageSnapshotVtbl {
            base: FUnknownVtbl {
                queryInterface: query::<C, W, OFFSET>,
                addRef: add_ref::<C, W, OFFSET>,
                release: release::<C, W, OFFSET>,
            },
            snapshot: snapshot::<C, W, OFFSET>,
        }
    }
}

unsafe impl<C, W, const OFFSET: isize> Construct<C, W, OFFSET> for IHostMessageSnapshot
where
    C: HostMessageSnapshotTrait + Class,
    W: Wrapper<C>,
{
    const OBJ: Self = Self {
        vtbl: &Self::make_vtbl::<C, W, OFFSET>(),
    };
}

trait HostMessageSnapshotTrait {
    fn snapshot_into(&self, packet: &mut MessagePacket) -> tresult;
}

pub struct HostMessage {
    id: Mutex<Option<std::ffi::CString>>,
    attributes: ComWrapper<HostAttributeList>,
}

impl Default for HostMessage {
    fn default() -> Self {
        Self {
            id: Mutex::new(None),
            attributes: create_host_attribute_list(),
        }
    }
}

impl HostMessage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl HostMessageSnapshotTrait for HostMessage {
    fn snapshot_into(&self, packet: &mut MessagePacket) -> tresult {
        let Ok(id) = self.id.try_lock() else {
            return kResultFalse;
        };
        let Ok(attrs) = self.attributes.attrs.try_lock() else {
            return kResultFalse;
        };
        let Some(id) = id.as_ref() else {
            return kInvalidArgument;
        };
        let id_bytes = id.as_bytes();
        if id_bytes.len() > MESSAGE_MAX_ID_BYTES || attrs.len() > MESSAGE_MAX_ATTRIBUTES {
            return kOutOfMemory;
        }
        packet.clear();
        packet.id[..id_bytes.len()].copy_from_slice(id_bytes);
        packet.id_len = id_bytes.len();
        for (key, value) in attrs.iter() {
            if key.len() > MESSAGE_MAX_KEY_BYTES {
                return kOutOfMemory;
            }
            let Some((key_offset, key_len)) = packet.copy_bytes(key.as_bytes()) else {
                return kOutOfMemory;
            };
            let (kind, value_offset, value_len, scalar) = match value {
                AttrValue::Int(v) => (PacketKind::Int, 0, 0, *v as u64),
                AttrValue::Float(v) => (PacketKind::Float, 0, 0, v.to_bits()),
                AttrValue::Bin(bytes) => {
                    let Some((offset, len)) = packet.copy_bytes(bytes) else {
                        return kOutOfMemory;
                    };
                    (PacketKind::Binary, offset, len, 0)
                }
                AttrValue::Str(units) => {
                    let Some(byte_len) = units.len().checked_mul(2) else {
                        return kOutOfMemory;
                    };
                    let Some(end) = packet.used.checked_add(byte_len) else {
                        return kOutOfMemory;
                    };
                    if end > packet.bytes.len() {
                        return kOutOfMemory;
                    }
                    let offset = packet.used;
                    for (index, unit) in units.iter().enumerate() {
                        let start = offset + index * 2;
                        packet.bytes[start..start + 2].copy_from_slice(&unit.to_ne_bytes());
                    }
                    packet.used = end;
                    (PacketKind::String, offset, byte_len, 0)
                }
            };
            packet.attributes.push(PacketAttribute {
                kind,
                key_offset,
                key_len,
                value_offset,
                value_len,
                scalar,
            });
        }
        kResultOk
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage, IHostMessageSnapshot);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        // Pointer to the stored id (valid until replaced); null if unset.
        if let Ok(g) = self.id.lock() {
            if let Some(ref s) = *g {
                return s.as_ptr();
            }
        }
        ptr::null()
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        if id.is_null() {
            return;
        }
        let owned = CStr::from_ptr(id).to_owned();
        if let Ok(mut g) = self.id.lock() {
            *g = Some(owned);
        }
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed pointer to the message's own attribute list (kept alive by `self`).
        self.attributes
            .to_com_ptr::<IAttributeList>()
            .map(|p| p.as_ptr())
            .unwrap_or(ptr::null_mut())
    }
}

/// Create a host message.
pub fn create_host_message() -> ComWrapper<HostMessage> {
    ComWrapper::new(HostMessage::new())
}

// Host-side in-memory `IBStream`. Plugins serialize their state into a stream the host
// provides (`IComponent::getState`) and restore from one the host fills
// (`IComponent::setState`). This backs both with a growable byte buffer plus a cursor.
struct MemBuf {
    data: Vec<u8>,
    pos: usize,
}

/// Cap on how large a plugin can grow a host-provided state stream (and how far it may seek
/// into one). The cursor and the write length both come from the plugin, and the buffer grows
/// to `cursor + length`: without a bound, a wild seek turns the following write into a
/// multi-gigabyte `Vec::resize` — a capacity-overflow panic inside an `extern "system"` vtable
/// thunk, which aborts the process rather than unwinding. 64 MiB is far above any real plugin
/// state (the largest sample-library presets are a few MiB). Mirrors the `MAX_*` caps on every
/// other host-side buffer; over-cap operations fail with a result code instead of allocating.
pub const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_STREAM_FILENAME_UNITS: usize = 127;

/// The purpose of a host-provided stream, published under the `StateType` key of
/// `IStreamAttributes::getAttributes` so a plugin can tell a project load from a preset load.
///
/// Every variant maps to a string the SDK defines in `Steinberg::Vst::StateType`
/// (`vstpresetkeys.h`) — the `state_type_values_match_the_sdk_constants` test pins each one
/// against `vst3`'s generated constants so a typo cannot ship. `kDefault` is deliberately
/// absent: it means "restored from a preset *marked as default*, or the host wants to store a
/// default state of the plug-in", which is not something this host ever asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStateType {
    /// `StateType::kProject` — state saved with, or restored from, a host project.
    Project,
    /// `StateType::kTrackPreset` — state saved to, or restored from, a standalone preset
    /// (a `.vstpreset` file).
    TrackPreset,
}

impl StreamStateType {
    fn attribute_value(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::TrackPreset => "TrackPreset",
        }
    }
}

impl From<&StateContext> for StreamStateType {
    fn from(context: &StateContext) -> Self {
        match context {
            StateContext::Project => Self::Project,
            StateContext::Preset { .. } => Self::TrackPreset,
        }
    }
}

/// What the host publishes about a stream it hands a plugin: the `IStreamAttributes` entries
/// and the name `IStreamAttributes::getFileName` reports.
///
/// A struct rather than a row of `Option<&str>` parameters — the two string fields mean very
/// different things (a bare file name versus a full path) and would otherwise be trivially
/// swappable at a call site.
#[derive(Debug, Clone, Copy, Default)]
struct StreamMetadata<'a> {
    /// `PresetAttributes::kStateType`. `None` leaves the attribute off entirely, which the
    /// SDK's `Helpers::isProjectState` reports as "the host does not implement this".
    state_type: Option<StreamStateType>,
    /// What `IStreamAttributes::getFileName` returns — "filename (without file extension)".
    file_name: Option<&'a str>,
    /// `PresetAttributes::kFilePathStringType` — "full file path string (if available) where
    /// the preset comes from".
    file_path: Option<&'a str>,
}

impl<'a> StreamMetadata<'a> {
    /// Metadata that says only what kind of state the stream carries.
    fn new(state_type: StreamStateType) -> Self {
        Self {
            state_type: Some(state_type),
            file_name: None,
            file_path: None,
        }
    }

    /// The metadata a `setState` stream should carry for `context`, including the source
    /// file's path and stem when the context names one.
    fn for_state_context(context: &'a StateContext) -> Self {
        let path = context.file_path();
        Self {
            state_type: Some(StreamStateType::from(context)),
            file_name: path.and_then(|p| p.file_stem()).and_then(|s| s.to_str()),
            file_path: path.and_then(|p| p.to_str()),
        }
    }
}

#[cfg(test)]
mod stream_state_type_tests {
    use super::*;

    /// Read one of `vst3`'s `CString` constants (a NUL-terminated `*const c_char`).
    fn sdk_constant(value: *const std::os::raw::c_char) -> &'static str {
        // SAFETY: the argument is a `'static` NUL-terminated literal generated by `vst3`.
        unsafe { CStr::from_ptr(value) }
            .to_str()
            .expect("SDK state-type constants are ASCII")
    }

    #[test]
    fn state_type_values_match_the_sdk_constants() {
        use vst3::Steinberg::Vst::StateType;

        assert_eq!(
            StreamStateType::Project.attribute_value(),
            sdk_constant(StateType::kProject),
        );
        assert_eq!(
            StreamStateType::TrackPreset.attribute_value(),
            sdk_constant(StateType::kTrackPreset),
        );
        // The one defined value this host does not produce; asserted so that adding it later
        // starts from the SDK spelling rather than a guess.
        assert_eq!(sdk_constant(StateType::kDefault), "Default");
    }

    #[test]
    fn the_attribute_is_published_under_the_sdk_key() {
        assert_eq!(sdk_constant(PresetAttributes::kStateType), "StateType");
        assert_eq!(
            sdk_constant(PresetAttributes::kFilePathStringType),
            "FilePathString"
        );
    }

    /// A project restore and a preset load must not look alike to the plugin: `kProject` is
    /// "restored from a project loading", and everything else is what the SDK's
    /// `Helpers::isProjectState` reports as "coming from a preset".
    #[test]
    fn a_state_context_picks_the_matching_state_type() {
        assert_eq!(
            StreamStateType::from(&StateContext::Project),
            StreamStateType::Project
        );
        assert_eq!(
            StreamStateType::from(&StateContext::preset()),
            StreamStateType::TrackPreset
        );
        assert_eq!(
            StreamStateType::from(&StateContext::preset_from_path("/tmp/Lead.vstpreset")),
            StreamStateType::TrackPreset
        );
    }
}

/// Host implementation of `IBStream` over an in-memory buffer.
pub struct MemoryStream {
    inner: Mutex<MemBuf>,
    file_name: Option<Vec<u16>>,
    attributes: ComWrapper<HostAttributeList>,
}

impl MemoryStream {
    #[cfg(test)]
    fn new(data: Vec<u8>) -> Self {
        Self::with_metadata(data, StreamMetadata::default())
    }

    fn with_metadata(data: Vec<u8>, metadata: StreamMetadata<'_>) -> Self {
        let attributes = create_host_attribute_list();
        if let Some(state_type) = metadata.state_type {
            attributes.put(
                "StateType".to_string(),
                AttrValue::Str(state_type.attribute_value().encode_utf16().collect()),
            );
        }
        if let Some(file_path) = metadata.file_path {
            attributes.put(
                "FilePathString".to_string(),
                AttrValue::Str(file_path.encode_utf16().collect()),
            );
        }
        let file_name = metadata.file_name.map(|name| {
            name.encode_utf16()
                .take(MAX_STREAM_FILENAME_UNITS)
                .collect()
        });
        Self {
            inner: Mutex::new(MemBuf { data, pos: 0 }),
            file_name,
            attributes,
        }
    }

    /// A copy of everything written to the stream (used after `getState`).
    pub fn to_vec(&self) -> Vec<u8> {
        self.inner
            .lock()
            .map(|b| b.data.clone())
            .unwrap_or_default()
    }

    // Safe inner ops — also the unit-test surface for the read/write/seek logic.

    /// Write `src` at the cursor, zero-filling any gap a prior seek left past the end, and
    /// return the number of bytes written. `None` when the write would grow the buffer past
    /// [`MAX_STREAM_BYTES`] (or overflow `usize`), leaving the stream untouched.
    fn write_at_cursor(&self, src: &[u8]) -> Option<usize> {
        let mut b = self.inner.lock().ok()?;
        let end = b.pos.checked_add(src.len())?;
        if end > MAX_STREAM_BYTES {
            return None;
        }
        if end > b.data.len() {
            b.data.resize(end, 0);
        }
        let pos = b.pos;
        b.data[pos..end].copy_from_slice(src);
        b.pos = end;
        Some(src.len())
    }

    fn read_at_cursor(&self, n: usize) -> Vec<u8> {
        if let Ok(mut b) = self.inner.lock() {
            let start = b.pos.min(b.data.len());
            let end = (start + n).min(b.data.len());
            let out = b.data[start..end].to_vec();
            b.pos = end;
            out
        } else {
            Vec::new()
        }
    }

    /// Move the cursor and return its new absolute position. A position before the start is
    /// clamped to 0 (a lenient plugin seeking past the beginning still lands on a valid
    /// stream); one past [`MAX_STREAM_BYTES`] is rejected with `None`, because the cursor is
    /// what the next write grows the buffer to.
    fn seek_to(&self, pos: i64, mode: u32) -> Option<i64> {
        let mut b = self.inner.lock().ok()?;
        let base = match mode {
            SEEK_CUR => b.pos as i64,
            SEEK_END => b.data.len() as i64,
            _ => 0, // SEEK_SET
        };
        let new = base.checked_add(pos)?.max(0);
        if new > MAX_STREAM_BYTES as i64 {
            return None;
        }
        b.pos = new as usize;
        Some(new)
    }

    fn position(&self) -> i64 {
        self.inner.lock().map(|b| b.pos as i64).unwrap_or(0)
    }
}

// IBStream seek modes — fixed by the VST3 ABI. kIBSeekSet (0) is the `_` arm in seek_to.
const SEEK_CUR: u32 = 1; // kIBSeekCur
const SEEK_END: u32 = 2; // kIBSeekEnd

impl Class for MemoryStream {
    type Interfaces = (IBStream, IStreamAttributes);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_read: *mut i32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kResultFalse;
        }
        let bytes = self.read_at_cursor(num_bytes as usize);
        ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, bytes.len());
        if !num_bytes_read.is_null() {
            *num_bytes_read = bytes.len() as i32;
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kResultFalse;
        }
        let src = std::slice::from_raw_parts(buffer as *const u8, num_bytes as usize);
        // A refused write reports zero bytes written and kOutOfMemory: the plugin's own error
        // path is the only correct answer here, since panicking (or aborting on a capacity
        // overflow) would unwind out of a C++ call.
        let Some(written) = self.write_at_cursor(src) else {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kOutOfMemory;
        };
        if !num_bytes_written.is_null() {
            *num_bytes_written = written as i32;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        let Some(new) = self.seek_to(pos, mode as u32) else {
            return kInvalidArgument;
        };
        if !result.is_null() {
            *result = new;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        if pos.is_null() {
            return kResultFalse;
        }
        *pos = self.position();
        kResultOk
    }
}

impl IStreamAttributesTrait for MemoryStream {
    unsafe fn getFileName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        let Some(file_name) = self.file_name.as_ref() else {
            (*name).fill(0);
            return kResultFalse;
        };
        (*name).fill(0);
        let count = file_name.len().min((*name).len().saturating_sub(1));
        (&mut *name)[..count].copy_from_slice(&file_name[..count]);
        kResultOk
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map(|attributes| attributes.as_ptr())
            .unwrap_or(ptr::null_mut())
    }
}

/// Create an empty attributed stream for a plugin data/state write.
pub fn create_memory_stream_with_metadata(
    file_name: Option<&str>,
    state_type: StreamStateType,
) -> ComWrapper<MemoryStream> {
    ComWrapper::new(MemoryStream::with_metadata(
        Vec::new(),
        StreamMetadata {
            file_name,
            ..StreamMetadata::new(state_type)
        },
    ))
}

/// Create an attributed stream seeded for a plugin data/state read.
pub fn create_memory_stream_from_with_metadata(
    data: Vec<u8>,
    file_name: Option<&str>,
    state_type: StreamStateType,
) -> ComWrapper<MemoryStream> {
    ComWrapper::new(MemoryStream::with_metadata(
        data,
        StreamMetadata {
            file_name,
            ..StreamMetadata::new(state_type)
        },
    ))
}

/// Create the stream a `setState` call reads from, carrying the attributes that tell the
/// plugin where the bytes came from: the `StateType`, and — when the context names a source
/// file — that file's path and stem.
pub fn create_state_restore_stream(
    data: Vec<u8>,
    context: &StateContext,
) -> ComWrapper<MemoryStream> {
    ComWrapper::new(MemoryStream::with_metadata(
        data,
        StreamMetadata::for_state_context(context),
    ))
}

// --- Linux IRunLoop ------------------------------------------------------
// VSTGUI-based editors (and most non-JUCE plugin UIs) strictly require the
// host frame to also implement `Steinberg::Linux::IRunLoop`: the view
// registers file-descriptor event handlers (its X11 connection) and
// periodic timers with the host, and paints/responds ONLY when the host
// services them. Without this the editor attaches but stays black. The host
// must call `Plugin::service_run_loop()` on its UI thread regularly (every
// frame) while an editor is open.

/// What a plugin's editor registered with the host's run loop, shared
/// between the frame (registration, called by the plugin during attach) and
/// the plugin impl (servicing, driven by the host each UI frame).
#[cfg(target_os = "linux")]
pub struct RunLoopRegistry {
    pub handlers: Vec<(
        vst3::ComPtr<vst3::Steinberg::Linux::IEventHandler>,
        vst3::Steinberg::Linux::FileDescriptor,
    )>,
    pub timers: Vec<RunLoopTimer>,
}

#[cfg(target_os = "linux")]
pub struct RunLoopTimer {
    pub handler: vst3::ComPtr<vst3::Steinberg::Linux::ITimerHandler>,
    pub interval_ms: u64,
    pub due: std::time::Instant,
}

#[cfg(target_os = "linux")]
impl RunLoopRegistry {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
            timers: Vec::new(),
        }
    }
}

// The registry holds COM pointers into plugin code. They are only ever
// touched from the host's UI thread (registration inside `open_editor`,
// servicing inside `service_run_loop`, both UI-thread calls); the Send
// bound is inherited from `PluginInternal: Send` storage, the same
// pragmatics as the ComPtrs PluginImpl already holds.
#[cfg(target_os = "linux")]
unsafe impl Send for RunLoopRegistry {}

// Host implementation of `IPlugFrame` (all platforms) plus
// `Linux::IRunLoop` (Linux only). A plugin editor calls `resizeView` to ask
// the host to resize the window hosting its view (recorded; the host polls
// take_editor_resize_request), and on Linux registers its event
// handlers/timers via the IRunLoop half (serviced via
// `Plugin::service_run_loop`).
pub struct HostPlugFrame {
    requested: Arc<Mutex<Option<(i32, i32)>>>,
    #[cfg(target_os = "linux")]
    run_loop: Arc<Mutex<RunLoopRegistry>>,
}

impl HostPlugFrame {
    #[cfg(target_os = "linux")]
    pub fn new(
        requested: Arc<Mutex<Option<(i32, i32)>>>,
        run_loop: Arc<Mutex<RunLoopRegistry>>,
    ) -> Self {
        Self {
            requested,
            run_loop,
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new(requested: Arc<Mutex<Option<(i32, i32)>>>) -> Self {
        Self { requested }
    }
}

#[cfg(target_os = "linux")]
impl Class for HostPlugFrame {
    type Interfaces = (IPlugFrame, vst3::Steinberg::Linux::IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for HostPlugFrame {
    type Interfaces = (IPlugFrame,);
}

#[cfg(target_os = "linux")]
impl vst3::Steinberg::Linux::IRunLoopTrait for HostPlugFrame {
    unsafe fn registerEventHandler(
        &self,
        handler: *mut vst3::Steinberg::Linux::IEventHandler,
        fd: vst3::Steinberg::Linux::FileDescriptor,
    ) -> tresult {
        let Some(handler) = vst3::ComRef::from_raw(handler) else {
            return kInvalidArgument;
        };
        match self.run_loop.lock() {
            Ok(mut reg) => {
                reg.handlers.push((handler.to_com_ptr(), fd));
                kResultOk
            }
            Err(_) => kInternalError,
        }
    }

    unsafe fn unregisterEventHandler(
        &self,
        handler: *mut vst3::Steinberg::Linux::IEventHandler,
    ) -> tresult {
        match self.run_loop.lock() {
            Ok(mut reg) => {
                reg.handlers.retain(|(h, _)| h.as_ptr() != handler);
                kResultOk
            }
            Err(_) => kInternalError,
        }
    }

    unsafe fn registerTimer(
        &self,
        handler: *mut vst3::Steinberg::Linux::ITimerHandler,
        milliseconds: vst3::Steinberg::Linux::TimerInterval,
    ) -> tresult {
        let Some(handler) = vst3::ComRef::from_raw(handler) else {
            return kInvalidArgument;
        };
        let interval_ms = milliseconds.max(1);
        match self.run_loop.lock() {
            Ok(mut reg) => {
                reg.timers.push(RunLoopTimer {
                    handler: handler.to_com_ptr(),
                    interval_ms,
                    due: std::time::Instant::now() + std::time::Duration::from_millis(interval_ms),
                });
                kResultOk
            }
            Err(_) => kInternalError,
        }
    }

    unsafe fn unregisterTimer(
        &self,
        handler: *mut vst3::Steinberg::Linux::ITimerHandler,
    ) -> tresult {
        match self.run_loop.lock() {
            Ok(mut reg) => {
                reg.timers.retain(|t| t.handler.as_ptr() != handler);
                kResultOk
            }
            Err(_) => kInternalError,
        }
    }
}

impl IPlugFrameTrait for HostPlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let Some(view) = ComRef::<IPlugView>::from_raw(view) else {
            return kInvalidArgument;
        };
        if new_size.is_null() {
            return kInvalidArgument;
        }
        let r = &*new_size;
        let Some(width) = r.right.checked_sub(r.left).filter(|width| *width > 0) else {
            return kInvalidArgument;
        };
        let Some(height) = r.bottom.checked_sub(r.top).filter(|height| *height > 0) else {
            return kInvalidArgument;
        };

        match self.requested.lock() {
            Ok(mut slot) => *slot = Some((width, height)),
            Err(_) => return kInternalError,
        }
        // Drop the slot lock before invoking plugin code. `onSize` is required in this exact
        // callstack and may re-enter `resizeView`; retaining the mutex here would deadlock and
        // overwriting the slot after the callback would lose the nested request.
        view.onSize(new_size)
    }
}

/// Create a host plug-frame backed by a shared resize-request slot (and, on
/// Linux, a run-loop registry - see `RunLoopRegistry`).
#[cfg(target_os = "linux")]
pub fn create_host_plug_frame(
    requested: Arc<Mutex<Option<(i32, i32)>>>,
    run_loop: Arc<Mutex<RunLoopRegistry>>,
) -> ComWrapper<HostPlugFrame> {
    ComWrapper::new(HostPlugFrame::new(requested, run_loop))
}

/// Create a host plug-frame backed by a shared resize-request slot.
#[cfg(not(target_os = "linux"))]
pub fn create_host_plug_frame(
    requested: Arc<Mutex<Option<(i32, i32)>>>,
) -> ComWrapper<HostPlugFrame> {
    ComWrapper::new(HostPlugFrame::new(requested))
}

/// Cap on buffered editor feedback — the parameter changes and gesture events a plugin's editor
/// reports through `IComponentHandler`. Both are drained only by an optional host poll
/// (`Plugin::get_parameter_changes` / `Plugin::take_parameter_edits`), so a host that never polls
/// would otherwise grow them for the plugin's whole lifetime: dragging a knob emits one
/// `performEdit` per UI frame. Mirrors `MAX_OUTPUT_MIDI` on the outgoing side — pre-reserved so
/// steady-state pushes never reallocate, and pushes past the cap are dropped.
pub const MAX_EDITOR_FEEDBACK: usize = 4096;

/// Cap on the queued host-notification stream — the `IComponentHandler2` / `IUnitHandler` /
/// `IProgress` requests a plugin raises, drained by `Plugin::take_host_notifications`.
///
/// Unlike the editor-feedback caps, reaching this one is reported to the plugin: the handler
/// returns `kResultFalse` from `setDirty` / `requestOpenEditor` / `startGroupEdit` /
/// `finishGroupEdit` / `notifyUnitSelection` / `notifyProgramListChange` /
/// `notifyUnitByBusChange` / `IProgress::start` rather than silently discarding the request,
/// so a plugin that checks its result code learns the host refused it. Hosts must therefore
/// drain `take_host_notifications` regularly (once per UI frame is plenty) or the queue fills
/// and the plugin starts seeing refusals.
pub const MAX_HOST_NOTIFICATIONS: usize = 1024;
const MAX_CONTEXT_MENU_ITEMS: usize = 256;

struct PendingContextMenuItem {
    tag: i32,
    flags: i32,
    target: Option<ComPtr<IContextMenuTarget>>,
}

struct ContextMenuRegistry {
    next_menu_id: AtomicU64,
    pending: Mutex<HashMap<u64, Vec<PendingContextMenuItem>>>,
    owner_thread: ThreadId,
}

impl ContextMenuRegistry {
    fn new() -> Self {
        Self {
            next_menu_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::with_capacity(MAX_HOST_NOTIFICATIONS)),
            owner_thread: thread::current().id(),
        }
    }

    fn execute(&self, menu_id: u64, item_id: u32) -> crate::Result<()> {
        if thread::current().id() != self.owner_thread {
            return Err(crate::Error::Other(
                "context-menu targets must be invoked on the plugin control thread".to_string(),
            ));
        }
        let (tag, target) = {
            let mut menus = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let items = menus.get(&menu_id).ok_or_else(|| {
                crate::Error::Other("context menu is no longer pending".to_string())
            })?;
            let item = items.get(item_id as usize).ok_or_else(|| {
                crate::Error::Other("context-menu item id is out of range".to_string())
            })?;
            if item.flags & IContextMenuItem_::Flags_::kIsDisabled as i32 != 0
                || item.flags & IContextMenuItem_::Flags_::kIsSeparator as i32 != 0
            {
                return Err(crate::Error::Other(
                    "context-menu item is not executable".to_string(),
                ));
            }
            if item.target.is_none() {
                return Err(crate::Error::Other(
                    "context-menu item has no executable target".to_string(),
                ));
            }
            let tag = item.tag;
            let target = item.target.clone();
            menus.remove(&menu_id);
            (tag, target)
        };
        let result = unsafe {
            target
                .as_ref()
                .ok_or_else(|| crate::Error::Other("context-menu target disappeared".to_string()))?
                .executeMenuItem(tag)
        };
        if result == kResultOk || result == kResultTrue {
            Ok(())
        } else {
            Err(crate::Error::Other(format!(
                "plugin rejected context-menu command: {result:#x}"
            )))
        }
    }

    fn dismiss(&self, menu_id: u64) -> crate::Result<()> {
        if thread::current().id() != self.owner_thread {
            return Err(crate::Error::Other(
                "context menus must be dismissed on the plugin control thread".to_string(),
            ));
        }
        if self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&menu_id)
            .is_some()
        {
            Ok(())
        } else {
            Err(crate::Error::Other(
                "context menu is no longer pending".to_string(),
            ))
        }
    }
}

struct StoredContextMenuItem {
    item: IContextMenuItem,
    target: Option<ComPtr<IContextMenuTarget>>,
}

struct HostContextMenu {
    parameter_id: Option<u32>,
    items: Mutex<Vec<StoredContextMenuItem>>,
    notifications: Arc<Mutex<Vec<crate::plugin::HostNotification>>>,
    registry: Arc<ContextMenuRegistry>,
}

impl Class for HostContextMenu {
    type Interfaces = (IContextMenu,);
}

impl IContextMenuTrait for HostContextMenu {
    unsafe fn getItemCount(&self) -> i32 {
        self.items
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
            .min(i32::MAX as usize) as i32
    }

    unsafe fn getItem(
        &self,
        index: i32,
        item: *mut IContextMenuItem,
        target: *mut *mut IContextMenuTarget,
    ) -> tresult {
        if index < 0 || item.is_null() || target.is_null() {
            return kInvalidArgument;
        }
        *target = ptr::null_mut();
        let items = self
            .items
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(stored) = items.get(index as usize) else {
            return kInvalidArgument;
        };
        *item = stored.item;
        if let Some(stored_target) = stored.target.as_ref() {
            let owned_target = stored_target.clone();
            *target = owned_target.as_ptr();
            std::mem::forget(owned_target);
        }
        kResultOk
    }

    unsafe fn addItem(
        &self,
        item: *const IContextMenuItem,
        target: *mut IContextMenuTarget,
    ) -> tresult {
        if item.is_null() {
            return kInvalidArgument;
        }
        let target =
            ComRef::<IContextMenuTarget>::from_raw(target).map(|target| target.to_com_ptr());
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if items.len() >= MAX_CONTEXT_MENU_ITEMS {
            return kResultFalse;
        }
        items.push(StoredContextMenuItem {
            item: *item,
            target,
        });
        kResultOk
    }

    unsafe fn removeItem(
        &self,
        item: *const IContextMenuItem,
        target: *mut IContextMenuTarget,
    ) -> tresult {
        if item.is_null() {
            return kInvalidArgument;
        }
        let requested = &*item;
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(index) = items.iter().position(|stored| {
            stored.item.name == requested.name
                && stored.item.tag == requested.tag
                && stored.item.flags == requested.flags
                && stored
                    .target
                    .as_ref()
                    .map_or(ptr::null_mut(), ComPtr::as_ptr)
                    == target
        }) else {
            return kResultFalse;
        };
        items.remove(index);
        kResultOk
    }

    unsafe fn popup(&self, x: i32, y: i32) -> tresult {
        let items = self
            .items
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let public_items = items
            .iter()
            .enumerate()
            .map(|(index, stored)| crate::plugin::ContextMenuItem {
                item_id: index as u32,
                name: crate::internal::utils::vst_string_to_string(&stored.item.name),
                tag: stored.item.tag,
                flags: stored.item.flags,
            })
            .collect::<Vec<_>>();
        let pending_items = items
            .iter()
            .map(|stored| PendingContextMenuItem {
                tag: stored.item.tag,
                flags: stored.item.flags,
                target: stored.target.clone(),
            })
            .collect::<Vec<_>>();
        drop(items);

        let mut notifications = self
            .notifications
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if notifications.len() >= MAX_HOST_NOTIFICATIONS {
            return kResultFalse;
        }
        let mut pending = self
            .registry
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if pending.len() >= MAX_HOST_NOTIFICATIONS {
            return kResultFalse;
        }
        let menu_id = self.registry.next_menu_id.fetch_add(1, Ordering::Relaxed);
        pending.insert(menu_id, pending_items);
        notifications.push(crate::plugin::HostNotification::ContextMenuRequested {
            menu_id,
            parameter_id: self.parameter_id,
            x,
            y,
            items: public_items,
        });
        kResultOk
    }
}

// Component Handler implementation
//
// # Two independently ordered streams
//
// Everything a plugin reports through the component handler lands in one of two queues, and
// **the two are ordered only within themselves — there is no cross-ordering between them**:
//
// - `edits` — the per-parameter gesture log (`beginEdit`/`performEdit`/`endEdit`), drained by
//   `take_parameter_edits`.
// - `notifications` — the control-plane request log (`IComponentHandler2`, `IUnitHandler`,
//   `IComponentHandler3` context menus, `IProgress`), drained by `take_host_notifications`.
//
// `startGroupEdit` / `finishGroupEdit` therefore arrive as `HostNotification::GroupEditStarted`
// / `GroupEditFinished` in the *notification* stream while the parameter edits they are meant
// to bracket arrive in the *edit* stream. Nothing records where the bracket fell relative to a
// specific `ParameterEdit`, so a host cannot currently tell which edits belonged to a group —
// only that a group was opened and closed at some point between two drains. Treat the brackets
// as an "a multi-parameter change is in flight" hint (e.g. coalesce undo), not as a delimiter.
// Interleaving them into one ordered stream would change the public shape of both accessors
// and is not implemented.
pub struct ComponentHandler {
    pub(crate) parameter_mirror: Arc<super::parameter_mirror::ParameterMirror>,
    loading_thread: ThreadId,
    // Ordered log of begin/change/end gestures the editor reports, preserving their order so
    // the host can reconstruct each gesture (drained via `take_parameter_edits`). This is the
    // richer superset of the mirror (which keeps only the latest value per parameter).
    // Ordered against itself only — see the type comment about the group-edit brackets.
    edits: Arc<Mutex<Vec<crate::plugin::ParameterEdit>>>,
    // Union of every `restartComponent` flag the plugin has raised since the host last drained
    // it. A bitmask rather than a log: the flags are idempotent requests ("my latency changed",
    // "re-read my parameters"), so accumulating them is both complete and inherently bounded —
    // a plugin that spams restartComponent while nothing polls costs one word, not a queue.
    restart_flags: AtomicI32,
    // Ordered IComponentHandler2 / IUnitHandler / IProgress / context-menu requests. These are
    // control-plane work items, never executed from inside the plugin callback. Ordered against
    // themselves only — not against `edits`. Capped at MAX_HOST_NOTIFICATIONS, and a push past
    // the cap is *refused* (kResultFalse to the plugin) rather than dropped, so a host that
    // never drains makes the plugin's own requests start failing.
    notifications: Arc<Mutex<Vec<crate::plugin::HostNotification>>>,
    context_menus: Arc<ContextMenuRegistry>,
}

impl ComponentHandler {
    pub(crate) fn new(parameter_mirror: Arc<super::parameter_mirror::ParameterMirror>) -> Self {
        ComponentHandler {
            parameter_mirror,
            loading_thread: thread::current().id(),
            edits: Arc::new(Mutex::new(Vec::with_capacity(MAX_EDITOR_FEEDBACK))),
            restart_flags: AtomicI32::new(0),
            notifications: Arc::new(Mutex::new(Vec::with_capacity(MAX_HOST_NOTIFICATIONS))),
            context_menus: Arc::new(ContextMenuRegistry::new()),
        }
    }

    /// Take the accumulated `restartComponent` flags, clearing them.
    pub fn take_restart_flags(&self) -> crate::plugin::RestartFlags {
        crate::plugin::RestartFlags::from_bits(self.restart_flags.swap(0, Ordering::AcqRel))
    }

    /// Drain the ordered parameter-edit gesture log accumulated since the last call.
    ///
    /// Ordered relative to other parameter edits only. The `startGroupEdit`/`finishGroupEdit`
    /// brackets live in the separate [`Self::take_host_notifications`] stream with no recorded
    /// interleaving, so the edits that a group covered cannot be identified — see the type
    /// comment on [`ComponentHandler`].
    ///
    /// Capped at [`MAX_EDITOR_FEEDBACK`]; gestures past the cap are dropped (the plugin is not
    /// told, because `IComponentHandler` has no result code a plugin acts on here).
    pub fn take_parameter_edits(&self) -> Vec<crate::plugin::ParameterEdit> {
        // A COM FFI callback could be mid-push when a previous one panicked; recover the lock
        // rather than propagating a poison panic across the boundary.
        let mut edits = self.edits.lock().unwrap_or_else(|p| p.into_inner());
        // Drain in place rather than `mem::take`: taking the `Vec` would leave a zero-capacity
        // buffer behind, so the next editor gesture would reallocate on the COM callback path.
        edits.drain(..).collect()
    }

    /// Drain ordered host requests raised through `IComponentHandler2`, `IUnitHandler`,
    /// `IComponentHandler3` and `IProgress`.
    ///
    /// Ordered relative to other notifications only — never against
    /// [`Self::take_parameter_edits`]. In particular `GroupEditStarted`/`GroupEditFinished`
    /// cannot be correlated with the parameter edits they bracket; see the type comment on
    /// [`ComponentHandler`].
    ///
    /// **Drain this regularly.** The queue is capped at [`MAX_HOST_NOTIFICATIONS`] and a push
    /// past the cap returns `kResultFalse` to the plugin, so a host that never drains starts
    /// making the plugin's own `setDirty` / `requestOpenEditor` / group-edit /
    /// progress-reporting calls fail.
    pub fn take_host_notifications(&self) -> Vec<crate::plugin::HostNotification> {
        let mut notifications = self
            .notifications
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        notifications.drain(..).collect()
    }

    pub fn execute_context_menu_item(&self, menu_id: u64, item_id: u32) -> crate::Result<()> {
        self.context_menus.execute(menu_id, item_id)
    }

    pub fn dismiss_context_menu(&self, menu_id: u64) -> crate::Result<()> {
        self.context_menus.dismiss(menu_id)
    }

    // Append a gesture event, recovering a poisoned lock (these run on the COM FFI callback
    // path, where a panic would unwind across the C++ boundary — UB).
    fn push_edit(&self, edit: crate::plugin::ParameterEdit) {
        let mut edits = self.edits.lock().unwrap_or_else(|p| p.into_inner());
        if edits.len() < MAX_EDITOR_FEEDBACK {
            edits.push(edit);
        }
    }

    fn push_notification(&self, notification: crate::plugin::HostNotification) -> bool {
        let mut notifications = self
            .notifications
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if notifications.len() >= MAX_HOST_NOTIFICATIONS {
            return false;
        }
        notifications.push(notification);
        true
    }
}

impl Class for ComponentHandler {
    type Interfaces = (
        IComponentHandler,
        IComponentHandler2,
        IComponentHandler3,
        IUnitHandler,
        IUnitHandler2,
    );
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: u32) -> i32 {
        if thread::current().id() != self.loading_thread {
            self.parameter_mirror.record_off_thread_edit();
            return kResultFalse;
        }
        log::debug!("Host: Begin edit for parameter {}", id);
        self.push_edit(crate::plugin::ParameterEdit {
            id,
            kind: crate::plugin::ParameterEditKind::BeginGesture,
            value: None,
        });
        kResultOk
    }

    unsafe fn performEdit(&self, id: u32, value_normalized: f64) -> i32 {
        if thread::current().id() != self.loading_thread {
            self.parameter_mirror.record_off_thread_edit();
            return kResultFalse;
        }
        log::debug!(
            "Host: Perform edit for parameter {} = {}",
            id,
            value_normalized
        );
        // The latest value is independently visible to DSP and control-plane observers.
        self.parameter_mirror.publish_editor(id, value_normalized);
        // ...and as an ordered gesture event for the richer `take_parameter_edits` drain.
        self.push_edit(crate::plugin::ParameterEdit {
            id,
            kind: crate::plugin::ParameterEditKind::ValueChange,
            value: Some(value_normalized),
        });
        kResultOk
    }

    unsafe fn endEdit(&self, id: u32) -> i32 {
        if thread::current().id() != self.loading_thread {
            self.parameter_mirror.record_off_thread_edit();
            return kResultFalse;
        }
        log::debug!("Host: End edit for parameter {}", id);
        self.push_edit(crate::plugin::ParameterEdit {
            id,
            kind: crate::plugin::ParameterEditKind::EndGesture,
            value: None,
        });
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> i32 {
        log::debug!("Host: Restart component requested with flags: {flags:#x}");
        // Recorded for the host to poll (`Plugin::take_restart_flags`), not acted on here: the
        // host decides what a restart means for it. See `RestartFlags` for which flags this
        // library handles on the host's behalf (none, currently) and which need host action.
        self.restart_flags.fetch_or(flags, Ordering::AcqRel);
        if flags & vst3::Steinberg::Vst::RestartFlags_::kParamValuesChanged != 0 {
            self.parameter_mirror.request_controller_refresh();
        }
        kResultOk
    }
}

impl IComponentHandler3Trait for ComponentHandler {
    unsafe fn createContextMenu(
        &self,
        plug_view: *mut IPlugView,
        parameter_id: *const u32,
    ) -> *mut IContextMenu {
        if plug_view.is_null() {
            return ptr::null_mut();
        }
        let menu = ComWrapper::new(HostContextMenu {
            parameter_id: parameter_id.as_ref().copied(),
            items: Mutex::new(Vec::with_capacity(16)),
            notifications: self.notifications.clone(),
            registry: self.context_menus.clone(),
        });
        let Some(menu) = menu.to_com_ptr::<IContextMenu>() else {
            return ptr::null_mut();
        };
        let raw = menu.as_ptr();
        std::mem::forget(menu);
        raw
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: u8) -> i32 {
        log::debug!("Host: Plugin marked state as dirty (state: {})", state);
        if self.push_notification(crate::plugin::HostNotification::DirtyChanged(state != 0)) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn requestOpenEditor(&self, name: *const std::os::raw::c_char) -> i32 {
        log::debug!("Host: Plugin requested editor open");
        let name = if name.is_null() {
            None
        } else {
            Some(CStr::from_ptr(name).to_string_lossy().into_owned())
        };
        if self.push_notification(crate::plugin::HostNotification::OpenEditorRequested { name }) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    // The group brackets land in the notification stream while the edits they bracket land in
    // the gesture stream; nothing records the interleaving. See the `ComponentHandler` type
    // comment for what a host can and cannot conclude from them.
    unsafe fn startGroupEdit(&self) -> i32 {
        log::debug!("Host: Plugin started group edit");
        if self.push_notification(crate::plugin::HostNotification::GroupEditStarted) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn finishGroupEdit(&self) -> i32 {
        log::debug!("Host: Plugin finished group edit");
        if self.push_notification(crate::plugin::HostNotification::GroupEditFinished) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl IUnitHandlerTrait for ComponentHandler {
    unsafe fn notifyUnitSelection(&self, unit_id: i32) -> tresult {
        if self.push_notification(crate::plugin::HostNotification::UnitSelectionChanged { unit_id })
        {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn notifyProgramListChange(&self, list_id: i32, program_index: i32) -> tresult {
        if self.push_notification(crate::plugin::HostNotification::ProgramListChanged {
            list_id,
            program_index: (program_index >= 0).then_some(program_index),
        }) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl IUnitHandler2Trait for ComponentHandler {
    unsafe fn notifyUnitByBusChange(&self) -> tresult {
        if self.push_notification(crate::plugin::HostNotification::UnitByBusChanged) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

// Event List implementation
/// Cap on queued events. The input list's only drain is `process()`, which returns early while
/// the plugin isn't processing, so a host that sends MIDI to a stopped plugin would otherwise
/// grow this forever (and then dump every stale event into the first block once it starts). Far
/// above any single block's working set; same pre-reserve/drop-when-full policy as
/// `MAX_OUTPUT_MIDI`.
pub const MAX_QUEUED_EVENTS: usize = 4096;
const MAX_QUEUED_EVENT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

pub struct HostEventList {
    pub events: Mutex<Vec<PluginEvent>>,
    payload_bytes: AtomicUsize,
}

impl HostEventList {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::with_capacity(MAX_QUEUED_EVENTS)),
            payload_bytes: AtomicUsize::new(0),
        }
    }

    pub fn clear(&self) {
        match self.events.lock() {
            Ok(mut events) => {
                events.clear();
                self.payload_bytes.store(0, Ordering::Relaxed);
                log::trace!("HostEventList: Cleared all events");
            }
            Err(_) => {
                log::error!("HostEventList: Failed to lock events for clear");
            }
        }
    }

    /// Move queued events into reusable optional slots for allocation-free chunk routing.
    pub fn take_into_slots(&self, out: &mut Vec<Option<PluginEvent>>) {
        out.clear();
        if let Ok(mut events) = self.events.lock() {
            out.extend(events.drain(..).map(Some));
            self.payload_bytes.store(0, Ordering::Relaxed);
        }
    }

    /// Replace the queued events with `events`, reusing the existing allocation. Capped like
    /// every other path into the list; the excess is dropped with a warning.
    pub fn reset_with(&self, events: impl IntoIterator<Item = PluginEvent>) {
        let Ok(mut queued) = self.events.lock() else {
            log::error!("HostEventList: Failed to lock events for reset_with");
            return;
        };
        queued.clear();
        let mut payload_bytes = 0usize;
        for event in events {
            if queued.len() >= MAX_QUEUED_EVENTS {
                log::warn!("HostEventList: dropping event, queue full at {MAX_QUEUED_EVENTS}");
                break;
            }
            let next_payload_bytes = payload_bytes.saturating_add(event.payload_bytes());
            if next_payload_bytes > MAX_QUEUED_EVENT_PAYLOAD_BYTES {
                log::warn!(
                    "HostEventList: dropping event, payload budget exceeds \
                     {MAX_QUEUED_EVENT_PAYLOAD_BYTES} bytes"
                );
                continue;
            }
            payload_bytes = next_payload_bytes;
            queued.push(event);
        }
        self.payload_bytes.store(payload_bytes, Ordering::Relaxed);
    }

    /// True if the list currently holds no events.
    pub fn is_empty(&self) -> bool {
        self.events
            .lock()
            .map(|events| events.is_empty())
            .unwrap_or(true)
    }

    /// Move each queued event into `f`, leaving the list empty while retaining its backing
    /// allocation. Pointer-backed payloads therefore cross into the output queue without a
    /// second allocation or byte copy on the audio thread.
    pub fn drain_each(&self, mut f: impl FnMut(PluginEvent)) {
        if let Ok(mut events) = self.events.lock() {
            for event in events.drain(..) {
                f(event);
            }
            self.payload_bytes.store(0, Ordering::Relaxed);
        }
    }

    pub fn add_event(&self, event: PluginEvent) {
        match self.events.lock() {
            Ok(mut events) => {
                if events.len() >= MAX_QUEUED_EVENTS {
                    log::warn!(
                        "HostEventList: dropping event, queue full at {MAX_QUEUED_EVENTS} \
                         (is the plugin processing?)"
                    );
                    return;
                }
                let queued_payload_bytes = self.payload_bytes.load(Ordering::Relaxed);
                if queued_payload_bytes.saturating_add(event.payload_bytes())
                    > MAX_QUEUED_EVENT_PAYLOAD_BYTES
                {
                    log::warn!(
                        "HostEventList: dropping event, payload budget exceeds \
                         {MAX_QUEUED_EVENT_PAYLOAD_BYTES} bytes"
                    );
                    return;
                }
                self.payload_bytes.store(
                    queued_payload_bytes + event.payload_bytes(),
                    Ordering::Relaxed,
                );
                events.push(event);
                log::trace!(
                    "HostEventList: Added event via add_event, total count: {}",
                    events.len()
                );
            }
            Err(_) => {
                log::error!("HostEventList: Failed to lock events for add_event");
            }
        }
    }

    /// Deep-copy a raw SDK event into the owned list.
    pub fn add_raw_event(&self, event: &Event) -> bool {
        let Ok(event) = (unsafe { raw_event_to_plugin_event(event) }) else {
            return false;
        };
        self.add_event(event);
        true
    }
}

impl Default for HostEventList {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for HostEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for HostEventList {
    unsafe fn getEventCount(&self) -> i32 {
        match self.events.lock() {
            Ok(events) => events.len() as i32,
            Err(_) => {
                log::error!("HostEventList: Failed to lock events for getEventCount");
                0
            }
        }
    }

    unsafe fn getEvent(&self, index: i32, event: *mut Event) -> i32 {
        if event.is_null() {
            log::warn!("HostEventList: getEvent called with null event pointer");
            return kResultFalse;
        }

        if index < 0 {
            log::warn!(
                "HostEventList: getEvent called with negative index: {}",
                index
            );
            return kResultFalse;
        }

        match self.events.lock() {
            Ok(events) => {
                if let Some(e) = events.get(index as usize) {
                    match plugin_event_to_raw(e) {
                        Ok(raw) => {
                            *event = raw;
                            kResultOk
                        }
                        Err(()) => kResultFalse,
                    }
                } else {
                    log::warn!(
                        "HostEventList: getEvent index {} out of bounds (count: {})",
                        index,
                        events.len()
                    );
                    kResultFalse
                }
            }
            Err(_) => {
                log::error!("HostEventList: Failed to lock events for getEvent");
                kResultFalse
            }
        }
    }

    unsafe fn addEvent(&self, event: *mut Event) -> i32 {
        if event.is_null() {
            log::warn!("HostEventList: addEvent called with null event pointer");
            return kResultFalse;
        }

        match self.events.lock() {
            Ok(mut events) => {
                // Bound what a plugin can emit into the output list in a single block, so a
                // misbehaving plugin can't drive unbounded growth from inside `process()`.
                if events.len() >= MAX_QUEUED_EVENTS {
                    log::warn!(
                        "HostEventList: dropping plugin event, queue full at {MAX_QUEUED_EVENTS}"
                    );
                    return kResultFalse;
                }
                let owned = match raw_event_to_plugin_event(&*event) {
                    Ok(event) => event,
                    Err(()) => return kResultFalse,
                };
                let queued_payload_bytes = self.payload_bytes.load(Ordering::Relaxed);
                if queued_payload_bytes.saturating_add(owned.payload_bytes())
                    > MAX_QUEUED_EVENT_PAYLOAD_BYTES
                {
                    log::warn!(
                        "HostEventList: dropping plugin event, payload budget exceeds \
                         {MAX_QUEUED_EVENT_PAYLOAD_BYTES} bytes"
                    );
                    return kResultFalse;
                }
                self.payload_bytes.store(
                    queued_payload_bytes + owned.payload_bytes(),
                    Ordering::Relaxed,
                );
                events.push(owned);
                log::trace!("HostEventList: Added event, total count: {}", events.len());
                kResultOk
            }
            Err(_) => {
                log::error!("HostEventList: Failed to lock events for addEvent");
                kResultFalse
            }
        }
    }
}

#[allow(non_upper_case_globals, clippy::unnecessary_cast)]
fn plugin_event_to_raw(event: &PluginEvent) -> std::result::Result<Event, ()> {
    use Event_::EventTypes_::*;

    let mut raw: Event = unsafe { std::mem::zeroed() };
    raw.busIndex = event.bus_index;
    raw.sampleOffset = event.sample_offset;
    raw.ppqPosition = event.ppq_position;
    raw.flags = event.flags;
    match &event.data {
        PluginEventData::NoteOn {
            channel,
            pitch,
            tuning,
            velocity,
            length,
            note_id,
        } => {
            raw.r#type = kNoteOnEvent as u16;
            raw.__field0.noteOn = NoteOnEvent {
                channel: *channel,
                pitch: *pitch,
                tuning: *tuning,
                velocity: *velocity,
                length: *length,
                noteId: *note_id,
            };
        }
        PluginEventData::NoteOff {
            channel,
            pitch,
            velocity,
            note_id,
            tuning,
        } => {
            raw.r#type = kNoteOffEvent as u16;
            raw.__field0.noteOff = NoteOffEvent {
                channel: *channel,
                pitch: *pitch,
                velocity: *velocity,
                noteId: *note_id,
                tuning: *tuning,
            };
        }
        PluginEventData::Data { data_type, bytes } => {
            let size = u32::try_from(bytes.len()).map_err(|_| ())?;
            raw.r#type = kDataEvent as u16;
            raw.__field0.data = DataEvent {
                size,
                r#type: *data_type,
                bytes: bytes.as_ptr(),
            };
        }
        PluginEventData::PolyPressure {
            channel,
            pitch,
            pressure,
            note_id,
        } => {
            raw.r#type = kPolyPressureEvent as u16;
            raw.__field0.polyPressure = PolyPressureEvent {
                channel: *channel,
                pitch: *pitch,
                pressure: *pressure,
                noteId: *note_id,
            };
        }
        PluginEventData::NoteExpressionValue {
            type_id,
            note_id,
            value,
        } => {
            raw.r#type = kNoteExpressionValueEvent as u16;
            raw.__field0.noteExpressionValue = NoteExpressionValueEvent {
                typeId: *type_id,
                noteId: *note_id,
                value: *value,
            };
        }
        PluginEventData::NoteExpressionText {
            type_id,
            note_id,
            text,
        } => {
            raw.r#type = kNoteExpressionTextEvent as u16;
            raw.__field0.noteExpressionText = NoteExpressionTextEvent {
                typeId: *type_id,
                noteId: *note_id,
                textLen: u32::try_from(text.len()).map_err(|_| ())?,
                text: text.as_ptr(),
            };
        }
        PluginEventData::NoteExpressionIntValue {
            type_id,
            note_id,
            value,
        } => {
            raw.r#type = kNoteExpressionIntValueEvent as u16;
            raw.__field0.noteExpressionIntValue = NoteExpressionIntValueEvent {
                typeId: *type_id,
                noteId: *note_id,
                value: *value,
            };
        }
        PluginEventData::Chord {
            root,
            bass_note,
            mask,
            text,
        } => {
            raw.r#type = kChordEvent as u16;
            raw.__field0.chord = ChordEvent {
                root: *root,
                bassNote: *bass_note,
                mask: *mask,
                textLen: u16::try_from(text.len()).map_err(|_| ())?,
                text: text.as_ptr(),
            };
        }
        PluginEventData::Scale { root, mask, text } => {
            raw.r#type = kScaleEvent as u16;
            raw.__field0.scale = ScaleEvent {
                root: *root,
                mask: *mask,
                textLen: u16::try_from(text.len()).map_err(|_| ())?,
                text: text.as_ptr(),
            };
        }
        PluginEventData::LegacyMidiCcOut {
            control_number,
            channel,
            value,
            value2,
        } => {
            raw.r#type = kLegacyMIDICCOutEvent as u16;
            // VST3's `int8` is `c_char`, which is signed on macOS/x86 and unsigned on ARM
            // Linux, so these fields must be cast rather than assigned.
            raw.__field0.midiCCOut = LegacyMIDICCOutEvent {
                controlNumber: *control_number,
                channel: *channel as c_char,
                value: *value as c_char,
                value2: *value2 as c_char,
            };
        }
    }
    Ok(raw)
}

#[allow(non_upper_case_globals, clippy::unnecessary_cast)]
unsafe fn raw_event_to_plugin_event(raw: &Event) -> std::result::Result<PluginEvent, ()> {
    use Event_::EventTypes_::*;

    unsafe fn copy_bytes(ptr: *const u8, len: usize) -> std::result::Result<Vec<u8>, ()> {
        if len > MAX_EVENT_PAYLOAD_BYTES || (len != 0 && ptr.is_null()) {
            return Err(());
        }
        Ok(if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
        })
    }

    unsafe fn copy_text(ptr: *const u16, len: usize) -> std::result::Result<Vec<u16>, ()> {
        if len > MAX_EVENT_TEXT_UNITS || (len != 0 && ptr.is_null()) {
            return Err(());
        }
        Ok(if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
        })
    }

    let data = unsafe {
        match raw.r#type as u32 {
            t if t == kNoteOnEvent as u32 => {
                let value = raw.__field0.noteOn;
                PluginEventData::NoteOn {
                    channel: value.channel,
                    pitch: value.pitch,
                    tuning: value.tuning,
                    velocity: value.velocity,
                    length: value.length,
                    note_id: value.noteId,
                }
            }
            t if t == kNoteOffEvent as u32 => {
                let value = raw.__field0.noteOff;
                PluginEventData::NoteOff {
                    channel: value.channel,
                    pitch: value.pitch,
                    velocity: value.velocity,
                    note_id: value.noteId,
                    tuning: value.tuning,
                }
            }
            t if t == kDataEvent as u32 => {
                let value = raw.__field0.data;
                PluginEventData::Data {
                    data_type: value.r#type,
                    bytes: copy_bytes(value.bytes, usize::try_from(value.size).map_err(|_| ())?)?,
                }
            }
            t if t == kPolyPressureEvent as u32 => {
                let value = raw.__field0.polyPressure;
                PluginEventData::PolyPressure {
                    channel: value.channel,
                    pitch: value.pitch,
                    pressure: value.pressure,
                    note_id: value.noteId,
                }
            }
            t if t == kNoteExpressionValueEvent as u32 => {
                let value = raw.__field0.noteExpressionValue;
                PluginEventData::NoteExpressionValue {
                    type_id: value.typeId,
                    note_id: value.noteId,
                    value: value.value,
                }
            }
            t if t == kNoteExpressionTextEvent as u32 => {
                let value = raw.__field0.noteExpressionText;
                PluginEventData::NoteExpressionText {
                    type_id: value.typeId,
                    note_id: value.noteId,
                    text: copy_text(value.text, usize::try_from(value.textLen).map_err(|_| ())?)?,
                }
            }
            t if t == kNoteExpressionIntValueEvent as u32 => {
                let value = raw.__field0.noteExpressionIntValue;
                PluginEventData::NoteExpressionIntValue {
                    type_id: value.typeId,
                    note_id: value.noteId,
                    value: value.value,
                }
            }
            t if t == kChordEvent as u32 => {
                let value = raw.__field0.chord;
                PluginEventData::Chord {
                    root: value.root,
                    bass_note: value.bassNote,
                    mask: value.mask,
                    text: copy_text(value.text, usize::from(value.textLen))?,
                }
            }
            t if t == kScaleEvent as u32 => {
                let value = raw.__field0.scale;
                PluginEventData::Scale {
                    root: value.root,
                    mask: value.mask,
                    text: copy_text(value.text, usize::from(value.textLen))?,
                }
            }
            t if t == kLegacyMIDICCOutEvent as u32 => {
                let value = raw.__field0.midiCCOut;
                PluginEventData::LegacyMidiCcOut {
                    control_number: value.controlNumber,
                    channel: value.channel as i8,
                    value: value.value as u8,
                    value2: value.value2 as u8,
                }
            }
            _ => return Err(()),
        }
    };
    Ok(PluginEvent {
        bus_index: raw.busIndex,
        sample_offset: raw.sampleOffset,
        ppq_position: raw.ppqPosition,
        flags: raw.flags,
        data,
    })
}

pub fn create_event_list() -> ComWrapper<HostEventList> {
    ComWrapper::new(HostEventList::new())
}

pub use super::parameter_changes::ParameterChanges;

#[cfg(test)]
mod host_attr_tests {
    use super::*;

    #[test]
    fn attribute_list_round_trips_each_type() {
        let list = HostAttributeList::new();
        list.put("i".into(), AttrValue::Int(42));
        list.put("f".into(), AttrValue::Float(1.5));
        list.put("s".into(), AttrValue::Str(vec![72, 105])); // "Hi"
        list.put("b".into(), AttrValue::Bin(vec![1, 2, 3]));

        assert_eq!(list.get_value("i"), Some(AttrValue::Int(42)));
        assert_eq!(list.get_value("f"), Some(AttrValue::Float(1.5)));
        assert_eq!(list.get_value("s"), Some(AttrValue::Str(vec![72, 105])));
        assert_eq!(list.get_value("b"), Some(AttrValue::Bin(vec![1, 2, 3])));
        assert_eq!(list.get_value("missing"), None);
    }
}

#[cfg(test)]
mod component_handler_tests {
    use super::*;
    use crate::plugin::{ContextMenuItem, HostNotification, ParameterEdit, ParameterEditKind};

    struct TestContextMenuTarget {
        calls: Arc<Mutex<Vec<i32>>>,
    }

    impl Class for TestContextMenuTarget {
        type Interfaces = (IContextMenuTarget,);
    }

    impl IContextMenuTargetTrait for TestContextMenuTarget {
        unsafe fn executeMenuItem(&self, tag: i32) -> tresult {
            self.calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(tag);
            kResultOk
        }
    }

    fn context_menu_item(name: &str, tag: i32, flags: i32) -> IContextMenuItem {
        let mut item = IContextMenuItem {
            name: [0; 128],
            tag,
            flags,
        };
        for (destination, source) in item.name.iter_mut().zip(name.encode_utf16()) {
            *destination = source;
        }
        item
    }

    #[test]
    fn captures_begin_perform_end_in_order_and_drains() {
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(vec![5]),
        ));

        // Drive a full gesture: mouse-down, two drag values, mouse-up.
        unsafe {
            handler.beginEdit(5);
            handler.performEdit(5, 0.25);
            handler.performEdit(5, 0.5);
            handler.endEdit(5);
        }

        let edits = handler.take_parameter_edits();
        assert_eq!(
            edits,
            vec![
                ParameterEdit {
                    id: 5,
                    kind: ParameterEditKind::BeginGesture,
                    value: None,
                },
                ParameterEdit {
                    id: 5,
                    kind: ParameterEditKind::ValueChange,
                    value: Some(0.25),
                },
                ParameterEdit {
                    id: 5,
                    kind: ParameterEditKind::ValueChange,
                    value: Some(0.5),
                },
                ParameterEdit {
                    id: 5,
                    kind: ParameterEditKind::EndGesture,
                    value: None,
                },
            ]
        );

        // The gesture drain does not consume the latest-value mirror.
        assert!(handler.take_parameter_edits().is_empty());
        assert_eq!(handler.parameter_mirror.take_api(), vec![(5, 0.5)]);
    }

    #[test]
    fn off_thread_editor_edits_are_rejected_and_counted() {
        let mirror = Arc::new(super::super::parameter_mirror::ParameterMirror::new(vec![
            5,
        ]));
        let handler = Arc::new(ComponentHandler::new(mirror.clone()));
        let result = std::thread::spawn(move || unsafe {
            (
                handler.beginEdit(5),
                handler.performEdit(5, 0.7),
                handler.endEdit(5),
            )
        })
        .join()
        .expect("off-thread callback");
        assert_eq!(result, (kResultFalse, kResultFalse, kResultFalse));
        assert_eq!(mirror.feedback_loss_count(), 3);
        assert!(mirror.take_observer(128).is_empty());
        let mut dsp = Vec::new();
        mirror.drain_dsp(|id, value| {
            dsp.push((id, value));
            true
        });
        assert!(dsp.is_empty());
    }

    /// Both editor-feedback buffers are drained only by an optional host poll, so a host that
    /// never polls must not be able to grow them without bound — dragging a knob emits one
    /// `performEdit` (and one gesture event) per UI frame, for as long as the editor is open.
    #[test]
    fn editor_feedback_is_capped_when_the_host_never_polls() {
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(vec![7]),
        ));

        // Simulate a very long drag: far more edits than the cap, never polled.
        unsafe {
            for i in 0..(MAX_EDITOR_FEEDBACK * 2) {
                handler.performEdit(7, (i % 100) as f64 / 100.0);
            }
        }

        assert_eq!(handler.parameter_mirror.take_api().len(), 1);
        let edits = handler.take_parameter_edits();
        assert_eq!(
            edits.len(),
            MAX_EDITOR_FEEDBACK,
            "the gesture log must stop at the cap too"
        );

        // Draining keeps the buffer's capacity, so the next gesture doesn't reallocate on the
        // COM callback path.
        assert!(handler.take_parameter_edits().is_empty());
        assert!(handler.edits.lock().unwrap().capacity() >= MAX_EDITOR_FEEDBACK);
    }

    #[test]
    fn handler2_requests_are_ordered_and_report_backpressure() {
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(Vec::new()),
        ));
        unsafe {
            assert_eq!(handler.setDirty(1), kResultOk);
            assert_eq!(handler.requestOpenEditor(c"editor".as_ptr()), kResultOk);
            assert_eq!(handler.startGroupEdit(), kResultOk);
            assert_eq!(handler.finishGroupEdit(), kResultOk);
        }
        let notifications = handler.take_host_notifications();
        assert_eq!(
            notifications,
            vec![
                HostNotification::DirtyChanged(true),
                HostNotification::OpenEditorRequested {
                    name: Some("editor".to_string())
                },
                HostNotification::GroupEditStarted,
                HostNotification::GroupEditFinished,
            ]
        );

        unsafe {
            for _ in 0..MAX_HOST_NOTIFICATIONS {
                assert_eq!(handler.setDirty(0), kResultOk);
            }
            assert_eq!(handler.setDirty(1), kResultFalse);
        }
        assert_eq!(
            handler.take_host_notifications().len(),
            MAX_HOST_NOTIFICATIONS
        );
    }

    #[test]
    fn handler3_context_menu_preserves_items_and_executes_plugin_target() {
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(Vec::new()),
        ));
        let handler_wrapper = ComWrapper::new(handler);
        assert!(
            handler_wrapper.as_com_ref::<IComponentHandler3>().is_some(),
            "controllers must be able to query IComponentHandler3"
        );

        let calls = Arc::new(Mutex::new(Vec::new()));
        let target = ComWrapper::new(TestContextMenuTarget {
            calls: calls.clone(),
        });
        let target = target.as_com_ref::<IContextMenuTarget>().unwrap();
        let parameter_id = 42;
        let menu = unsafe {
            handler_wrapper.createContextMenu(
                std::ptr::NonNull::<IPlugView>::dangling().as_ptr(),
                &parameter_id,
            )
        };
        let menu = unsafe { ComPtr::<IContextMenu>::from_raw(menu) }.unwrap();
        let reset = context_menu_item("Reset", 17, 0);
        let separator = context_menu_item("", 0, IContextMenuItem_::Flags_::kIsSeparator as i32);
        unsafe {
            assert_eq!(menu.addItem(&reset, target.as_ptr()), kResultOk);
            assert_eq!(menu.addItem(&separator, ptr::null_mut()), kResultOk);
            assert_eq!(menu.getItemCount(), 2);
            let mut returned = context_menu_item("", 0, 0);
            let mut returned_target = ptr::null_mut();
            assert_eq!(
                menu.getItem(0, &mut returned, &mut returned_target),
                kResultOk
            );
            assert_eq!(
                crate::internal::utils::vst_string_to_string(&returned.name),
                "Reset"
            );
            assert_eq!(returned.tag, 17);
            assert!(!returned_target.is_null());
            drop(ComPtr::<IContextMenuTarget>::from_raw(returned_target));
            assert_eq!(menu.popup(11, 23), kResultOk);
        }
        drop(menu);

        let notifications = handler_wrapper.take_host_notifications();
        let (menu_id, items) = match notifications.as_slice() {
            [
                HostNotification::ContextMenuRequested {
                    menu_id,
                    parameter_id: Some(42),
                    x: 11,
                    y: 23,
                    items,
                },
            ] => (*menu_id, items),
            other => panic!("unexpected context-menu notification: {other:?}"),
        };
        assert_eq!(
            items,
            &[
                ContextMenuItem {
                    item_id: 0,
                    name: "Reset".to_string(),
                    tag: 17,
                    flags: 0,
                },
                ContextMenuItem {
                    item_id: 1,
                    name: String::new(),
                    tag: 0,
                    flags: IContextMenuItem_::Flags_::kIsSeparator as i32,
                },
            ]
        );
        assert!(items[1].is_separator());

        handler_wrapper
            .execute_context_menu_item(menu_id, 0)
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![17]);
        assert!(
            handler_wrapper
                .execute_context_menu_item(menu_id, 0)
                .is_err(),
            "a popup can only be completed once"
        );
    }

    #[test]
    fn handler3_rejects_invalid_views_and_releases_dismissed_targets() {
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(Vec::new()),
        ));
        assert!(unsafe { handler.createContextMenu(ptr::null_mut(), ptr::null()) }.is_null());

        let menu = unsafe {
            handler.createContextMenu(
                std::ptr::NonNull::<IPlugView>::dangling().as_ptr(),
                ptr::null(),
            )
        };
        let menu = unsafe { ComPtr::<IContextMenu>::from_raw(menu) }.unwrap();
        let separator = context_menu_item("", 0, IContextMenuItem_::Flags_::kIsSeparator as i32);
        unsafe {
            assert_eq!(menu.addItem(&separator, ptr::null_mut()), kResultOk);
            assert_eq!(menu.popup(0, 0), kResultOk);
        }
        let notification = handler.take_host_notifications().pop().unwrap();
        let HostNotification::ContextMenuRequested { menu_id, .. } = notification else {
            panic!("expected context-menu notification");
        };
        handler.dismiss_context_menu(menu_id).unwrap();
        assert!(handler.dismiss_context_menu(menu_id).is_err());
    }

    #[test]
    fn unit_handler_requests_are_ordered_and_preserve_whole_list_changes() {
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(Vec::new()),
        ));
        unsafe {
            assert_eq!(handler.notifyUnitSelection(7), kResultOk);
            assert_eq!(handler.notifyProgramListChange(11, 3), kResultOk);
            assert_eq!(handler.notifyProgramListChange(11, -1), kResultOk);
            assert_eq!(handler.notifyUnitByBusChange(), kResultOk);
        }
        let notifications = handler.take_host_notifications();
        assert_eq!(
            notifications,
            vec![
                HostNotification::UnitSelectionChanged { unit_id: 7 },
                HostNotification::ProgramListChanged {
                    list_id: 11,
                    program_index: Some(3),
                },
                HostNotification::ProgramListChanged {
                    list_id: 11,
                    program_index: None,
                },
                HostNotification::UnitByBusChanged,
            ]
        );
        assert!(!notifications[0].invalidates_unit_cache());
        assert!(
            notifications[1..]
                .iter()
                .all(HostNotification::invalidates_unit_cache)
        );
        assert!(handler.take_host_notifications().is_empty());
    }

    /// `restartComponent` is how a plugin tells the host something about it changed. Every flag
    /// used to be acknowledged and dropped; they must survive until the host drains them.
    #[test]
    fn restart_flags_accumulate_until_drained() {
        use vst3::Steinberg::Vst::RestartFlags_ as Flags;
        let handler = ComponentHandler::new(Arc::new(
            super::super::parameter_mirror::ParameterMirror::new(Vec::new()),
        ));
        assert!(handler.take_restart_flags().is_empty());

        // Two separate restarts, e.g. a preset load followed by a mode switch.
        unsafe {
            handler.restartComponent(Flags::kParamValuesChanged | Flags::kParamTitlesChanged);
            handler.restartComponent(Flags::kLatencyChanged);
        }

        let flags = handler.take_restart_flags();
        assert!(flags.param_values_changed());
        assert!(flags.param_titles_changed());
        assert!(flags.latency_changed());
        assert!(!flags.io_changed());

        // Draining clears them, so the host doesn't act on the same request twice.
        assert!(handler.take_restart_flags().is_empty());

        // A plugin that spams restarts while nothing polls costs one word, not a queue.
        unsafe {
            for _ in 0..10_000 {
                handler.restartComponent(Flags::kIoChanged);
            }
        }
        let flags = handler.take_restart_flags();
        assert!(flags.io_changed());
        assert_eq!(flags.bits(), Flags::kIoChanged);
    }
}

#[cfg(test)]
mod host_application_tests {
    use super::*;

    #[test]
    fn interface_support_reports_plugin_side_interfaces_only() {
        let host = create_host_application()
            .to_com_ptr::<IPlugInterfaceSupport>()
            .unwrap();
        unsafe {
            let mut process_context = IProcessContextRequirements::IID;
            assert_eq!(
                host.isPlugInterfaceSupported(&mut process_context as *mut _ as *const TUID,),
                kResultTrue
            );
            let mut remap_param_id = IRemapParamID::IID;
            assert_eq!(
                host.isPlugInterfaceSupported(&mut remap_param_id as *mut _ as *const TUID),
                kResultTrue
            );

            let mut component_handler = IComponentHandler::IID;
            assert_eq!(
                host.isPlugInterfaceSupported(&mut component_handler as *mut _ as *const TUID,),
                kResultFalse
            );
            assert_eq!(
                host.isPlugInterfaceSupported(ptr::null_mut()),
                kInvalidArgument
            );
        }
    }

    #[test]
    fn create_instance_requires_matching_class_and_interface_ids() {
        let host = create_host_application()
            .to_com_ptr::<IHostApplication>()
            .unwrap();
        unsafe {
            let mut message_cid = IMessage::IID;
            let mut message_iid = IMessage::IID;
            let mut raw = ptr::null_mut();
            assert_eq!(
                host.createInstance(
                    &mut message_cid as *mut _ as *mut TUID,
                    &mut message_iid as *mut _ as *mut TUID,
                    &mut raw,
                ),
                kResultTrue
            );
            assert!(!raw.is_null());
            drop(ComPtr::<IMessage>::from_raw(raw.cast::<IMessage>()));

            let mut attributes_iid = IAttributeList::IID;
            raw = std::ptr::NonNull::<std::ffi::c_void>::dangling().as_ptr();
            assert_eq!(
                host.createInstance(
                    &mut message_cid as *mut _ as *mut TUID,
                    &mut attributes_iid as *mut _ as *mut TUID,
                    &mut raw,
                ),
                kNoInterface
            );
            assert!(raw.is_null());

            assert_eq!(
                host.createInstance(
                    ptr::null_mut(),
                    &mut message_iid as *mut _ as *mut TUID,
                    &mut raw,
                ),
                kInvalidArgument
            );
        }
    }

    #[test]
    fn progress_callbacks_are_bounded_ordered_and_polled() {
        use crate::plugin::{HostNotification, ProgressKind};

        let host = create_host_application();
        let progress = host.to_com_ptr::<IProgress>().unwrap();
        let description: Vec<u16> = "Loading samples\0".encode_utf16().collect();
        let mut id = 0;
        unsafe {
            assert_eq!(
                progress.start(
                    IProgress_::ProgressType_::AsyncStateRestoration,
                    description.as_ptr(),
                    &mut id,
                ),
                kResultOk
            );
            assert_ne!(id, 0);
            assert_eq!(progress.update(id, 0.5), kResultOk);
            assert_eq!(progress.finish(id), kResultOk);
            assert_eq!(progress.update(id, 0.75), kResultFalse);
            assert_eq!(progress.update(u64::MAX, 0.5), kResultFalse);
            assert_eq!(progress.update(id, f64::NAN), kInvalidArgument);
        }
        assert_eq!(
            host.take_progress_notifications(),
            vec![
                HostNotification::ProgressStarted {
                    id,
                    kind: ProgressKind::AsyncStateRestoration,
                    description: Some("Loading samples".to_string()),
                },
                HostNotification::ProgressUpdated {
                    id,
                    value: crate::plugin::ProgressValue::new(0.5).unwrap(),
                },
                HostNotification::ProgressFinished { id },
            ]
        );
    }

    #[test]
    fn progress_reports_backpressure_instead_of_false_success() {
        let host = create_host_application();
        let progress = host.to_com_ptr::<IProgress>().unwrap();
        let mut id = 0;
        unsafe {
            assert_eq!(
                progress.start(
                    IProgress_::ProgressType_::UIBackgroundTask,
                    ptr::null(),
                    &mut id,
                ),
                kResultOk
            );
            for _ in 1..MAX_HOST_NOTIFICATIONS {
                assert_eq!(progress.update(id, 0.25), kResultOk);
            }
            assert_eq!(progress.update(id, 0.5), kResultFalse);
            assert_eq!(progress.finish(id), kResultFalse);
        }
        assert_eq!(
            host.take_progress_notifications().len(),
            MAX_HOST_NOTIFICATIONS
        );
        unsafe {
            assert_eq!(progress.finish(id), kResultOk);
        }
    }
}

#[cfg(test)]
mod connection_proxy_tests {
    use super::*;

    #[derive(Default)]
    struct RecordingConnectionPoint {
        notifications: AtomicUsize,
        captured: Mutex<Vec<(String, i64, Vec<u8>)>>,
    }

    impl Class for RecordingConnectionPoint {
        type Interfaces = (IConnectionPoint,);
    }

    impl IConnectionPointTrait for RecordingConnectionPoint {
        unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
            kResultOk
        }

        unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
            kResultOk
        }

        unsafe fn notify(&self, message: *mut IMessage) -> tresult {
            self.notifications.fetch_add(1, Ordering::Relaxed);
            if let Some(message) = ComRef::<IMessage>::from_raw(message) {
                let id = message.getMessageID();
                if !id.is_null() {
                    let id = CStr::from_ptr(id).to_string_lossy().into_owned();
                    let mut count = 0;
                    let mut binary = ptr::null();
                    let mut size = 0;
                    if let Some(attrs) = ComRef::<IAttributeList>::from_raw(message.getAttributes())
                    {
                        let _ = attrs.getInt(c"count".as_ptr(), &mut count);
                        let _ = attrs.getBinary(c"blob".as_ptr(), &mut binary, &mut size);
                    }
                    let binary = if binary.is_null() {
                        Vec::new()
                    } else {
                        std::slice::from_raw_parts(binary.cast::<u8>(), size as usize).to_vec()
                    };
                    self.captured.lock().unwrap().push((id, count, binary));
                }
            }
            kResultOk
        }
    }

    /// A proxy gated to the calling thread, plus the endpoint it forwards to.
    fn proxy_with_destination() -> (ConnectionProxy, ComWrapper<RecordingConnectionPoint>) {
        let destination = ComWrapper::new(RecordingConnectionPoint::default());
        let destination_ptr = destination
            .to_com_ptr::<IConnectionPoint>()
            .expect("RecordingConnectionPoint declares IConnectionPoint");
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ConnectionDelivery::new(
            destination_ptr,
            Arc::clone(&dropped),
            Arc::new(ConnectionMessageCounters::default()),
        );
        let proxy = ConnectionProxy::new(delivery, thread::current().id(), "test", dropped);
        (proxy, destination)
    }

    #[test]
    fn connection_proxy_forwards_only_on_its_control_thread() {
        let destination = ComWrapper::new(RecordingConnectionPoint::default());
        let destination_ptr = destination.to_com_ptr::<IConnectionPoint>().unwrap();
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ConnectionDelivery::new(
            destination_ptr,
            Arc::clone(&dropped),
            Arc::new(ConnectionMessageCounters::default()),
        );
        let proxy = ComWrapper::new(ConnectionProxy::new(
            delivery,
            thread::current().id(),
            "test",
            dropped,
        ));
        let proxy_ptr = proxy.to_com_ptr::<IConnectionPoint>().unwrap();
        let message = create_host_message().to_com_ptr::<IMessage>().unwrap();

        unsafe {
            assert_eq!(proxy_ptr.notify(message.as_ptr()), kResultOk);
        }
        assert_eq!(destination.notifications.load(Ordering::Relaxed), 1);

        let message_ptr = message.as_ptr() as usize;
        let result =
            std::thread::spawn(move || unsafe { proxy_ptr.notify(message_ptr as *mut IMessage) })
                .join()
                .unwrap();
        assert_eq!(result, kResultFalse);
        assert_eq!(destination.notifications.load(Ordering::Relaxed), 1);
    }

    /// The drop is the SDK's behaviour, but it must be countable — otherwise a plugin whose
    /// meters never update looks identical to a plugin that sends nothing.
    #[test]
    fn off_thread_notifies_are_counted_so_the_drop_is_diagnosable() {
        let (proxy, destination) = proxy_with_destination();
        let message = create_host_message().to_com_ptr::<IMessage>().unwrap();
        let message_ptr = message.as_ptr() as usize;

        let results = thread::scope(|scope| {
            scope
                .spawn(|| {
                    (0..3)
                        .map(|_| unsafe { proxy.notify(message_ptr as *mut IMessage) })
                        .collect::<Vec<_>>()
                })
                .join()
                .expect("notify never panics")
        });

        assert_eq!(results, vec![kResultFalse; 3]);
        assert_eq!(destination.notifications.load(Ordering::Relaxed), 0);
        assert_eq!(proxy.dropped_message_count(), 3);
    }

    /// Only the thread gate increments the counter: a forwarded message and a malformed one
    /// must not inflate the "your plugin is messaging off-thread" signal.
    #[test]
    fn forwarded_and_null_notifies_do_not_count_as_off_thread_drops() {
        let (proxy, destination) = proxy_with_destination();
        let message = create_host_message().to_com_ptr::<IMessage>().unwrap();

        assert_eq!(unsafe { proxy.notify(message.as_ptr()) }, kResultOk);
        assert_eq!(unsafe { proxy.notify(ptr::null_mut()) }, kResultFalse);

        assert_eq!(destination.notifications.load(Ordering::Relaxed), 1);
        assert_eq!(proxy.dropped_message_count(), 0);
    }

    #[test]
    fn off_thread_snapshot_survives_message_mutation() {
        let (proxy, destination) = proxy_with_destination();
        let message = create_host_message();
        *message.id.lock().unwrap() = Some(std::ffi::CString::new("before").unwrap());
        message.attributes.put("count".into(), AttrValue::Int(7));
        message
            .attributes
            .put("blob".into(), AttrValue::Bin(vec![1, 2, 3]));
        let pointer = message.to_com_ptr::<IMessage>().unwrap();
        let raw = pointer.as_ptr() as usize;
        let result = thread::scope(|scope| {
            scope
                .spawn(|| unsafe { proxy.notify(raw as *mut IMessage) })
                .join()
                .unwrap()
        });
        assert_eq!(result, kResultOk);
        *message.id.lock().unwrap() = Some(std::ffi::CString::new("after").unwrap());
        message.attributes.put("count".into(), AttrValue::Int(99));
        message
            .attributes
            .put("blob".into(), AttrValue::Bin(vec![9]));
        proxy.delivery.pump();
        assert_eq!(
            *destination.captured.lock().unwrap(),
            vec![("before".into(), 7, vec![1, 2, 3])]
        );
        assert_eq!(proxy.delivery.counters.queued.load(Ordering::Relaxed), 1);
        assert_eq!(proxy.delivery.counters.delivered.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn off_thread_snapshot_rejects_contention_overflow_and_full_pool() {
        let (proxy, _) = proxy_with_destination();
        let message = create_host_message();
        *message.id.lock().unwrap() = Some(std::ffi::CString::new("data").unwrap());
        let pointer = message.to_com_ptr::<IMessage>().unwrap();
        let raw = pointer.as_ptr() as usize;
        let held = message.id.lock().unwrap();
        let contended = thread::scope(|scope| {
            scope
                .spawn(|| unsafe { proxy.notify(raw as *mut IMessage) })
                .join()
                .unwrap()
        });
        drop(held);
        assert_eq!(contended, kResultFalse);
        message.attributes.put(
            "blob".into(),
            AttrValue::Bin(vec![1; MESSAGE_PACKET_BYTES + 1]),
        );
        let oversized = thread::scope(|scope| {
            scope
                .spawn(|| unsafe { proxy.notify(raw as *mut IMessage) })
                .join()
                .unwrap()
        });
        assert_eq!(oversized, kResultFalse);
        message
            .attributes
            .put("blob".into(), AttrValue::Bin(vec![1]));
        let results = thread::scope(|scope| {
            scope
                .spawn(|| {
                    (0..MESSAGE_PACKET_COUNT + 1)
                        .map(|_| unsafe { proxy.notify(raw as *mut IMessage) })
                        .collect::<Vec<_>>()
                })
                .join()
                .unwrap()
        });
        assert_eq!(
            &results[..MESSAGE_PACKET_COUNT],
            &[kResultOk; MESSAGE_PACKET_COUNT]
        );
        assert_eq!(results[MESSAGE_PACKET_COUNT], kResultFalse);
        assert_eq!(proxy.delivery.counters.contended.load(Ordering::Relaxed), 1);
        assert_eq!(proxy.delivery.counters.oversized.load(Ordering::Relaxed), 1);
        assert_eq!(proxy.delivery.counters.full.load(Ordering::Relaxed), 1);
    }

    struct ForeignMessage;
    impl Class for ForeignMessage {
        type Interfaces = (IMessage,);
    }
    impl IMessageTrait for ForeignMessage {
        unsafe fn getMessageID(&self) -> FIDString {
            c"foreign".as_ptr()
        }
        unsafe fn setMessageID(&self, _: FIDString) {}
        unsafe fn getAttributes(&self) -> *mut IAttributeList {
            ptr::null_mut()
        }
    }

    #[test]
    fn unsupported_null_and_closed_messages_are_not_delivered() {
        let (proxy, destination) = proxy_with_destination();
        let foreign = ComWrapper::new(ForeignMessage)
            .to_com_ptr::<IMessage>()
            .unwrap();
        let raw = foreign.as_ptr() as usize;
        assert_eq!(unsafe { proxy.notify(ptr::null_mut()) }, kResultFalse);
        let result = thread::scope(|scope| {
            scope
                .spawn(|| unsafe { proxy.notify(raw as *mut IMessage) })
                .join()
                .unwrap()
        });
        assert_eq!(result, kResultFalse);
        let message = create_host_message();
        *message.id.lock().unwrap() = Some(std::ffi::CString::new("queued").unwrap());
        let pointer = message.to_com_ptr::<IMessage>().unwrap();
        let raw = pointer.as_ptr() as usize;
        assert_eq!(
            thread::scope(|scope| scope
                .spawn(|| unsafe { proxy.notify(raw as *mut IMessage) })
                .join()
                .unwrap()),
            kResultOk
        );
        let weak = Arc::downgrade(&proxy.delivery);
        proxy.clear();
        proxy.delivery.pump();
        assert_eq!(destination.notifications.load(Ordering::Relaxed), 0);
        assert_eq!(
            proxy
                .delivery
                .counters
                .discarded_on_close
                .load(Ordering::Relaxed),
            1
        );
        drop(proxy);
        assert!(weak.upgrade().is_none());
    }
}

#[cfg(test)]
mod host_event_list_tests {
    use super::*;

    /// `process()` is the input list's only drain and it returns early while the plugin isn't
    /// processing, so queueing MIDI at a stopped plugin must not grow the list forever.
    #[test]
    fn queued_events_are_capped_so_a_stopped_plugin_cannot_grow_them() {
        let list = HostEventList::new();
        let event: Event = unsafe { std::mem::zeroed() };

        for _ in 0..(MAX_QUEUED_EVENTS + 500) {
            list.add_raw_event(&event);
        }
        assert_eq!(unsafe { list.getEventCount() }, MAX_QUEUED_EVENTS as i32);

        // The plugin-facing COM path is capped too, and reports the refusal.
        let mut event = event;
        assert_eq!(
            unsafe { list.addEvent(&mut event as *mut Event) },
            kResultFalse
        );

        // Clearing (as each block does) makes room again without dropping capacity.
        list.clear();
        assert_eq!(unsafe { list.getEventCount() }, 0);
        assert!(list.events.lock().unwrap().capacity() >= MAX_QUEUED_EVENTS);
        list.add_raw_event(&event);
        assert_eq!(unsafe { list.getEventCount() }, 1);
    }

    #[test]
    fn sysex_is_deep_copied_before_plugin_memory_expires() {
        let list = HostEventList::new();
        let source = vec![0xf0, 0x7d, 1, 2, 3, 0xf7];
        let source_ptr = source.as_ptr();
        let mut raw: Event = unsafe { std::mem::zeroed() };
        raw.r#type = Event_::EventTypes_::kDataEvent as u16;
        raw.__field0.data = DataEvent {
            size: source.len() as u32,
            r#type: DataEvent_::DataTypes_::kMidiSysEx,
            bytes: source_ptr,
        };

        assert_eq!(unsafe { list.addEvent(&mut raw) }, kResultOk);
        drop(source);

        let stored = list.events.lock().unwrap();
        let PluginEventData::Data { bytes, .. } = &stored[0].data else {
            panic!("expected data event");
        };
        assert_eq!(bytes, &[0xf0, 0x7d, 1, 2, 3, 0xf7]);
        assert_ne!(bytes.as_ptr(), source_ptr);
        drop(stored);

        let mut returned: Event = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { list.getEvent(0, &mut returned) }, kResultOk);
        let returned_data = unsafe { returned.__field0.data };
        assert_eq!(
            unsafe { std::slice::from_raw_parts(returned_data.bytes, returned_data.size as usize) },
            &[0xf0, 0x7d, 1, 2, 3, 0xf7]
        );
    }

    #[test]
    fn malformed_or_oversized_pointer_payloads_are_rejected() {
        let list = HostEventList::new();
        let mut raw: Event = unsafe { std::mem::zeroed() };
        raw.r#type = Event_::EventTypes_::kDataEvent as u16;
        raw.__field0.data = DataEvent {
            size: 1,
            r#type: DataEvent_::DataTypes_::kMidiSysEx,
            bytes: ptr::null(),
        };
        assert_eq!(unsafe { list.addEvent(&mut raw) }, kResultFalse);

        raw.__field0.data.size = (MAX_EVENT_PAYLOAD_BYTES + 1) as u32;
        raw.__field0.data.bytes = std::ptr::dangling();
        assert_eq!(unsafe { list.addEvent(&mut raw) }, kResultFalse);
        assert_eq!(unsafe { list.getEventCount() }, 0);
    }
}

#[cfg(test)]
mod plug_frame_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};

    struct FakePlugView {
        on_size_calls: Arc<AtomicUsize>,
        on_size_result: Arc<AtomicI32>,
        observed_unlocked_slot: Arc<AtomicBool>,
        slot: Arc<Mutex<Option<(i32, i32)>>>,
        reenter_once: AtomicBool,
        frame: AtomicPtr<IPlugFrame>,
        view: Arc<AtomicPtr<IPlugView>>,
    }

    impl Class for FakePlugView {
        type Interfaces = (IPlugView,);
    }

    impl IPlugViewTrait for FakePlugView {
        unsafe fn isPlatformTypeSupported(&self, _type: FIDString) -> tresult {
            kResultOk
        }

        unsafe fn attached(&self, _parent: *mut std::ffi::c_void, _type: FIDString) -> tresult {
            kResultOk
        }

        unsafe fn removed(&self) -> tresult {
            kResultOk
        }

        unsafe fn onWheel(&self, _distance: f32) -> tresult {
            kResultOk
        }

        unsafe fn onKeyDown(&self, _key: char16, _key_code: int16, _modifiers: int16) -> tresult {
            kResultOk
        }

        unsafe fn onKeyUp(&self, _key: char16, _key_code: int16, _modifiers: int16) -> tresult {
            kResultOk
        }

        unsafe fn getSize(&self, _size: *mut ViewRect) -> tresult {
            kResultOk
        }

        unsafe fn onSize(&self, _new_size: *mut ViewRect) -> tresult {
            self.on_size_calls.fetch_add(1, Ordering::SeqCst);
            self.observed_unlocked_slot
                .store(self.slot.try_lock().is_ok(), Ordering::SeqCst);

            if self.reenter_once.swap(false, Ordering::SeqCst) {
                let frame = ComRef::<IPlugFrame>::from_raw(self.frame.load(Ordering::SeqCst))
                    .expect("frame pointer");
                let mut nested = ViewRect {
                    left: 0,
                    top: 0,
                    right: 321,
                    bottom: 123,
                };
                assert_eq!(
                    frame.resizeView(self.view.load(Ordering::SeqCst), &mut nested),
                    kResultOk
                );
            }
            self.on_size_result.load(Ordering::SeqCst)
        }

        unsafe fn onFocus(&self, _state: TBool) -> tresult {
            kResultOk
        }

        unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
            kResultOk
        }

        unsafe fn canResize(&self) -> tresult {
            kResultTrue
        }

        unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
            kResultOk
        }
    }

    #[cfg(target_os = "linux")]
    fn make_frame(slot: Arc<Mutex<Option<(i32, i32)>>>) -> HostPlugFrame {
        HostPlugFrame::new(slot, Arc::new(Mutex::new(RunLoopRegistry::new())))
    }

    #[cfg(not(target_os = "linux"))]
    fn make_frame(slot: Arc<Mutex<Option<(i32, i32)>>>) -> HostPlugFrame {
        HostPlugFrame::new(slot)
    }

    #[test]
    fn records_requested_size_and_calls_on_size_synchronously() {
        let slot = Arc::new(Mutex::new(None));
        let frame = make_frame(slot.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let unlocked = Arc::new(AtomicBool::new(false));
        let view = ComWrapper::new(FakePlugView {
            on_size_calls: calls.clone(),
            on_size_result: Arc::new(AtomicI32::new(kResultOk)),
            observed_unlocked_slot: unlocked.clone(),
            slot: slot.clone(),
            reenter_once: AtomicBool::new(false),
            frame: AtomicPtr::new(std::ptr::null_mut()),
            view: Arc::new(AtomicPtr::new(std::ptr::null_mut())),
        });
        let view = view.to_com_ptr::<IPlugView>().unwrap();
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 640,
            bottom: 480,
        };
        let r = unsafe { frame.resizeView(view.as_ptr(), &mut rect) };
        assert_eq!(r, kResultOk);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(unlocked.load(Ordering::SeqCst));
        assert_eq!(*slot.lock().unwrap(), Some((640, 480)));
    }

    #[test]
    fn rejects_nulls_and_invalid_dimensions_without_recording() {
        let slot = Arc::new(Mutex::new(None));
        let frame = make_frame(slot.clone());
        let mut rect = ViewRect {
            left: 5,
            top: 5,
            right: 5,
            bottom: 10,
        };
        assert_eq!(
            unsafe { frame.resizeView(std::ptr::null_mut(), &mut rect) },
            kInvalidArgument
        );
        assert_eq!(
            unsafe { frame.resizeView(std::ptr::null_mut(), std::ptr::null_mut()) },
            kInvalidArgument
        );
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[test]
    fn propagates_on_size_failure_and_allows_reentrant_resize() {
        let slot = Arc::new(Mutex::new(None));
        let frame = make_frame(slot.clone());
        let frame_wrapper = {
            #[cfg(target_os = "linux")]
            {
                ComWrapper::new(HostPlugFrame::new(
                    slot.clone(),
                    Arc::new(Mutex::new(RunLoopRegistry::new())),
                ))
            }
            #[cfg(not(target_os = "linux"))]
            {
                ComWrapper::new(HostPlugFrame::new(slot.clone()))
            }
        };
        let frame_ptr = frame_wrapper.to_com_ptr::<IPlugFrame>().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let on_size_result = Arc::new(AtomicI32::new(kResultOk));
        let view_ptr = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
        let view_wrapper = ComWrapper::new(FakePlugView {
            on_size_calls: calls.clone(),
            on_size_result: on_size_result.clone(),
            observed_unlocked_slot: Arc::new(AtomicBool::new(false)),
            slot: slot.clone(),
            reenter_once: AtomicBool::new(true),
            frame: AtomicPtr::new(frame_ptr.as_ptr()),
            view: view_ptr.clone(),
        });
        let view = view_wrapper.to_com_ptr::<IPlugView>().unwrap();
        view_ptr.store(view.as_ptr(), Ordering::SeqCst);

        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 640,
            bottom: 480,
        };
        assert_eq!(
            unsafe { frame_ptr.resizeView(view.as_ptr(), &mut rect) },
            kResultOk
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(*slot.lock().unwrap(), Some((321, 123)));

        on_size_result.store(kResultFalse, Ordering::SeqCst);
        assert_eq!(
            unsafe { frame.resizeView(view.as_ptr(), &mut rect) },
            kResultFalse
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn run_loop_rejects_null_registrations_and_starts_empty() {
        use vst3::Steinberg::Linux::IRunLoopTrait;
        let reg = Arc::new(Mutex::new(RunLoopRegistry::new()));
        let frame = HostPlugFrame::new(Arc::new(Mutex::new(None)), reg.clone());
        // A registry with no editor attached is empty.
        assert!(reg.lock().unwrap().handlers.is_empty());
        assert!(reg.lock().unwrap().timers.is_empty());
        // Null handlers are rejected without touching the registry.
        unsafe {
            assert_eq!(
                frame.registerEventHandler(std::ptr::null_mut(), 3),
                kInvalidArgument
            );
            assert_eq!(
                frame.registerTimer(std::ptr::null_mut(), 16),
                kInvalidArgument
            );
        }
        assert!(reg.lock().unwrap().handlers.is_empty());
        assert!(reg.lock().unwrap().timers.is_empty());
    }
}

#[cfg(test)]
mod memory_stream_tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips_from_start() {
        let s = MemoryStream::new(Vec::new());
        assert_eq!(s.write_at_cursor(&[1, 2, 3, 4]), Some(4));
        assert_eq!(s.position(), 4);
        assert_eq!(s.to_vec(), vec![1, 2, 3, 4]);

        // Rewind (mode 0 = SEEK_SET) and read it all back.
        assert_eq!(s.seek_to(0, 0), Some(0));
        assert_eq!(s.read_at_cursor(4), vec![1, 2, 3, 4]);
    }

    #[test]
    fn read_past_end_is_clamped() {
        let s = MemoryStream::new(vec![9, 8]);
        assert_eq!(s.read_at_cursor(10), vec![9, 8]);
        // Cursor now at end; further reads yield nothing.
        assert_eq!(s.read_at_cursor(10), Vec::<u8>::new());
    }

    #[test]
    fn seek_modes_and_overwrite() {
        let s = MemoryStream::new(vec![0, 0, 0, 0]);
        // SEEK_END then write appends.
        assert_eq!(s.seek_to(0, SEEK_END), Some(4));
        s.write_at_cursor(&[5]);
        assert_eq!(s.to_vec(), vec![0, 0, 0, 0, 5]);
        // SEEK_SET to 1 then overwrite in place.
        assert_eq!(s.seek_to(1, 0), Some(1));
        s.write_at_cursor(&[7, 7]);
        assert_eq!(s.to_vec(), vec![0, 7, 7, 0, 5]);
        // SEEK_CUR is relative, and a seek before the start clamps to it.
        assert_eq!(s.seek_to(-3, SEEK_CUR), Some(0));
        assert_eq!(s.seek_to(-100, SEEK_CUR), Some(0));
    }

    /// The cursor and the write length both come from the plugin, and the buffer grows to
    /// `cursor + length`. A wild seek followed by any write must not turn into a multi-gigabyte
    /// allocation — that panics on capacity overflow inside a vtable thunk, which aborts the
    /// process instead of unwinding.
    #[test]
    fn huge_seek_then_write_is_refused_instead_of_allocating() {
        let s = MemoryStream::new(Vec::new());

        // Past the cap: the seek itself is refused, so the cursor never gets there.
        assert_eq!(s.seek_to(i64::MAX / 2, 0), None);
        assert_eq!(s.position(), 0);
        assert_eq!(s.seek_to(MAX_STREAM_BYTES as i64 + 1, 0), None);
        assert_eq!(s.position(), 0);

        // At the cap the seek is allowed, but the write that would grow past it is not, and
        // the stream is left untouched.
        assert_eq!(
            s.seek_to(MAX_STREAM_BYTES as i64, 0),
            Some(MAX_STREAM_BYTES as i64)
        );
        assert_eq!(s.write_at_cursor(&[1]), None);
        assert!(s.to_vec().is_empty());
    }

    /// The same refusal through the COM vtable, which is where a real plugin arrives: a result
    /// code and a zero byte count, never a panic.
    #[test]
    fn com_write_past_the_cap_reports_an_error_code() {
        let s = MemoryStream::new(Vec::new());
        let mut byte = 0u8;
        let mut written: i32 = -1;

        unsafe {
            // A seek beyond the cap is rejected outright...
            let mut pos: i64 = -1;
            assert_eq!(
                s.seek(MAX_STREAM_BYTES as i64 * 4, 0, &mut pos),
                kInvalidArgument
            );

            // ...and a write from a legal cursor that would cross it fails cleanly.
            assert_eq!(s.seek(MAX_STREAM_BYTES as i64, 0, &mut pos), kResultOk);
            assert_eq!(pos, MAX_STREAM_BYTES as i64);
            let result = s.write(
                &mut byte as *mut u8 as *mut std::ffi::c_void,
                1,
                &mut written,
            );
            assert_eq!(result, kOutOfMemory);
            assert_eq!(written, 0);
        }
        assert!(s.to_vec().is_empty());
    }

    /// Growth is bounded like every other host-side buffer: a plugin that keeps writing hits
    /// the cap and is told so, rather than growing the host's memory without limit.
    #[test]
    fn total_size_is_capped() {
        let s = MemoryStream::new(Vec::new());
        // Land one byte short of the cap, then write two.
        assert_eq!(
            s.seek_to(MAX_STREAM_BYTES as i64 - 1, 0),
            Some(MAX_STREAM_BYTES as i64 - 1)
        );
        assert_eq!(s.write_at_cursor(&[1]), Some(1));
        assert_eq!(s.to_vec().len(), MAX_STREAM_BYTES);
        assert_eq!(s.write_at_cursor(&[2]), None);
        assert_eq!(s.to_vec().len(), MAX_STREAM_BYTES);
    }

    /// Read a UTF-16 attribute back the way a plugin would, or `None` when the stream does
    /// not carry it.
    fn read_attribute(stream: &MemoryStream, key: &CStr) -> Option<String> {
        unsafe {
            let attributes = ComRef::<IAttributeList>::from_raw(stream.getAttributes())
                .expect("MemoryStream must vend its owned attribute list");
            let mut buf = [0u16; 512];
            if attributes.getString(
                key.as_ptr(),
                buf.as_mut_ptr(),
                std::mem::size_of_val(&buf) as u32,
            ) != kResultOk
            {
                return None;
            }
            let end = buf.iter().position(|unit| *unit == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        }
    }

    #[test]
    fn stream_attributes_expose_bounded_filename_and_state_type() {
        let long_name = "x".repeat(300);
        let stream = MemoryStream::with_metadata(
            vec![1, 2, 3],
            StreamMetadata {
                file_name: Some(&long_name),
                ..StreamMetadata::new(StreamStateType::Project)
            },
        );
        unsafe {
            let mut name: String128 = [0; 128];
            assert_eq!(stream.getFileName(&mut name), kResultOk);
            assert_eq!(name[..MAX_STREAM_FILENAME_UNITS], [u16::from(b'x'); 127]);
            assert_eq!(name[MAX_STREAM_FILENAME_UNITS], 0);
        }
        assert_eq!(
            read_attribute(&stream, c"StateType").as_deref(),
            Some("Project")
        );
        // No source file was named, so the path attribute must be absent rather than empty.
        assert_eq!(read_attribute(&stream, c"FilePathString"), None);
    }

    /// A preset restore tells the plugin both that the bytes came from a preset and which
    /// file they came from; a project restore tells it neither.
    #[test]
    fn a_restore_stream_publishes_the_context_it_was_built_from() {
        let preset = create_state_restore_stream(
            vec![7],
            &StateContext::preset_from_path("/Users/me/Presets/Big Lead.vstpreset"),
        );
        assert_eq!(
            read_attribute(&preset, c"StateType").as_deref(),
            Some("TrackPreset")
        );
        assert_eq!(
            read_attribute(&preset, c"FilePathString").as_deref(),
            Some("/Users/me/Presets/Big Lead.vstpreset")
        );
        unsafe {
            let mut name: String128 = [0; 128];
            assert_eq!(preset.getFileName(&mut name), kResultOk);
            let end = name
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(name.len());
            assert_eq!(String::from_utf16_lossy(&name[..end]), "Big Lead");
        }

        let pathless = create_state_restore_stream(vec![7], &StateContext::preset());
        assert_eq!(
            read_attribute(&pathless, c"StateType").as_deref(),
            Some("TrackPreset")
        );
        assert_eq!(read_attribute(&pathless, c"FilePathString"), None);

        let project = create_state_restore_stream(vec![7], &StateContext::Project);
        assert_eq!(
            read_attribute(&project, c"StateType").as_deref(),
            Some("Project")
        );
        assert_eq!(read_attribute(&project, c"FilePathString"), None);
    }
}

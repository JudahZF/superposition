//! macOS POSIX shared-memory mappings for a fixed [`SharedBank`] layout.
//!
//! This crate maps initialized `SharedBank` bytes. Fixed-bank heap initialization has its
//! own narrow unsafe boundary in `sp-shared-memory`; protocol state changes remain safe APIs.

mod audio_thread;

pub use audio_thread::AudioThreadPolicy;

use std::{
    ffi::CString,
    io,
    mem::{MaybeUninit, size_of},
    os::raw::{c_char, c_int, c_uint, c_void},
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

pub use sp_shared_memory::{
    BANK_COUNT, BankMetadata, PARAMETER_FEEDBACK_CAPACITY_PER_SLOT, RestartCursor, RestartSnapshot,
    SharedBank,
};

const O_RDWR: c_int = 0x0002;
const O_CREAT: c_int = 0x0200;
const O_EXCL: c_int = 0x0800;
const SHARED_MEMORY_MODE: c_uint = 0o666;
const PROT_READ: c_int = 0x01;
const PROT_WRITE: c_int = 0x02;
const MAP_SHARED: c_int = 0x0001;
const MAP_FAILED: *mut c_void = (-1_isize) as *mut c_void;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn close(file_descriptor: c_int) -> c_int;
    fn ftruncate(file_descriptor: c_int, length: i64) -> c_int;
    fn getpagesize() -> c_int;
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        file_descriptor: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, length: usize) -> c_int;
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
    fn superposition_shm_open_create(name: *const c_char, flags: c_int, mode: c_uint) -> c_int;
    fn superposition_shm_length(file_descriptor: c_int, length: *mut i64) -> c_int;
    fn superposition_request_wake_supported() -> c_int;
    fn superposition_notify_request(address: *mut c_void) -> c_int;
    fn superposition_wait_for_request(
        address: *mut c_void,
        observed: u32,
        timeout_ns: u64,
    ) -> c_int;
    fn shm_open(name: *const c_char, flags: c_int) -> c_int;
    fn shm_unlink(name: *const c_char) -> c_int;
}

static NEXT_REGION_ID: AtomicU64 = AtomicU64::new(1);

static RUN_NONCE: OnceLock<u32> = OnceLock::new();

/// Whether the running macOS supports process-shared address waits (14.4 or later).
#[must_use]
pub fn request_wake_supported() -> bool {
    // SAFETY: the shim has no arguments or side effects and checks OS availability.
    unsafe { superposition_request_wake_supported() != 0 }
}

const SHARED_MEMORY_CREATE_ATTEMPTS: usize = 64;

/// Conversion and sampling for Darwin's process-independent continuous clock.
///
/// Construct this value off the real-time thread. Calling [`Self::now_ticks`] performs
/// one system clock read and does not allocate or lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonotonicClock {
    numer: u32,
    denom: u32,
}

impl MonotonicClock {
    /// Loads the Darwin timebase used to convert shared raw ticks into durations.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error when Darwin cannot provide a valid timebase.
    pub fn new() -> io::Result<Self> {
        let mut info = MaybeUninit::<MachTimebaseInfo>::uninit();
        // SAFETY: Darwin writes one `MachTimebaseInfo` to the valid out pointer.
        let status = unsafe { mach_timebase_info(info.as_mut_ptr()) };
        if status != 0 {
            return Err(io::Error::other(format!(
                "mach_timebase_info failed with status {status}"
            )));
        }
        // SAFETY: a zero status documents that Darwin initialized the output record.
        let info = unsafe { info.assume_init() };
        if info.numer == 0 || info.denom == 0 {
            return Err(io::Error::other("Darwin returned an invalid timebase"));
        }
        Ok(Self {
            numer: info.numer,
            denom: info.denom,
        })
    }

    /// Captures the current raw tick in the system-wide continuous clock domain.
    #[must_use]
    pub fn now_ticks(self) -> u64 {
        // SAFETY: `mach_continuous_time` takes no arguments and has no preconditions.
        unsafe { mach_continuous_time() }
    }

    /// Converts raw continuous-clock ticks into a saturating duration.
    #[must_use]
    pub fn ticks_to_duration(self, ticks: u64) -> Duration {
        const NANOS_PER_SECOND: u128 = 1_000_000_000;

        let nanos =
            u128::from(ticks).saturating_mul(u128::from(self.numer)) / u128::from(self.denom);
        let seconds = nanos / NANOS_PER_SECOND;
        if seconds > u128::from(u64::MAX) {
            return Duration::MAX;
        }
        let subsecond_nanos = u32::try_from(nanos % NANOS_PER_SECOND).unwrap_or_default();
        let seconds = u64::try_from(seconds).unwrap_or(u64::MAX);
        Duration::new(seconds, subsecond_nanos)
    }

    /// Converts a duration into raw continuous-clock ticks, rounding up.
    #[must_use]
    pub fn duration_to_ticks(self, duration: Duration) -> u64 {
        let numerator = duration.as_nanos().saturating_mul(u128::from(self.denom));
        let ticks = numerator.div_ceil(u128::from(self.numer));
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }
}

/// A named mapping containing exactly one initialized [`SharedBank`].
///
/// The creator unlinks the POSIX name when this value is dropped. Existing mappings
/// remain valid until their owners close them, which lets the worker outlive the
/// launch handshake without leaking a named shared-memory object.
pub struct SharedMemoryRegion {
    name: CString,
    file_descriptor: c_int,
    bank: NonNull<SharedBank>,
    unlink_on_drop: bool,
}

// SAFETY: the mapping pointer is exclusively owned by this value. Moving the owner to
// another thread transfers exclusive Rust access; cross-process publication remains gated
// by the protocol atomics rather than Rust aliasing.
unsafe impl Send for SharedMemoryRegion {}

impl SharedMemoryRegion {
    /// Creates and initializes a uniquely named shared bank.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error when a name cannot be allocated, sized,
    /// or mapped.
    pub fn create(generation: u64) -> io::Result<Self> {
        if generation == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory generation must be nonzero",
            ));
        }
        let mut last_collision = None;
        for _ in 0..SHARED_MEMORY_CREATE_ATTEMPTS {
            let name = unique_name()?;
            // SAFETY: `name` is NUL-terminated, and all flags/mode bits are Darwin POSIX
            // constants. The returned descriptor is checked before further use.
            let file_descriptor = unsafe {
                superposition_shm_open_create(
                    name.as_ptr(),
                    O_RDWR | O_CREAT | O_EXCL,
                    SHARED_MEMORY_MODE,
                )
            };
            if file_descriptor >= 0 {
                return initialize_created_region(&name, file_descriptor, generation);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
            // This name belongs to an unknown prior owner. Never unlink it: advance the
            // per-run counter and retry another nonce-qualified O_EXCL name instead.
            last_collision = Some(error);
        }
        Err(last_collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique shared-memory name",
            )
        }))
    }

    /// Opens an initialized bank previously created by [`Self::create`].
    ///
    /// # Errors
    ///
    /// Returns an operating-system error if the named object cannot be opened or
    /// mapped, or `InvalidData` if its header or slot contents do not match this build.
    pub fn open(name: &str) -> io::Result<Self> {
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared-memory name contains NUL",
            )
        })?;
        // SAFETY: `name` is NUL-terminated and `O_RDWR` is a Darwin POSIX constant.
        let file_descriptor = unsafe { shm_open(name.as_ptr(), O_RDWR) };
        if file_descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let bank = match map_bank(file_descriptor) {
            Ok(bank) => bank,
            Err(error) => {
                // SAFETY: `file_descriptor` is valid and has not been closed.
                unsafe { close(file_descriptor) };
                return Err(error);
            }
        };
        // SAFETY: the creator publishes a fully initialized `SharedBank` before it
        // starts the worker. This immutable borrow only reads header scalars/atomics.
        if !unsafe { bank.as_ref().is_compatible() }
            || unsafe { bank.as_ref().validate_mapped_contents() }.is_err()
        {
            // SAFETY: this mapping and descriptor were acquired immediately above.
            unsafe {
                munmap(bank.as_ptr().cast(), size_of::<SharedBank>());
                close(file_descriptor);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shared-memory bank is not compatible with this build",
            ));
        }

        Ok(Self {
            name,
            file_descriptor,
            bank,
            unlink_on_drop: false,
        })
    }

    /// Returns the POSIX name a worker uses to open this mapping.
    ///
    /// # Panics
    ///
    /// Panics only if an internally stored name is not UTF-8. Constructors accept
    /// UTF-8 input and generated names are ASCII, so this indicates broken internal
    /// state rather than an external input error.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name
            .to_str()
            .expect("generated shared-memory name is UTF-8")
    }

    /// Returns the mapped address that remains stable until this region is dropped.
    ///
    /// This is an identity value for callback setup and diagnostics only; callers must use
    /// [`Self::bank`] or [`Self::bank_mut`] for all memory access.
    #[must_use]
    pub fn bank_address(&self) -> usize {
        self.bank.as_ptr().addr()
    }

    /// Returns this bank's nonzero worker generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.bank().generation()
    }

    /// Returns the wake sequence before scanning this bank for a request.
    #[must_use]
    pub fn request_wake_sequence(&self) -> u32 {
        self.bank()
            .header
            .request_wake_sequence
            .load(Ordering::Acquire)
    }

    /// Signals the worker after a request has been release-published in this bank.
    ///
    /// The caller must publish the slot first. A missing waiter is a successful signal;
    /// the sequence change prevents the next compare-and-wait from sleeping past it.
    /// This method performs one kernel wake call but does not allocate, wait, or format errors.
    #[must_use]
    pub fn notify_request(&self) -> bool {
        self.notify_request_status() >= 0
    }

    fn notify_request_status(&self) -> c_int {
        let sequence = &self.bank().header.request_wake_sequence;
        sequence.fetch_add(1, Ordering::Release);
        // SAFETY: `sequence` is a four-byte aligned atomic in this live MAP_SHARED bank.
        // The shim never stores through the pointer and makes one shared-address wake call.
        unsafe { superposition_notify_request(ptr::from_ref(sequence).cast_mut().cast()) }
    }

    /// Waits until the wake sequence differs from `observed`, a signal arrives, or time expires.
    ///
    /// Snapshot the sequence before scanning slots. Always rescan after this returns because
    /// timeout, interruption, and spurious wakeups are normal. Only the worker may wait here.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a zero timeout or an OS error for unsupported or failed waits.
    pub fn wait_for_request(&self, observed: u32, timeout: Duration) -> io::Result<()> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request-wake timeout must be nonzero",
            ));
        }
        let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        let sequence = &self.bank().header.request_wake_sequence;
        // SAFETY: `sequence` is a four-byte aligned atomic in this live MAP_SHARED bank.
        // The shim compares it without storing and waits for no longer than `timeout_ns`.
        let status = unsafe {
            superposition_wait_for_request(
                ptr::from_ref(sequence).cast_mut().cast(),
                observed,
                timeout_ns,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status))
        }
    }

    /// Reinitializes this mapping in place after the old worker has exited and been reaped.
    ///
    /// The POSIX mapping and its callback-visible address are preserved. This operation is
    /// intentionally explicit because it may discard abandoned or in-flight slots.
    ///
    /// # Safety
    ///
    /// Every worker and other process that could access this mapping must already have exited
    /// and been reaped, with all handles capable of reaching the mapping closed. Replacing the
    /// atomics while another process accesses them would violate the shared-memory protocol.
    ///
    /// # Errors
    ///
    /// Returns an error when `generation` is zero.
    pub unsafe fn reset_after_worker_exit(&mut self, generation: u64) -> io::Result<()> {
        let bank = SharedBank::new(generation).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid replacement shared-memory generation: {error}"),
            )
        })?;
        // SAFETY: the method's contract excludes all other mappings/readers. Both source and
        // destination have the exact SharedBank layout; the source is fully initialized and
        // contains no owned pointers or destructors. Copying avoids a bank-sized stack value.
        unsafe { ptr::copy_nonoverlapping(bank.as_ref(), self.bank.as_ptr(), 1) };
        Ok(())
    }

    /// Borrows the mapped bank for worker-side atomic state transitions.
    #[must_use]
    pub fn bank(&self) -> &SharedBank {
        // SAFETY: `bank` comes from a successful mapping and stays mapped for the
        // lifetime of `self`; callers receive only the safe protocol API.
        unsafe { self.bank.as_ref() }
    }

    /// Borrows the mapped bank for this mapping owner's plain-payload writes.
    ///
    /// The host uses this access to prepare input payloads, while the worker uses its
    /// distinct mapping to write output payloads after atomically claiming a slot.
    #[must_use]
    pub fn bank_mut(&mut self) -> &mut SharedBank {
        // SAFETY: this method requires exclusive access to this process's mapping.
        // Cross-process payload ownership is transferred by protocol state transitions.
        unsafe { self.bank.as_mut() }
    }
}

impl Drop for SharedMemoryRegion {
    fn drop(&mut self) {
        // SAFETY: this value owns the mapping and descriptor exactly once. POSIX
        // permits unlinking before other mapping owners close their descriptors.
        unsafe {
            munmap(self.bank.as_ptr().cast(), size_of::<SharedBank>());
            close(self.file_descriptor);
            if self.unlink_on_drop {
                shm_unlink(self.name.as_ptr());
            }
        }
    }
}

/// Resets a mapping after its worker has been synchronously stopped and reaped.
///
/// Keep this operation in the platform ownership boundary so safe application crates do not need
/// to manipulate mapped atomics directly.
///
/// # Errors
///
/// Returns an error when `generation` is zero.
pub fn reset_reaped_region(region: &mut SharedMemoryRegion, generation: u64) -> io::Result<()> {
    // SAFETY: this boundary is called only after the owning worker session synchronously stops and
    // reaps its process; no other process can retain access to the mapping.
    unsafe { region.reset_after_worker_exit(generation) }
}

/// Lifecycle state of one stable mapped bank in a rack pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappedBankLifecycle {
    /// The callback may publish requests to this bank.
    Active,
    /// This mapping is quiescent and may be prepared for a replacement worker.
    Inactive,
    /// A replacement worker may use this quiescent mapping but the callback cannot yet dispatch.
    Prepared,
    /// A newer bank is active; retain this mapping until its old worker is reaped.
    Retiring,
}

/// Lock-free control/callback handshake for one rack's live worker replacement.
pub struct RackRecoverySignal {
    state: AtomicU32,
    bank_index: AtomicU32,
    generation: AtomicU64,
}

/// Coherent phase observed from [`RackRecoverySignal`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RackRecoveryState {
    /// Normal callback dispatch.
    Idle,
    /// The control plane requests that the callback stop touching this rack's active bank.
    QuiesceRequested,
    /// The callback has closed the rack gate and released its live request.
    Quiescent,
    /// A ready replacement worker owns the named inactive bank.
    ReplacementReady {
        /// Stable pair index of the replacement bank.
        bank_index: usize,
        /// Replacement worker/bank generation.
        generation: u64,
    },
    /// The callback switched to the replacement; the old pair index may now be reset.
    ReplacementActive {
        /// Stable pair index retired by the switch.
        retiring_bank_index: usize,
    },
    /// The control plane reset the reaped worker's retired mapping for future use.
    RetiredReset {
        /// Stable pair index reset to inactive.
        bank_index: usize,
        /// Fresh inactive generation now stored in the mapping header.
        generation: u64,
    },
}

impl RackRecoverySignal {
    /// Creates an idle replacement handshake.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            bank_index: AtomicU32::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// Returns the currently published handshake phase.
    #[must_use]
    pub fn state(&self) -> RackRecoveryState {
        match self.state.load(Ordering::Acquire) {
            0 => RackRecoveryState::Idle,
            1 => RackRecoveryState::QuiesceRequested,
            2 => RackRecoveryState::Quiescent,
            3 => RackRecoveryState::ReplacementReady {
                bank_index: self.bank_index.load(Ordering::Relaxed) as usize,
                generation: self.generation.load(Ordering::Relaxed),
            },
            4 => RackRecoveryState::ReplacementActive {
                retiring_bank_index: self.bank_index.load(Ordering::Relaxed) as usize,
            },
            5 => RackRecoveryState::RetiredReset {
                bank_index: self.bank_index.load(Ordering::Relaxed) as usize,
                generation: self.generation.load(Ordering::Relaxed),
            },
            _ => unreachable!("rack recovery state is private and range checked"),
        }
    }

    /// Starts recovery only when no earlier replacement remains in progress.
    pub fn request_quiesce(&self) -> bool {
        self.state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Callback acknowledgement that the failed rack no longer touches either mapping.
    pub fn mark_quiescent(&self) -> bool {
        self.state
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Publishes a fully initialized replacement worker and inactive bank.
    pub fn publish_replacement(&self, bank_index: usize, generation: u64) -> bool {
        let Ok(bank_index) = u32::try_from(bank_index) else {
            return false;
        };
        if generation == 0 || self.state.load(Ordering::Acquire) != 2 {
            return false;
        }
        self.bank_index.store(bank_index, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Relaxed);
        self.state.store(3, Ordering::Release);
        true
    }

    /// Callback acknowledgement of the block-boundary bank switch.
    pub fn mark_replacement_active(&self, retiring_bank_index: usize) -> bool {
        let Ok(retiring_bank_index) = u32::try_from(retiring_bank_index) else {
            return false;
        };
        if self.state.load(Ordering::Acquire) != 3 {
            return false;
        }
        self.bank_index
            .store(retiring_bank_index, Ordering::Relaxed);
        self.state.store(4, Ordering::Release);
        true
    }

    /// Publishes the reset retired bank after the old worker was reaped.
    pub fn publish_retired_reset(&self, bank_index: usize, generation: u64) -> bool {
        let Ok(bank_index) = u32::try_from(bank_index) else {
            return false;
        };
        if generation == 0 || self.state.load(Ordering::Acquire) != 4 {
            return false;
        }
        self.bank_index.store(bank_index, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Relaxed);
        self.state.store(5, Ordering::Release);
        true
    }

    /// Callback acknowledgement that both stable mappings are ready for another cycle.
    pub fn complete_retirement(&self) -> bool {
        self.state
            .compare_exchange(5, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Default for RackRecoverySignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Metadata for one callback-visible mapped bank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedBankMetadata {
    /// Fixed pair index and current generation in the mapped header.
    pub identity: BankMetadata,
    /// Stable mapped address used by the callback.
    pub address: usize,
    /// Whether the bank is active, inactive, prepared for a worker, or awaiting retirement.
    pub lifecycle: MappedBankLifecycle,
}

/// Two stable POSIX mappings retained for one rack across worker replacement.
///
/// Both mappings are created before the audio callback receives this owner. Switching only
/// publishes a new fixed index: it never moves a mapping or changes either callback-visible
/// address. A retiring bank is deliberately retained until the control plane confirms that its
/// worker exited, so abandoned and in-flight slots cannot be reused by a replacement.
pub struct MappedRackBanks {
    regions: [SharedMemoryRegion; BANK_COUNT],
    active_index: AtomicU32,
    lifecycle: [MappedBankLifecycle; BANK_COUNT],
}

impl MappedRackBanks {
    /// Creates active and inactive mappings with distinct consecutive generations.
    ///
    /// # Errors
    ///
    /// Returns an error when generation is zero, cannot reserve a nonzero replacement
    /// generation, or macOS cannot allocate either mapping.
    pub fn create(active_generation: u64) -> io::Result<Self> {
        let inactive_generation = active_generation.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "active generation cannot reserve an inactive mapping",
            )
        })?;
        let active = SharedMemoryRegion::create(active_generation)?;
        let inactive = SharedMemoryRegion::create(inactive_generation)?;
        // SAFETY: both mappings were created above and no worker has received either name.
        unsafe { Self::from_regions(active, inactive) }
    }

    /// Adds a stable inactive mapping beside a control-plane-created active mapping.
    ///
    /// This is a compatibility bridge for callers that already created the worker's initial
    /// bank. The additional mapping is still allocated before callback start.
    ///
    /// # Errors
    ///
    /// Returns an error when the active generation cannot reserve a distinct nonzero backup
    /// generation, or macOS cannot create the backup mapping.
    pub fn with_active_region(active: SharedMemoryRegion) -> io::Result<Self> {
        let inactive_generation = active.generation().checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "active generation cannot reserve an inactive mapping",
            )
        })?;
        let inactive = SharedMemoryRegion::create(inactive_generation)?;
        // SAFETY: the new inactive mapping has never been shared with a worker. The active
        // mapping is only selected, never reinitialized by this constructor.
        unsafe { Self::from_regions(active, inactive) }
    }

    /// Combines two already-created compatible mappings into an active/inactive pair.
    ///
    /// # Safety
    ///
    /// No worker may map or access `inactive`. The pair treats that mapping as resettable only
    /// while its lifecycle is `Inactive`; passing a live worker mapping as inactive could race
    /// its atomic protocol state during preparation.
    ///
    /// # Errors
    ///
    /// Returns `InvalidData` when either mapping is malformed, both have the same generation,
    /// or they unexpectedly resolve to the same mapped address.
    pub unsafe fn from_regions(
        active: SharedMemoryRegion,
        inactive: SharedMemoryRegion,
    ) -> io::Result<Self> {
        // SAFETY: the caller guarantees the second region is not accessed by a worker.
        unsafe { Self::from_indexed_regions([active, inactive], 0) }
    }

    /// Reconstitutes a pair in fixed bank-index order with either bank selected as active.
    ///
    /// # Safety
    ///
    /// No worker may map or access `regions[active_index ^ 1]`. The caller must preserve the
    /// control plane's original bank-index order; this constructor never swaps the mappings.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for an index other than zero or one, or `InvalidData` for
    /// incompatible, duplicate-generation, or duplicate-address mappings.
    pub unsafe fn from_indexed_regions(
        regions: [SharedMemoryRegion; BANK_COUNT],
        active_index: usize,
    ) -> io::Result<Self> {
        if active_index >= BANK_COUNT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mapped rack active bank index is outside the fixed pair",
            ));
        }
        if !regions[0].bank().is_compatible()
            || !regions[1].bank().is_compatible()
            || regions[0].generation() == regions[1].generation()
            || regions[0].bank_address() == regions[1].bank_address()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mapped rack banks must be distinct compatible generations",
            ));
        }
        let mut lifecycle = [MappedBankLifecycle::Inactive; BANK_COUNT];
        lifecycle[active_index] = MappedBankLifecycle::Active;
        Ok(Self {
            regions,
            active_index: AtomicU32::new(u32::try_from(active_index).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid active bank index")
            })?),
            lifecycle,
        })
    }

    /// Returns the callback-selected fixed bank index.
    #[must_use]
    pub fn active_index(&self) -> usize {
        match self.active_index.load(Ordering::Acquire) {
            0 => 0,
            1 => 1,
            _ => unreachable!("mapped rack bank selector is private and range checked"),
        }
    }

    /// Returns one stable mapped bank by its fixed index.
    #[must_use]
    pub fn bank(&self, bank_index: usize) -> Option<&SharedMemoryRegion> {
        self.regions.get(bank_index)
    }

    /// Returns one stable mapped bank mutably for pre-callback payload preparation.
    #[must_use]
    pub fn bank_mut(&mut self, bank_index: usize) -> Option<&mut SharedMemoryRegion> {
        self.regions.get_mut(bank_index)
    }

    /// Returns the callback-selected stable mapping.
    #[must_use]
    pub fn active_bank(&self) -> &SharedMemoryRegion {
        &self.regions[self.active_index()]
    }

    /// Returns the callback-selected stable mapping mutably.
    #[must_use]
    pub fn active_bank_mut(&mut self) -> &mut SharedMemoryRegion {
        let active_index = self.active_index();
        &mut self.regions[active_index]
    }

    /// Returns active and inactive generation/address/lifecycle metadata.
    #[must_use]
    pub fn metadata(&self) -> [MappedBankMetadata; BANK_COUNT] {
        [self.bank_metadata(0), self.bank_metadata(1)]
    }

    /// Returns metadata for one stable mapped bank.
    #[must_use]
    pub fn bank_metadata(&self, bank_index: usize) -> MappedBankMetadata {
        let region = &self.regions[bank_index];
        MappedBankMetadata {
            identity: BankMetadata {
                index: bank_index,
                generation: region.generation(),
            },
            address: region.bank_address(),
            lifecycle: self.lifecycle[bank_index],
        }
    }

    /// Returns the non-active bank's metadata and POSIX name for replacement-worker launch.
    ///
    /// This metadata is only a snapshot. The replacement must still be activated at a block
    /// boundary after the worker reports ready.
    #[must_use]
    pub fn inactive_bank_metadata(&self) -> MappedBankMetadata {
        self.bank_metadata(self.inactive_index())
    }

    /// Returns the inactive mapping name used by a replacement worker.
    #[must_use]
    pub fn inactive_bank_name(&self) -> &str {
        self.regions[self.inactive_index()].name()
    }

    /// Resets the quiescent inactive mapping for a replacement generation.
    ///
    /// The inactive bank must contain only `Free` slots. This explicitly rejects abandoned,
    /// in-flight, and unconsumed completion slots instead of reusing them.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-inactive lifecycle, duplicate or zero generation, or a
    /// non-quiescent mapping.
    pub fn prepare_inactive(&mut self, generation: u64) -> io::Result<MappedBankMetadata> {
        let inactive_index = self.inactive_index();
        if self.lifecycle[inactive_index] != MappedBankLifecycle::Inactive {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replacement bank is not inactive",
            ));
        }
        if generation == 0 || generation == self.active_bank().generation() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replacement generation must be nonzero and distinct from active",
            ));
        }
        if !self.regions[inactive_index].bank().is_quiescent() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "inactive bank contains an abandoned or in-flight slot",
            ));
        }
        // SAFETY: an `Inactive` mapping comes from creation or `retire_after_worker_exit`.
        // Neither path permits a live worker to retain it before preparation.
        unsafe {
            self.regions[inactive_index].reset_after_worker_exit(generation)?;
        }
        self.lifecycle[inactive_index] = MappedBankLifecycle::Prepared;
        Ok(self.bank_metadata(inactive_index))
    }

    /// Marks an externally reset inactive mapping prepared after validating its header.
    ///
    /// # Errors
    ///
    /// Returns an error unless the selected mapping is the quiescent inactive bank with the
    /// supplied nonzero generation.
    pub fn acknowledge_external_prepare(
        &mut self,
        bank_index: usize,
        generation: u64,
    ) -> io::Result<MappedBankMetadata> {
        if bank_index != self.inactive_index()
            || self.lifecycle.get(bank_index) != Some(&MappedBankLifecycle::Inactive)
            || generation == 0
            || self.regions[bank_index].generation() != generation
            || !self.regions[bank_index].bank().is_quiescent()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external replacement bank is not the quiescent inactive mapping",
            ));
        }
        self.lifecycle[bank_index] = MappedBankLifecycle::Prepared;
        Ok(self.bank_metadata(bank_index))
    }

    /// Switches the callback to the prepared inactive mapping at a block boundary.
    ///
    /// The previous active bank remains mapped as `Retiring` and is never reused here.
    ///
    /// # Errors
    ///
    /// Returns an error unless the candidate is prepared, compatible, quiescent, and has a
    /// distinct nonzero generation. The returned metadata identifies the retiring bank.
    pub fn activate_prepared(&mut self) -> io::Result<MappedBankMetadata> {
        let active_index = self.active_index();
        let inactive_index = self.inactive_index();
        let candidate = &self.regions[inactive_index];
        if self.lifecycle[inactive_index] != MappedBankLifecycle::Prepared
            || !candidate.bank().is_compatible()
            || !candidate.bank().is_quiescent()
            || candidate.generation() == self.regions[active_index].generation()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replacement bank is not a distinct, quiescent compatible mapping",
            ));
        }
        let inactive_index_u32 = u32::try_from(inactive_index).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "replacement bank index exceeds the selector range",
            )
        })?;
        self.lifecycle[active_index] = MappedBankLifecycle::Retiring;
        self.lifecycle[inactive_index] = MappedBankLifecycle::Active;
        self.active_index
            .store(inactive_index_u32, Ordering::Release);
        Ok(self.bank_metadata(active_index))
    }

    /// Retires and reinitializes a replaced bank after its old worker has exited and reaped.
    ///
    /// # Safety
    ///
    /// The old worker must be joined or reaped and every handle that could retain its mapping
    /// must be closed. Only then is it safe to discard abandoned or in-flight state and make
    /// the stable address available as inactive again.
    ///
    /// # Errors
    ///
    /// Returns an error for the active bank, a non-retiring lifecycle, or a zero/duplicate
    /// next generation.
    pub unsafe fn retire_after_worker_exit(
        &mut self,
        bank_index: usize,
        next_generation: u64,
    ) -> io::Result<MappedBankMetadata> {
        if bank_index >= BANK_COUNT
            || bank_index == self.active_index()
            || self.lifecycle[bank_index] != MappedBankLifecycle::Retiring
            || next_generation == 0
            || next_generation == self.active_bank().generation()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bank is not a safely retired replacement candidate",
            ));
        }
        // SAFETY: upheld by this method's safety contract after lifecycle validation above.
        unsafe {
            self.regions[bank_index].reset_after_worker_exit(next_generation)?;
        }
        self.lifecycle[bank_index] = MappedBankLifecycle::Inactive;
        Ok(self.bank_metadata(bank_index))
    }

    /// Marks an externally reset retired mapping inactive after validating its fresh header.
    ///
    /// # Errors
    ///
    /// Returns an error unless the selected mapping is quiescent, retiring, non-active, and has
    /// the supplied nonzero generation.
    pub fn acknowledge_external_retirement(
        &mut self,
        bank_index: usize,
        generation: u64,
    ) -> io::Result<MappedBankMetadata> {
        if bank_index >= BANK_COUNT
            || bank_index == self.active_index()
            || self.lifecycle[bank_index] != MappedBankLifecycle::Retiring
            || generation == 0
            || self.regions[bank_index].generation() != generation
            || !self.regions[bank_index].bank().is_quiescent()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external retired bank reset is inconsistent with callback lifecycle",
            ));
        }
        self.lifecycle[bank_index] = MappedBankLifecycle::Inactive;
        Ok(self.bank_metadata(bank_index))
    }

    fn inactive_index(&self) -> usize {
        self.active_index() ^ 1
    }
}

fn initialize_created_region(
    name: &CString,
    file_descriptor: c_int,
    generation: u64,
) -> io::Result<SharedMemoryRegion> {
    let result = (|| {
        let backing_bytes = backing_bytes_i64()?;
        // SAFETY: `file_descriptor` is open, and `backing_bytes` is the checked positive,
        // page-aligned size of the single mapping's backing object.
        if unsafe { ftruncate(file_descriptor, backing_bytes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let initialized_bank = SharedBank::new(generation).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid shared-memory bank generation: {error}"),
            )
        })?;
        let bank = map_bank(file_descriptor)?;
        // SAFETY: this new mapping is exclusively owned until this function returns. The boxed
        // bank has every atomic and fixed-layout field initialized, and contains no owned
        // pointers or destructors. Copying avoids a bank-sized stack value.
        unsafe { ptr::copy_nonoverlapping(initialized_bank.as_ref(), bank.as_ptr(), 1) };
        Ok(SharedMemoryRegion {
            name: name.clone(),
            file_descriptor,
            bank,
            unlink_on_drop: true,
        })
    })();

    if result.is_err() {
        // SAFETY: this process successfully created both resources under O_EXCL, so this
        // cleanup cannot unlink an unknown prior owner's object.
        unsafe {
            close(file_descriptor);
            shm_unlink(name.as_ptr());
        }
    }
    result
}

fn unique_name() -> io::Result<CString> {
    let region_value = NEXT_REGION_ID.fetch_add(1, Ordering::Relaxed);
    let region_id = u32::try_from(region_value & u64::from(u32::MAX))
        .expect("masked region identifier fits u32");
    let run_nonce = *RUN_NONCE.get_or_init(|| {
        // SAFETY: `mach_continuous_time` takes no arguments and has no preconditions.
        let ticks = unsafe { mach_continuous_time() };
        let folded = ticks ^ (ticks >> 32);
        u32::try_from(folded & u64::from(u32::MAX)).expect("masked run nonce fits u32") | 1
    });
    generated_name(std::process::id(), run_nonce, region_id)
}

fn generated_name(process_id: u32, run_nonce: u32, region_id: u32) -> io::Result<CString> {
    // Keep the Darwin POSIX shared-memory name below its conservative 31-byte limit.
    CString::new(format!("/sp-{process_id:x}-{run_nonce:x}-{region_id:x}"))
        .map_err(|_| io::Error::other("generated invalid shared-memory name"))
}

fn bank_bytes_i64() -> io::Result<i64> {
    i64::try_from(size_of::<SharedBank>())
        .map_err(|_| io::Error::other("shared-memory bank exceeds POSIX length"))
}

fn backing_bytes_i64() -> io::Result<i64> {
    let bank_bytes = bank_bytes_i64()?;
    // SAFETY: `getpagesize` takes no arguments and returns the host VM page size.
    let page_bytes = i64::from(unsafe { getpagesize() });
    if page_bytes <= 0 {
        return Err(io::Error::other("Darwin returned an invalid VM page size"));
    }
    bank_bytes
        .checked_add(page_bytes - 1)
        .map(|bytes| (bytes / page_bytes) * page_bytes)
        .ok_or_else(|| io::Error::other("shared-memory backing length overflows POSIX length"))
}

fn shared_memory_length(file_descriptor: c_int) -> io::Result<i64> {
    let mut length = 0_i64;
    // SAFETY: `file_descriptor` is open, and `length` points to valid writable storage for
    // the C shim's `int64_t` result. The shim calls `fstat` and writes only on success.
    if unsafe { superposition_shm_length(file_descriptor, &raw mut length) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(length)
}

fn map_bank(file_descriptor: c_int) -> io::Result<NonNull<SharedBank>> {
    let expected_length = backing_bytes_i64()?;
    let actual_length = shared_memory_length(file_descriptor)?;
    if actual_length != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "shared-memory object has {actual_length} bytes; expected {expected_length} bytes"
            ),
        ));
    }

    // SAFETY: the descriptor is open and `fstat` established that it has exactly the
    // expected page-rounded backing length, which is at least `SharedBank` before this
    // fixed-prefix mapping can be dereferenced. POSIX returns either `MAP_FAILED` or a
    // page-aligned valid mapping.
    let address = unsafe {
        mmap(
            ptr::null_mut(),
            size_of::<SharedBank>(),
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            file_descriptor,
            0,
        )
    };
    if address == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let Some(bank) = NonNull::new(address.cast()) else {
        // SAFETY: `mmap` returned a non-failed mapping with the exact length above. This
        // crate cannot represent a null mapping with `NonNull`, so release it before failing.
        unsafe { munmap(address, size_of::<SharedBank>()) };
        return Err(io::Error::other("mmap returned null"));
    };
    Ok(bank)
}

#[cfg(test)]
mod tests {
    use super::{
        MappedBankLifecycle, MappedRackBanks, MonotonicClock, O_CREAT, O_EXCL, O_RDWR,
        RackRecoverySignal, RackRecoveryState, SHARED_MEMORY_MODE, SharedMemoryRegion, close,
        ftruncate, generated_name, request_wake_supported, shm_unlink,
        superposition_shm_open_create,
    };
    use sp_shared_memory::{BlockRequest, BlockTicket, SlotState};
    use std::io::{BufRead, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const REQUEST_WAKE_CHILD_BANK: &str = "SP_REQUEST_WAKE_TEST_BANK";

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn request_wake_handles_notification_before_wait_and_timeout() {
        if !request_wake_supported() {
            return;
        }
        let region = SharedMemoryRegion::create(1).unwrap();
        let observed = region.request_wake_sequence();
        assert!(region.notify_request());
        assert_eq!(region.request_wake_sequence(), observed.wrapping_add(1));
        region
            .wait_for_request(observed, Duration::from_millis(100))
            .unwrap();

        let current = region.request_wake_sequence();
        region
            .wait_for_request(current, Duration::from_millis(2))
            .unwrap();
        assert_eq!(region.request_wake_sequence(), current);
    }

    #[test]
    fn request_wake_reaches_a_distinct_mapping() {
        if !request_wake_supported() {
            return;
        }
        let creator = SharedMemoryRegion::create(1).unwrap();
        let opener = SharedMemoryRegion::open(creator.name()).unwrap();
        assert_ne!(creator.bank_address(), opener.bank_address());
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let observed = opener.request_wake_sequence();
            ready_tx.send(()).unwrap();
            let start = Instant::now();
            opener
                .wait_for_request(observed, Duration::from_secs(3))
                .unwrap();
            (opener.request_wake_sequence(), start.elapsed())
        });
        ready_rx.recv().unwrap();
        assert!(creator.notify_request());
        let (sequence, elapsed) = waiter.join().unwrap();
        assert_eq!(sequence, 1);
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn editor_feedback_survives_worker_mapping_exit_until_bank_reset() {
        let mut creator = SharedMemoryRegion::create(7).expect("host mapping");
        let worker = SharedMemoryRegion::open(creator.name()).expect("worker mapping");
        worker
            .bank()
            .feedback
            .reset_slot(2)
            .expect("instance epoch");
        worker
            .bank()
            .feedback
            .publish(2, u32::MAX, 0.625)
            .expect("editor value");
        drop(worker);

        let snapshot = creator
            .bank()
            .feedback
            .snapshot_changed(2, 0, 0)
            .expect("host retained worker value");
        assert_eq!(snapshot.values, vec![(u32::MAX, 0.625)]);

        // SAFETY: the worker mapping was dropped above and no other process maps this test bank.
        unsafe { creator.reset_after_worker_exit(8) }.expect("next generation");
        assert_eq!(creator.generation(), 8);
        assert!(creator.bank().feedback.snapshot_changed(2, 0, 0).is_none());
    }

    #[test]
    fn request_wake_child_waits_for_shared_region() {
        let Ok(name) = std::env::var(REQUEST_WAKE_CHILD_BANK) else {
            return;
        };
        let region = SharedMemoryRegion::open(&name).unwrap();
        let observed = region.request_wake_sequence();
        println!("SP_REQUEST_WAKE_READY");
        std::io::stdout().flush().unwrap();
        let start = Instant::now();
        region
            .wait_for_request(observed, Duration::from_secs(3))
            .unwrap();
        assert_ne!(region.request_wake_sequence(), observed);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn request_wake_reaches_a_waiter_in_another_process() {
        if !request_wake_supported() {
            return;
        }
        let creator = SharedMemoryRegion::create(1).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::request_wake_child_waits_for_shared_region",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(REQUEST_WAKE_CHILD_BANK, creator.name())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let stdout = child.0.stdout.take().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let output_reader = std::thread::spawn(move || {
            let mut reported_ready = false;
            for line in std::io::BufReader::new(stdout).lines() {
                if !reported_ready && line.is_ok_and(|line| line.contains("SP_REQUEST_WAKE_READY"))
                {
                    let _ = ready_tx.send(true);
                    reported_ready = true;
                }
            }
            if !reported_ready {
                let _ = ready_tx.send(false);
            }
        });
        assert_eq!(ready_rx.recv_timeout(Duration::from_secs(2)), Ok(true));

        // Give the child time to enter the kernel wait after flushing its readiness line.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            creator.notify_request_status(),
            1,
            "no cross-process waiter"
        );

        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                output_reader.join().unwrap();
                assert!(status.success(), "cross-process waiter failed: {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cross-process waiter did not exit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn continuous_clock_uses_a_shared_nonzero_tick_domain() {
        let clock = MonotonicClock::new().unwrap();
        let first = clock.now_ticks();
        let second = clock.now_ticks();

        assert!(first > 0);
        assert!(second >= first);
        assert!(clock.ticks_to_duration(second - first) <= Duration::from_secs(1));
        assert!(clock.duration_to_ticks(Duration::from_micros(1)) > 0);
    }

    #[test]
    fn nonce_qualified_names_are_short_and_change_across_runs_and_regions() {
        let first = generated_name(u32::MAX, 1, 1).unwrap();
        let next_region = generated_name(u32::MAX, 1, 2).unwrap();
        let next_run = generated_name(u32::MAX, 3, 1).unwrap();
        assert_ne!(first, next_region);
        assert_ne!(first, next_run);
        assert!(first.as_bytes().len() <= 30);
    }

    #[test]
    fn clock_conversions_round_up_and_saturate_without_sampling_the_system_clock() {
        let clock = MonotonicClock { numer: 3, denom: 2 };
        assert_eq!(clock.ticks_to_duration(2), Duration::from_nanos(3));
        assert_eq!(clock.duration_to_ticks(Duration::from_nanos(1)), 1);
        assert_eq!(clock.duration_to_ticks(Duration::from_nanos(3)), 2);

        let saturating_clock = MonotonicClock {
            numer: u32::MAX,
            denom: 1,
        };
        assert_eq!(saturating_clock.ticks_to_duration(u64::MAX), Duration::MAX);
    }

    #[test]
    fn creation_rejects_a_zero_generation() {
        let Err(error) = SharedMemoryRegion::create(0) else {
            panic!("zero generation must fail");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn opening_an_undersized_object_returns_invalid_data_before_mapping() {
        let name = generated_name(std::process::id(), 17, 23).unwrap();
        // SAFETY: the generated name is NUL terminated, all flags are POSIX constants, and
        // the descriptor is closed and unlinked below regardless of the assertion result.
        let file_descriptor = unsafe {
            superposition_shm_open_create(
                name.as_ptr(),
                O_RDWR | O_CREAT | O_EXCL,
                SHARED_MEMORY_MODE,
            )
        };
        assert!(
            file_descriptor >= 0,
            "create undersized shared-memory object"
        );

        // SAFETY: the open descriptor belongs to this test and the positive length makes
        // the object deliberately smaller than the `SharedBank` mapping length.
        assert_eq!(unsafe { ftruncate(file_descriptor, 1) }, 0);
        let Err(error) = SharedMemoryRegion::open(name.to_str().unwrap()) else {
            panic!("undersized shared-memory object must be rejected");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        // SAFETY: this test owns the descriptor and O_EXCL-created name exactly once.
        unsafe {
            close(file_descriptor);
            shm_unlink(name.as_ptr());
        }
    }

    #[test]
    fn a_second_mapping_observes_the_initialized_protocol_header() {
        let creator = SharedMemoryRegion::create(7).unwrap();
        let opener = SharedMemoryRegion::open(creator.name()).unwrap();

        assert!(creator.bank().is_compatible());
        assert!(opener.bank().is_compatible());
        assert_eq!(
            opener
                .bank()
                .header
                .generation
                .load(std::sync::atomic::Ordering::Acquire),
            7
        );
    }

    #[test]
    fn mapped_pair_switches_generation_without_changing_callback_addresses() {
        let mut banks = MappedRackBanks::create(7).unwrap();
        let initial = banks.metadata();
        assert_eq!(initial[0].lifecycle, MappedBankLifecycle::Active);
        assert_eq!(initial[1].lifecycle, MappedBankLifecycle::Inactive);
        assert_ne!(initial[0].address, initial[1].address);

        let prepared = banks.prepare_inactive(11).unwrap();
        assert_eq!(prepared.identity.index, 1);
        assert_eq!(prepared.identity.generation, 11);
        assert_eq!(prepared.address, initial[1].address);
        assert_eq!(prepared.lifecycle, MappedBankLifecycle::Prepared);
        let retiring = banks.activate_prepared().unwrap();
        assert_eq!(retiring.identity.index, 0);
        assert_eq!(retiring.lifecycle, MappedBankLifecycle::Retiring);
        assert_eq!(banks.active_index(), 1);
        assert_eq!(banks.active_bank().bank_address(), initial[1].address);
        assert_eq!(banks.active_bank().generation(), 11);

        // SAFETY: this test never launches a worker for the retired mapping.
        let retired = unsafe {
            banks
                .retire_after_worker_exit(retiring.identity.index, 13)
                .unwrap()
        };
        assert_eq!(retired.address, initial[0].address);
        assert_eq!(retired.identity.generation, 13);
        assert_eq!(retired.lifecycle, MappedBankLifecycle::Inactive);
    }

    #[test]
    fn indexed_pair_keeps_fixed_bank_order_when_second_bank_is_active() {
        let first = SharedMemoryRegion::create(7).expect("bank zero");
        let second = SharedMemoryRegion::create(11).expect("bank one");
        let addresses = [first.bank_address(), second.bank_address()];
        // SAFETY: neither test mapping has a worker, and fixed index zero is inactive.
        let banks = unsafe { MappedRackBanks::from_indexed_regions([first, second], 1) }
            .expect("indexed pair");
        assert_eq!(banks.active_index(), 1);
        assert_eq!(banks.active_bank().bank_address(), addresses[1]);
        assert_eq!(banks.metadata()[0].address, addresses[0]);
        assert_eq!(banks.metadata()[0].lifecycle, MappedBankLifecycle::Inactive);
        assert_eq!(banks.metadata()[1].lifecycle, MappedBankLifecycle::Active);
    }

    #[test]
    fn recovery_signal_completes_one_dual_bank_handoff() {
        let recovery = RackRecoverySignal::new();
        assert_eq!(recovery.state(), RackRecoveryState::Idle);
        assert!(recovery.request_quiesce());
        assert_eq!(recovery.state(), RackRecoveryState::QuiesceRequested);
        assert!(recovery.mark_quiescent());
        assert_eq!(recovery.state(), RackRecoveryState::Quiescent);
        assert!(recovery.publish_replacement(1, 11));
        assert_eq!(
            recovery.state(),
            RackRecoveryState::ReplacementReady {
                bank_index: 1,
                generation: 11,
            }
        );
        assert!(recovery.mark_replacement_active(0));
        assert_eq!(
            recovery.state(),
            RackRecoveryState::ReplacementActive {
                retiring_bank_index: 0,
            }
        );
        assert!(recovery.publish_retired_reset(0, 13));
        assert_eq!(
            recovery.state(),
            RackRecoveryState::RetiredReset {
                bank_index: 0,
                generation: 13,
            }
        );
        assert!(recovery.complete_retirement());
        assert_eq!(recovery.state(), RackRecoveryState::Idle);
    }

    #[test]
    fn abandoned_inactive_mapping_is_never_prepared_or_reused() {
        let mut banks = MappedRackBanks::create(7).unwrap();
        let address = banks.bank(1).unwrap().bank_address();
        let request = BlockRequest {
            frame_count: 128,
            input_channel_count: 2,
            output_channel_count: 2,
            midi_event_count: 0,
            event_count: 0,
            flags: 0,
            sidechain_slots: 0,
        };
        let ticket = banks
            .bank_mut(1)
            .unwrap()
            .bank_mut()
            .request_block(0, request)
            .unwrap();
        banks
            .bank(1)
            .unwrap()
            .bank()
            .slot(0)
            .unwrap()
            .abandon_request(ticket)
            .unwrap();

        let error = banks.prepare_inactive(11).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(banks.bank(1).unwrap().bank_address(), address);
        assert_eq!(banks.bank(1).unwrap().generation(), 8);
    }

    #[test]
    fn opening_rejects_malformed_mapped_slots() {
        let mut creator = SharedMemoryRegion::create(7).unwrap();
        creator.bank_mut().slots[0]
            .metadata
            .state
            .store(99, Ordering::Release);
        let Err(error) = SharedMemoryRegion::open(creator.name()) else {
            panic!("unknown raw slot state must be rejected");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        creator.bank_mut().slots[0]
            .metadata
            .state
            .store(SlotState::Free.raw(), Ordering::Release);
        let slot = &mut creator.bank_mut().slots[0];
        slot.publish_request(
            BlockTicket {
                generation: 7,
                sequence: 1,
            },
            BlockRequest {
                frame_count: 1,
                input_channel_count: 0,
                output_channel_count: 1,
                midi_event_count: 0,
                event_count: 1,
                flags: 0,
                sidechain_slots: 0,
            },
        )
        .unwrap();
        slot.events[0].frame_offset = slot.metadata.frame_count;
        let Err(error) = SharedMemoryRegion::open(creator.name()) else {
            panic!("out-of-range event offset must be rejected");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}

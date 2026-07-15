//! macOS POSIX shared-memory mappings for a fixed [`SharedBank`] layout.
//!
//! This is the sole Rust `unsafe` boundary for the Phase 1 feasibility prototype.
//! It maps initialized `SharedBank` bytes; all protocol state changes remain in the
//! safe `sp-shared-memory` and `sp-protocol` APIs.

use std::{
    ffi::CString,
    io,
    mem::{MaybeUninit, size_of},
    os::raw::{c_char, c_int, c_long, c_uint, c_void},
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

pub use sp_shared_memory::SharedBank;

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
    fn shm_open(name: *const c_char, flags: c_int) -> c_int;
    fn shm_unlink(name: *const c_char) -> c_int;
}

static NEXT_REGION_ID: AtomicU64 = AtomicU64::new(1);
static RUN_NONCE: OnceLock<u32> = OnceLock::new();

const SHARED_MEMORY_CREATE_ATTEMPTS: usize = 64;
const RUSAGE_SELF: c_int = 0;
const RUSAGE_CHILDREN: c_int = -1;

#[derive(Clone, Copy)]
#[repr(C)]
struct Timeval {
    seconds: c_long,
    microseconds: c_int,
}

#[repr(C)]
struct Rusage {
    user_time: Timeval,
    system_time: Timeval,
    maximum_resident_set_size: c_long,
    _remaining: [c_long; 13],
}

unsafe extern "C" {
    fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
}

/// Safe snapshot of host-process CPU use and high-water resident memory.
///
/// The snapshot is intentionally process-scoped. It does not claim per-worker CPU or
/// device-energy measurements, which require separately collected evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessResourceUsage {
    /// Accumulated user CPU time for the current process, rounded down to microseconds.
    pub user_cpu_micros: u64,
    /// Accumulated kernel CPU time for the current process, rounded down to microseconds.
    pub system_cpu_micros: u64,
    /// macOS reports this process high-water resident set size in bytes.
    pub max_resident_bytes: u64,
}

/// Captures a safe `getrusage(RUSAGE_SELF)` snapshot for the current process.
///
/// # Errors
///
/// Returns the operating-system error when Darwin cannot provide resource usage.
pub fn current_process_resource_usage() -> io::Result<ProcessResourceUsage> {
    resource_usage(RUSAGE_SELF)
}

/// Captures accumulated resource usage for child processes which have been reaped.
///
/// # Errors
///
/// Returns the operating-system error when Darwin cannot provide resource usage.
pub fn child_process_resource_usage() -> io::Result<ProcessResourceUsage> {
    resource_usage(RUSAGE_CHILDREN)
}

fn resource_usage(who: c_int) -> io::Result<ProcessResourceUsage> {
    let mut usage = MaybeUninit::<Rusage>::uninit();
    // SAFETY: Darwin writes one `rusage` record to the valid out pointer for the supported
    // `RUSAGE_SELF` or `RUSAGE_CHILDREN` selector.
    if unsafe { getrusage(who, usage.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a zero result documents that Darwin initialized the output record.
    let usage = unsafe { usage.assume_init() };
    Ok(ProcessResourceUsage {
        user_cpu_micros: timeval_to_micros(usage.user_time),
        system_cpu_micros: timeval_to_micros(usage.system_time),
        max_resident_bytes: u64::try_from(usage.maximum_resident_set_size).unwrap_or_default(),
    })
}

fn timeval_to_micros(time: Timeval) -> u64 {
    let seconds = u64::try_from(time.seconds).unwrap_or_default();
    let microseconds = u64::try_from(time.microseconds).unwrap_or_default();
    seconds
        .saturating_mul(1_000_000)
        .saturating_add(microseconds.min(999_999))
}

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
    /// mapped, or `InvalidData` if its header does not match this build.
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
        if !unsafe { bank.as_ref().is_compatible() } {
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

fn initialize_created_region(
    name: &CString,
    file_descriptor: c_int,
    generation: u64,
) -> io::Result<SharedMemoryRegion> {
    let result = (|| {
        let bank_bytes = bank_bytes_i64()?;
        // SAFETY: `file_descriptor` is open, and `bank_bytes` is the checked positive
        // size of the single mapping we create below.
        if unsafe { ftruncate(file_descriptor, bank_bytes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let bank = map_bank(file_descriptor)?;
        // SAFETY: this new mapping is exclusively owned until this function returns.
        // `ptr::write` initializes the Rust atomics and all fields before sharing its name.
        unsafe { ptr::write(bank.as_ptr(), SharedBank::new(generation)) };
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

fn map_bank(file_descriptor: c_int) -> io::Result<NonNull<SharedBank>> {
    // SAFETY: the descriptor is open; the mapping length is exactly `SharedBank`.
    // POSIX returns either `MAP_FAILED` or a page-aligned valid mapping.
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
    NonNull::new(address.cast()).ok_or_else(|| io::Error::other("mmap returned null"))
}

#[cfg(test)]
mod tests {
    use super::{
        MonotonicClock, SharedMemoryRegion, child_process_resource_usage,
        current_process_resource_usage, generated_name,
    };
    use std::time::Duration;

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
    fn resource_usage_snapshot_is_safe_and_nonnegative() {
        let snapshot = current_process_resource_usage().unwrap();
        assert!(
            snapshot
                .user_cpu_micros
                .saturating_add(snapshot.system_cpu_micros)
                > 0
        );
        assert!(child_process_resource_usage().is_ok());
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
}

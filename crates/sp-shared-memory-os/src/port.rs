//! A VM port's region: one header page and one rack's two banks.
//!
//! A port server owns the memory (`sp-ivshmem`); the guest sees it as BAR 2 of the port's
//! `ivshmem-doorbell` device. The layout is fixed by this build and is the same on the host
//! and in the guest: the [`PortHeader`] at offset 0, bank `i` at [`port_bank_offset`]. The
//! host writes the banks first and the magic last, so a guest that sees the magic sees whole
//! banks.

use crate::{BANK_COUNT, SharedBank};
use std::{
    io,
    mem::size_of,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

/// `SPVMPORT`, little-endian: the region holds a valid header and two banks.
pub const PORT_MAGIC: u64 = u64::from_le_bytes(*b"SPVMPORT");
/// The version of this layout.
pub const PORT_LAYOUT_VERSION: u32 = 1;
/// The header page. Banks start on page boundaries after it.
pub const PORT_HEADER_BYTES: usize = 4096;
/// Port `n`'s device is at PCI bus 0, device `VM_PORT_FIRST_PCI_DEVICE + n`, function 0.
pub const VM_PORT_FIRST_PCI_DEVICE: u32 = 0x10;
/// The most VM ports one VM has: PCI devices 0x10 to 0x17.
pub const MAX_VM_PORTS: u32 = 8;

const BANK_STRIDE: usize = size_of::<SharedBank>().next_multiple_of(PORT_HEADER_BYTES);

/// The first bytes of a port's region.
#[repr(C)]
pub struct PortHeader {
    /// [`PORT_MAGIC`] when the region is valid; zero while the host rewrites it.
    pub magic: AtomicU64,
    /// [`PORT_LAYOUT_VERSION`].
    pub layout_version: AtomicU32,
    /// The port number, which the guest compares with its device's.
    pub port: AtomicU32,
    /// `size_of::<SharedBank>()` of the host build.
    pub bank_bytes: AtomicU64,
    /// The byte offset of each bank.
    pub bank_offsets: [AtomicU64; BANK_COUNT],
}

impl PortHeader {
    /// Checks that the region is valid for `port` and this build's layout.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` while the host has not written the region, and `InvalidData` for a
    /// region of another port or another build.
    pub fn check(&self, port: u32) -> io::Result<()> {
        if self.magic.load(Ordering::Acquire) != PORT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the host has not written the VM port region",
            ));
        }
        let offsets_match = (0..BANK_COUNT).all(|bank| {
            self.bank_offsets[bank].load(Ordering::Relaxed) == port_bank_offset(bank) as u64
        });
        if self.layout_version.load(Ordering::Relaxed) != PORT_LAYOUT_VERSION
            || self.bank_bytes.load(Ordering::Relaxed) != size_of::<SharedBank>() as u64
            || !offsets_match
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the VM port region has another layout",
            ));
        }
        if self.port.load(Ordering::Relaxed) != port {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the VM port region belongs to another port",
            ));
        }
        Ok(())
    }
}

/// The byte offset of bank `bank` (0 or 1) in a port's region.
pub const fn port_bank_offset(bank: usize) -> usize {
    PORT_HEADER_BYTES + bank * BANK_STRIDE
}

/// The size of a port's region: the header and two banks, rounded up to a power of two, as a
/// PCI BAR must be. Give this size to the port server.
pub const fn port_region_bytes() -> u64 {
    (port_bank_offset(BANK_COUNT) as u64).next_power_of_two()
}

/// The name of a port's bank, which a VM worker gives to its backend to open it.
pub fn port_bank_name(port: u32, bank: usize) -> String {
    format!("vmport-{port}-bank-{bank}")
}

/// The port and bank of a [`port_bank_name`], or `None` for any other name.
pub fn parse_port_bank_name(name: &str) -> Option<(u32, usize)> {
    let (port, bank) = name.strip_prefix("vmport-")?.split_once("-bank-")?;
    let port = port.parse().ok().filter(|port| *port < MAX_VM_PORTS)?;
    let bank = bank.parse().ok().filter(|bank| *bank < BANK_COUNT)?;
    // Reject spellings such as "+1" or "01" that do not round-trip.
    (port_bank_name(port, bank) == name).then_some((port, bank))
}

#[cfg(target_os = "linux")]
pub(crate) use host::PortMapping;
#[cfg(target_os = "linux")]
pub use host::{attach_port_bank, create_port_banks};

#[cfg(target_os = "linux")]
mod host {
    use super::{
        MAX_VM_PORTS, PORT_LAYOUT_VERSION, PORT_MAGIC, PortHeader, port_bank_name, port_bank_offset,
    };
    use crate::{
        BANK_COUNT, Backing, MappedRackBanks, SharedBank, SharedMemoryRegion, port_region_bytes,
        sys,
    };
    use std::{
        ffi::CString,
        io,
        mem::size_of,
        os::{
            fd::{BorrowedFd, OwnedFd},
            raw::c_void,
        },
        ptr::{self, NonNull},
        sync::{Arc, atomic::Ordering},
    };

    /// The host's one mapping of a port's region, shared by its two banks.
    pub(crate) struct PortMapping {
        /// Dropped after `Drop::drop` unmaps the region.
        _object: sys::RawObject,
        address: NonNull<c_void>,
        length: usize,
    }

    // SAFETY: the mapping is plain shared memory. Each bank's owner reaches its bank only
    // through `SharedMemoryRegion`; cross-process access is ordered by the protocol atomics.
    unsafe impl Send for PortMapping {}
    // SAFETY: as above; this type itself gives no access to the memory.
    unsafe impl Sync for PortMapping {}

    impl Drop for PortMapping {
        fn drop(&mut self) {
            // SAFETY: this value owns the mapping and every bank that borrowed it has dropped.
            unsafe { sys::unmap(self.address, self.length) };
        }
    }

    /// Maps a VM port's region from its port server's `memory` and writes a fresh header and
    /// two fresh banks: bank 0 active with `active_generation`, bank 1 inactive with the next.
    ///
    /// Every request wake of the pair is one write to `doorbell`, the eventfd that interrupts
    /// the guest on this port's vector. Each bank keeps its own copy of it.
    ///
    /// # Safety
    ///
    /// No other process may use this port's banks: every guest worker on the port must have
    /// exited, and no other host mapping of the region may be live.
    ///
    /// # Errors
    ///
    /// Returns `InvalidData` if `memory` is smaller than [`port_region_bytes`], `InvalidInput`
    /// for a port that is not below [`MAX_VM_PORTS`] or a generation that is zero or the
    /// largest value, and the operating-system error if the mapping or a descriptor copy
    /// fails.
    pub unsafe fn create_port_banks(
        memory: OwnedFd,
        port: u32,
        active_generation: u64,
        doorbell: BorrowedFd<'_>,
    ) -> io::Result<MappedRackBanks> {
        // The guest opens a bank by its name, and it knows no port from this one on.
        if port >= MAX_VM_PORTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VM port is out of range",
            ));
        }
        let generations = [
            active_generation,
            active_generation.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "active generation cannot reserve an inactive bank",
                )
            })?,
        ];
        let fresh = generations
            .iter()
            .map(|&generation| {
                SharedBank::new(generation).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid VM port bank generation: {error}"),
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mapping = map_region(memory)?;
        let address = mapping.address;

        // SAFETY: the region is at least `length` bytes, page aligned, and starts with the
        // header. The header has only atomics, so a shared reference is sound even while a
        // guest reads it.
        let header = unsafe { address.cast::<PortHeader>().as_ref() };
        header.magic.store(0, Ordering::Release);
        let banks: [NonNull<SharedBank>; BANK_COUNT] = std::array::from_fn(|bank| {
            // SAFETY: `port_bank_offset(bank) + size_of::<SharedBank>()` is within `length`,
            // and the offset is page aligned, which satisfies the bank's alignment.
            unsafe { address.byte_add(port_bank_offset(bank)).cast() }
        });
        for (bank, initialized) in banks.iter().zip(&fresh) {
            // SAFETY: by this function's contract no other process uses the bank. The source
            // is fully initialized and holds no pointers or destructors.
            unsafe { ptr::copy_nonoverlapping(initialized.as_ref(), bank.as_ptr(), 1) };
        }
        header
            .layout_version
            .store(PORT_LAYOUT_VERSION, Ordering::Relaxed);
        header.port.store(port, Ordering::Relaxed);
        header
            .bank_bytes
            .store(size_of::<SharedBank>() as u64, Ordering::Relaxed);
        for (bank, offset) in header.bank_offsets.iter().enumerate() {
            offset.store(port_bank_offset(bank) as u64, Ordering::Relaxed);
        }
        header.magic.store(PORT_MAGIC, Ordering::Release);

        let mut regions = Vec::with_capacity(BANK_COUNT);
        for (index, bank) in banks.into_iter().enumerate() {
            regions.push(SharedMemoryRegion {
                name: CString::new(port_bank_name(port, index))
                    .map_err(|_| io::Error::other("generated invalid bank name"))?,
                backing: Backing::Port {
                    _mapping: Arc::clone(&mapping),
                    eventfd: doorbell.try_clone_to_owned()?,
                },
                bank,
            });
        }
        let [active, inactive]: [SharedMemoryRegion; BANK_COUNT] = regions
            .try_into()
            .map_err(|_| io::Error::other("a VM port has two banks"))?;
        // SAFETY: no worker uses either bank (this function's contract).
        unsafe { MappedRackBanks::from_regions(active, inactive) }
    }

    /// Attaches to bank `bank` of a port region that a host has written, as a guest worker
    /// would: for a Linux process that plays the guest in tests and in the V0 probe.
    ///
    /// [`SharedMemoryRegion::wait_for_request`] waits on `interrupt`, the guest's own eventfd
    /// for the port's vector.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` while the host has not written the region, `InvalidData` for a
    /// region of another port or layout or an incompatible bank, `InvalidInput` for a bank
    /// other than 0 or 1, and the operating-system error if the mapping fails.
    pub fn attach_port_bank(
        memory: OwnedFd,
        port: u32,
        bank: usize,
        interrupt: OwnedFd,
    ) -> io::Result<SharedMemoryRegion> {
        if bank >= BANK_COUNT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a VM port has banks 0 and 1",
            ));
        }
        let mapping = map_region(memory)?;
        // SAFETY: the region starts with the header, which has only atomics.
        unsafe { mapping.address.cast::<PortHeader>().as_ref() }.check(port)?;
        // SAFETY: the bank's page-aligned offset and size lie within the region.
        let pointer: NonNull<SharedBank> =
            unsafe { mapping.address.byte_add(port_bank_offset(bank)).cast() };
        // SAFETY: the host wrote an initialized bank before the header's magic (checked).
        let shared = unsafe { pointer.as_ref() };
        if !shared.is_compatible() || shared.validate_mapped_contents().is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the VM port bank is not compatible with this build",
            ));
        }
        Ok(SharedMemoryRegion {
            name: CString::new(port_bank_name(port, bank))
                .map_err(|_| io::Error::other("generated invalid bank name"))?,
            backing: Backing::Port {
                _mapping: mapping,
                eventfd: interrupt,
            },
            bank: pointer,
        })
    }

    /// Maps a whole port region from a port server's memory.
    fn map_region(memory: OwnedFd) -> io::Result<Arc<PortMapping>> {
        let object = sys::RawObject::from(memory);
        let length = usize::try_from(port_region_bytes())
            .map_err(|_| io::Error::other("the VM port region exceeds the address space"))?;
        if sys::len(&object)? < i64::try_from(length).unwrap_or(i64::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the port server's memory is smaller than a VM port region",
            ));
        }
        let address = sys::map(&object, length)?;
        Ok(Arc::new(PortMapping {
            _object: object,
            address,
            length,
        }))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        PORT_MAGIC, PortHeader, attach_port_bank, create_port_banks, parse_port_bank_name,
        port_bank_name, port_bank_offset, port_region_bytes,
    };
    use crate::{MAX_VM_PORTS, SharedBank};
    use std::{
        io,
        os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    fn memfd(length: u64) -> OwnedFd {
        // SAFETY: the name is NUL-terminated; the returned descriptor is checked and owned.
        let fd = unsafe { libc::memfd_create(c"sp-port-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
        // SAFETY: `fd` is a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the descriptor is open.
        assert_eq!(
            unsafe { libc::ftruncate(fd.as_raw_fd(), libc::off_t::try_from(length).unwrap()) },
            0
        );
        fd
    }

    fn eventfd() -> OwnedFd {
        // SAFETY: plain flags; the returned descriptor is checked and owned.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a fresh descriptor that nothing else owns.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn eventfd_count(fd: &OwnedFd) -> u64 {
        let mut count = [0_u8; 8];
        // SAFETY: `count` is 8 writable bytes.
        let read = unsafe { libc::read(fd.as_raw_fd(), count.as_mut_ptr().cast(), 8) };
        if read == 8 {
            u64::from_ne_bytes(count)
        } else {
            0
        }
    }

    /// A second process's view of the region: the guest's BAR 2.
    struct Guest {
        address: *mut libc::c_void,
        length: usize,
    }

    impl Guest {
        fn map(memory: &OwnedFd) -> Self {
            let length = usize::try_from(port_region_bytes()).unwrap();
            // SAFETY: a shared mapping of an open memfd of at least `length` bytes.
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    memory.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            Self { address, length }
        }

        fn header(&self) -> &PortHeader {
            // SAFETY: the mapping starts with the header, which has only atomics.
            unsafe { &*self.address.cast::<PortHeader>() }
        }

        fn bank(&self, bank: usize) -> &SharedBank {
            // SAFETY: the host wrote an initialized bank at this page-aligned offset.
            unsafe {
                &*self
                    .address
                    .byte_add(port_bank_offset(bank))
                    .cast::<SharedBank>()
            }
        }
    }

    impl Drop for Guest {
        fn drop(&mut self) {
            // SAFETY: this value owns the mapping.
            unsafe { libc::munmap(self.address, self.length) };
        }
    }

    #[test]
    fn the_host_writes_a_header_and_two_banks_that_the_guest_reads() {
        let memory = memfd(port_region_bytes());
        let doorbell = eventfd();
        // SAFETY: no worker uses this test region.
        let banks =
            unsafe { create_port_banks(memory.try_clone().unwrap(), 3, 7, doorbell.as_fd()) }
                .expect("port banks");
        assert_eq!(banks.active_bank().name(), "vmport-3-bank-0");
        assert_eq!(banks.bank(1).unwrap().name(), "vmport-3-bank-1");

        let guest = Guest::map(&memory);
        assert_eq!(guest.header().magic.load(Ordering::Acquire), PORT_MAGIC);
        guest.header().check(3).expect("valid for port 3");
        assert_eq!(
            guest.header().check(4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(guest.bank(0).is_compatible());
        assert_eq!(guest.bank(0).generation(), 7);
        assert_eq!(guest.bank(1).generation(), 8);
    }

    #[test]
    fn a_request_wake_rings_the_doorbell_and_bumps_the_shared_sequence() {
        let memory = memfd(port_region_bytes());
        let doorbell = eventfd();
        // SAFETY: no worker uses this test region.
        let banks =
            unsafe { create_port_banks(memory.try_clone().unwrap(), 0, 1, doorbell.as_fd()) }
                .expect("port banks");
        let guest = Guest::map(&memory);
        let before = guest
            .bank(0)
            .header
            .request_wake_sequence
            .load(Ordering::Acquire);

        assert!(banks.active_bank().notify_request());
        assert!(banks.active_bank().notify_request());
        assert_eq!(eventfd_count(&doorbell), 2);
        assert_eq!(
            guest
                .bank(0)
                .header
                .request_wake_sequence
                .load(Ordering::Acquire),
            before.wrapping_add(2)
        );
    }

    #[test]
    fn an_attached_guest_waits_for_the_host_request_wake() {
        let memory = memfd(port_region_bytes());
        let interrupt = eventfd();
        let unwritten = attach_port_bank(memory.try_clone().unwrap(), 2, 0, eventfd());
        assert_eq!(unwritten.err().unwrap().kind(), io::ErrorKind::WouldBlock);

        // SAFETY: no worker uses this test region yet.
        let banks =
            unsafe { create_port_banks(memory.try_clone().unwrap(), 2, 5, interrupt.as_fd()) }
                .expect("port banks");
        let wrong_port = attach_port_bank(memory.try_clone().unwrap(), 3, 0, eventfd());
        assert_eq!(wrong_port.err().unwrap().kind(), io::ErrorKind::InvalidData);
        let guest = attach_port_bank(memory, 2, 0, interrupt).expect("guest bank");
        assert_eq!(guest.generation(), 5);

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let observed = guest.request_wake_sequence();
            ready_tx.send(()).unwrap();
            let started = Instant::now();
            guest
                .wait_for_request(observed, Duration::from_secs(3))
                .unwrap();
            (guest.request_wake_sequence() != observed, started.elapsed())
        });
        ready_rx.recv().unwrap();
        assert!(banks.active_bank().notify_request());
        let (woken, elapsed) = waiter.join().unwrap();
        assert!(woken);
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn a_region_smaller_than_the_layout_and_a_port_out_of_range_are_refused() {
        let memory = memfd(port_region_bytes() / 2);
        // SAFETY: no worker uses this test region.
        let error = unsafe { create_port_banks(memory, 0, 1, eventfd().as_fd()) }
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // The guest could not open a bank of this port by its name.
        let memory = memfd(port_region_bytes());
        // SAFETY: no worker uses this test region.
        let error = unsafe { create_port_banks(memory, MAX_VM_PORTS, 1, eventfd().as_fd()) }
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_region_holds_both_banks_in_a_power_of_two() {
        assert!(port_bank_offset(1) >= port_bank_offset(0) + size_of::<SharedBank>());
        assert!(port_region_bytes().is_power_of_two());
    }

    #[test]
    fn bank_names_round_trip_and_reject_other_spellings() {
        assert_eq!(parse_port_bank_name(&port_bank_name(7, 1)), Some((7, 1)));
        for name in [
            "vmport-8-bank-0",
            "vmport-1-bank-2",
            "vmport-01-bank-0",
            "/sp-1-2-3",
        ] {
            assert_eq!(parse_port_bank_name(name), None, "{name}");
        }
    }
}

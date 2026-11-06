//! A Windows worker in Lambda's VM, attached to one VM port through the virtio-win IVSHMEM
//! driver (`ivshmem/Public.h` in virtio-win, pinned by the VM image).
//!
//! Port `n` is the IVSHMEM device at PCI bus 0, device [`VM_PORT_FIRST_PCI_DEVICE`] + `n`. The
//! driver lets one handle map a device at a time, so one worker owns one port. The mapping is
//! cached (`MmCached`): the BAR is host RAM. The host's doorbell on vector 0 sets an event
//! that the driver signals from its DPC.

use std::{ffi::c_void, io, mem, ptr, sync::OnceLock};

use windows_sys::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
            SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SPDRP_ADDRESS, SPDRP_BUSNUMBER,
            SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
            SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW,
        },
        Foundation::{
            CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{CreateEventW, WaitForSingleObject},
        },
    },
    core::GUID,
};

use crate::port::VM_PORT_FIRST_PCI_DEVICE;

static GUID_DEVINTERFACE_IVSHMEM: GUID = GUID::from_u128(0xdf57_6976_569d_4672_95a0_f57e_4ea0_b210);

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, function, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const fn ivshmem_ioctl(function: u32) -> u32 {
    (0x22 << 16) | (function << 2)
}

const IOCTL_IVSHMEM_REQUEST_MMAP: u32 = ivshmem_ioctl(0x802);
const IOCTL_IVSHMEM_REGISTER_EVENT: u32 = ivshmem_ioctl(0x805);
const IVSHMEM_CACHE_CACHED: u8 = 1;

#[repr(C)]
struct IvshmemMmapConfig {
    cache_mode: u8,
}

#[repr(C)]
struct IvshmemMmap {
    peer_id: u16,
    size: u64,
    ptr: *mut c_void,
    vectors: u16,
}

#[repr(C)]
struct IvshmemEvent {
    vector: u16,
    event: HANDLE,
    single_shot: u8,
}

/// One owned port: the device handle (whose close releases the mapping) and its doorbell
/// event.
pub(crate) struct PortDevice {
    device: HANDLE,
    event: HANDLE,
    address: ptr::NonNull<c_void>,
    size: u64,
    peer_id: u16,
}

// SAFETY: the handles and the mapping belong to this process, not to a thread.
unsafe impl Send for PortDevice {}

impl Drop for PortDevice {
    fn drop(&mut self) {
        // SAFETY: this value owns both handles. Closing the device releases the mapping.
        unsafe {
            CloseHandle(self.device);
            CloseHandle(self.event);
        }
    }
}

impl PortDevice {
    /// Opens port `port`'s device, maps its region and registers the doorbell event.
    ///
    /// Fails with `ResourceBusy` while another process maps the port: an old worker that has
    /// not exited yet.
    pub(crate) fn open(port: u32) -> io::Result<Self> {
        let path = device_path(port)?;
        // SAFETY: `path` is NUL-terminated; the other arguments are documented constants.
        let device = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if device == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: an unnamed auto-reset event, not signalled.
        let event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if event.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: owned here and closed once.
            unsafe { CloseHandle(device) };
            return Err(error);
        }
        let mut mapped = IvshmemMmap {
            peer_id: 0,
            size: 0,
            ptr: ptr::null_mut(),
            vectors: 0,
        };
        let config = IvshmemMmapConfig {
            cache_mode: IVSHMEM_CACHE_CACHED,
        };
        let registration = IvshmemEvent {
            vector: 0,
            event,
            single_shot: 0,
        };
        // SAFETY: each buffer is a live `repr(C)` value of the size passed.
        let result = unsafe {
            ioctl(
                device,
                IOCTL_IVSHMEM_REQUEST_MMAP,
                &config,
                Some(&mut mapped),
            )
            .and_then(|()| {
                ioctl::<_, ()>(device, IOCTL_IVSHMEM_REGISTER_EVENT, &registration, None)
            })
        };
        let address = result.and_then(|()| {
            if mapped.vectors == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the VM port device has no interrupt vector",
                ));
            }
            ptr::NonNull::new(mapped.ptr)
                .ok_or_else(|| io::Error::other("the IVSHMEM driver mapped a null region"))
        });
        match address {
            Ok(address) => Ok(Self {
                device,
                event,
                address,
                size: mapped.size,
                peer_id: mapped.peer_id,
            }),
            Err(error) => {
                // SAFETY: both handles are owned here and closed once.
                unsafe {
                    CloseHandle(device);
                    CloseHandle(event);
                }
                Err(busy_if_attached(error))
            }
        }
    }

    pub(crate) fn address(&self) -> ptr::NonNull<c_void> {
        self.address
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// This guest's peer ID on the port server, for a check against the host's view.
    pub(crate) fn peer_id(&self) -> u16 {
        self.peer_id
    }

    /// Waits for the doorbell up to `timeout_ns`, rounded up to whole milliseconds.
    /// Returns zero on a ring or a timeout, else a Windows error code.
    pub(crate) fn wait(&self, timeout_ns: u64) -> i32 {
        let milliseconds = u32::try_from(timeout_ns.div_ceil(1_000_000)).unwrap_or(u32::MAX - 1);
        // SAFETY: `event` is a live event handle.
        match unsafe { WaitForSingleObject(self.event, milliseconds) } {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => 0,
            _ => io::Error::last_os_error().raw_os_error().unwrap_or(-1),
        }
    }
}

/// `DeviceIoControl` with one fixed-size input and an optional fixed-size output.
///
/// # Safety
///
/// `code` must take an `I` and return an `O` (or nothing for `None`).
unsafe fn ioctl<I, O>(
    device: HANDLE,
    code: u32,
    input: &I,
    output: Option<&mut O>,
) -> io::Result<()> {
    let (output, output_size) = output.map_or((ptr::null_mut(), 0), |output| {
        (ptr::from_mut(output).cast::<c_void>(), mem::size_of::<O>())
    });
    let mut returned = 0_u32;
    // SAFETY: the buffers are live and sized by their types (the caller's contract).
    let ok = unsafe {
        DeviceIoControl(
            device,
            code,
            ptr::from_ref(input).cast(),
            u32::try_from(mem::size_of::<I>()).unwrap_or(u32::MAX),
            output,
            u32::try_from(output_size).unwrap_or(u32::MAX),
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `STATUS_DEVICE_ALREADY_ATTACHED` reaches user mode as `ERROR_BUSY` or
/// `ERROR_DEVICE_ALREADY_ATTACHED`.
fn busy_if_attached(error: io::Error) -> io::Error {
    const ERROR_BUSY: i32 = 170;
    const ERROR_DEVICE_ALREADY_ATTACHED: i32 = 548;
    match error.raw_os_error() {
        Some(ERROR_BUSY | ERROR_DEVICE_ALREADY_ATTACHED) => io::Error::new(
            io::ErrorKind::ResourceBusy,
            "another process maps this VM port",
        ),
        _ => error,
    }
}

/// The interface path of the IVSHMEM device at bus 0, device `FIRST + port`.
fn device_path(port: u32) -> io::Result<Vec<u16>> {
    let wanted = VM_PORT_FIRST_PCI_DEVICE
        .checked_add(port)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "VM port is out of range"))?;
    // SAFETY: a present-device interface list for one class GUID.
    let set: HDEVINFO = unsafe {
        SetupDiGetClassDevsW(
            &raw const GUID_DEVINTERFACE_IVSHMEM,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == -1 {
        return Err(io::Error::last_os_error());
    }
    let found = (0..)
        .map_while(|index| interface(set, index))
        .find_map(|(path, info)| {
            let bus = property(set, &info, SPDRP_BUSNUMBER)?;
            let address = property(set, &info, SPDRP_ADDRESS)?;
            // SPDRP_ADDRESS of a PCI device is (device << 16) | function.
            let (device, function) = (address >> 16, address & 0xffff);
            (bus == 0 && device == wanted && function == 0).then_some(path)
        });
    // SAFETY: `set` came from SetupDiGetClassDevsW and is destroyed once.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no IVSHMEM device for VM port {port} at PCI 00:{wanted:02x}.0"),
        )
    })
}

/// The path and device of interface `index`, or `None` after the last one.
fn interface(set: HDEVINFO, index: u32) -> Option<(Vec<u16>, SP_DEVINFO_DATA)> {
    // SAFETY: plain-data records; `cbSize` is set before use.
    let mut data: SP_DEVICE_INTERFACE_DATA = unsafe { mem::zeroed() };
    data.cbSize = u32::try_from(mem::size_of::<SP_DEVICE_INTERFACE_DATA>()).ok()?;
    // SAFETY: `set` is live and `data` is a sized record.
    if unsafe {
        SetupDiEnumDeviceInterfaces(
            set,
            ptr::null(),
            &raw const GUID_DEVINTERFACE_IVSHMEM,
            index,
            &raw mut data,
        )
    } == 0
    {
        return None;
    }
    let mut required = 0_u32;
    // SAFETY: a size query with no buffer.
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            &raw const data,
            ptr::null_mut(),
            0,
            &raw mut required,
            ptr::null_mut(),
        )
    };
    // A u64 buffer is aligned for the detail record's u32 header.
    let mut buffer = vec![0_u64; usize::try_from(required).ok()?.div_ceil(8).max(1)];
    let detail = buffer
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    // SAFETY: plain-data record; `cbSize` is set before use.
    let mut info: SP_DEVINFO_DATA = unsafe { mem::zeroed() };
    info.cbSize = u32::try_from(mem::size_of::<SP_DEVINFO_DATA>()).ok()?;
    // SAFETY: `detail` points to `required` writable, aligned bytes. `cbSize` is the size of
    // the fixed header, as the API requires.
    let ok = unsafe {
        (*detail).cbSize =
            u32::try_from(mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()).ok()?;
        SetupDiGetDeviceInterfaceDetailW(
            set,
            &raw const data,
            detail,
            required,
            ptr::null_mut(),
            &raw mut info,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: the API wrote a NUL-terminated path inside `buffer`, which holds `capacity`
    // UTF-16 units from the path's start.
    let path = unsafe {
        let start = ptr::addr_of!((*detail).DevicePath).cast::<u16>();
        let capacity =
            (buffer.len() * 8 - mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath)) / 2;
        let units = std::slice::from_raw_parts(start, capacity);
        let length = units.iter().position(|&unit| unit == 0)?;
        units[..=length].to_vec()
    };
    Some((path, info))
}

/// One `u32` registry property of a device.
fn property(set: HDEVINFO, info: &SP_DEVINFO_DATA, property: u32) -> Option<u32> {
    let mut value = 0_u32;
    // SAFETY: `value` is 4 writable bytes.
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            ptr::from_ref(info),
            property,
            ptr::null_mut(),
            (&raw mut value).cast(),
            4,
            ptr::null_mut(),
        )
    };
    (ok != 0).then_some(value)
}

/// The performance counter's frequency, or `None` if Windows has none (never on Windows 10+).
pub(crate) fn qpc_frequency() -> Option<u64> {
    static FREQUENCY: OnceLock<Option<u64>> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut frequency = 0_i64;
        // SAFETY: `frequency` is valid writable storage.
        let ok = unsafe { QueryPerformanceFrequency(&raw mut frequency) } != 0;
        u64::try_from(frequency)
            .ok()
            .filter(|&frequency| ok && frequency > 0)
    })
}

/// The performance counter, in its own ticks. The guest's clock domain: never compared with
/// a host tick.
pub(crate) fn qpc_now() -> u64 {
    let mut now = 0_i64;
    // SAFETY: `now` is valid writable storage; the call cannot fail on Windows 10+.
    unsafe { QueryPerformanceCounter(&raw mut now) };
    u64::try_from(now).unwrap_or_default()
}

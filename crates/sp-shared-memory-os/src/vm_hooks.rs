//! In Lambda's Windows VM, plug-in code cannot move threads onto the real-time vCPUs
//! (Lambda's VM plan, section 06; ADR 0011).
//!
//! [`set_windows_cpus`] gives the Windows vCPUs. [`sweep`] then patches the import address
//! tables of plug-in modules: every module outside the Windows directory except the
//! executable. In those modules:
//!
//! - `SetThreadAffinityMask` and `SetThreadGroupAffinity` keep only the Windows vCPUs (all of
//!   them if no Windows vCPU is left);
//! - `SetProcessAffinityMask`, `SetThreadSelectedCpuSets` and `SetProcessDefaultCpuSets` do
//!   nothing and succeed: the agent owns the process mask;
//! - `GetProcAddress` gives these hooks for those names, so a dynamic lookup gets them too.
//!
//! A worker calls [`sweep`] after each control request: plug-ins load in requests. A sweep
//! walks only modules that it has not patched yet; it does not run while the worker is idle,
//! because it holds the loader lock. The worker's thread-attach callback puts new threads on the Windows vCPUs, also those
//! that a plug-in's `DllMain` starts before the next sweep. This lowers the risk; it does not
//! contain a plug-in that writes its own code.

use std::{
    ffi::c_void,
    mem, ptr,
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use windows_sys::{
    Win32::{
        Foundation::{FARPROC, HANDLE, HMODULE},
        System::{
            Diagnostics::Debug::{IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_NT_HEADERS64},
            LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress},
            Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect},
            ProcessStatus::K32EnumProcessModules,
            SystemInformation::{GROUP_AFFINITY, GetSystemWindowsDirectoryW},
            SystemServices::{IMAGE_DOS_HEADER, IMAGE_IMPORT_DESCRIPTOR},
            Threading::GetCurrentProcess,
        },
    },
    core::{BOOL, PCSTR},
};

/// The Windows vCPUs, or zero outside the VM.
static WINDOWS_CPUS: AtomicUsize = AtomicUsize::new(0);

/// Sets the Windows vCPUs as a mask (`SP_VM_WINDOWS_CPUS`). Zero turns the hooks off.
pub fn set_windows_cpus(mask: usize) {
    WINDOWS_CPUS.store(mask, Ordering::Relaxed);
}

/// The Windows vCPUs as a mask, or zero outside the VM.
pub fn windows_cpus() -> usize {
    WINDOWS_CPUS.load(Ordering::Relaxed)
}

type SetThreadAffinityMask = unsafe extern "system" fn(HANDLE, usize) -> usize;
type SetThreadGroupAffinity =
    unsafe extern "system" fn(HANDLE, *const GROUP_AFFINITY, *mut GROUP_AFFINITY) -> BOOL;
type GetProcAddressFn = unsafe extern "system" fn(HMODULE, PCSTR) -> FARPROC;

/// The real functions, and each real address with its hook.
struct Hooks {
    set_thread_affinity_mask: SetThreadAffinityMask,
    set_thread_group_affinity: SetThreadGroupAffinity,
    get_proc_address: GetProcAddressFn,
    /// `(real address, hook address)`, for kernel32's and kernelbase's exports.
    table: Vec<(usize, usize)>,
    windows_dir: Vec<u16>,
}

static HOOKS: OnceLock<Option<Hooks>> = OnceLock::new();
/// The modules patched so far, by base address. One patcher at a time: two would race on a
/// page's protection.
static PATCHED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn hooks() -> Option<&'static Hooks> {
    HOOKS.get_or_init(load).as_ref()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn load() -> Option<Hooks> {
    type AffinityFlag = unsafe extern "system" fn(HANDLE, usize) -> BOOL;
    type CpuSets = unsafe extern "system" fn(HANDLE, *const u32, u32) -> BOOL;
    let cpu_sets = ignore_cpu_sets as CpuSets as usize;
    let hooked: [(&str, usize); 6] = [
        (
            "SetThreadAffinityMask",
            set_thread_affinity_mask as SetThreadAffinityMask as usize,
        ),
        (
            "SetThreadGroupAffinity",
            set_thread_group_affinity as SetThreadGroupAffinity as usize,
        ),
        (
            "SetProcessAffinityMask",
            set_process_affinity_mask as AffinityFlag as usize,
        ),
        ("SetThreadSelectedCpuSets", cpu_sets),
        ("SetProcessDefaultCpuSets", cpu_sets),
        (
            "GetProcAddress",
            get_proc_address as GetProcAddressFn as usize,
        ),
    ];
    let modules: Vec<HMODULE> = ["kernelbase.dll", "kernel32.dll"]
        .iter()
        .map(|name| {
            let name = wide(name);
            // SAFETY: a NUL-terminated name; both modules are loaded in every process.
            unsafe { GetModuleHandleW(name.as_ptr()) }
        })
        .filter(|module| !module.is_null())
        .collect();
    let find = |name: &str| -> Option<usize> {
        let name = std::ffi::CString::new(name).ok()?;
        modules.iter().find_map(|&module| {
            // SAFETY: a loaded module and a NUL-terminated name.
            unsafe { GetProcAddress(module, name.as_ptr().cast()) }
                .map(|function| function as usize)
        })
    };
    let mut table = Vec::new();
    for (name, hook) in hooked {
        let Ok(c_name) = std::ffi::CString::new(name) else {
            continue;
        };
        for &module in &modules {
            // SAFETY: as above.
            if let Some(real) = unsafe { GetProcAddress(module, c_name.as_ptr().cast()) } {
                table.push((real as usize, hook));
            }
        }
    }
    let mut windows_dir = vec![0_u16; 260];
    // SAFETY: the buffer has the length passed.
    let length = unsafe { GetSystemWindowsDirectoryW(windows_dir.as_mut_ptr(), 260) } as usize;
    windows_dir.truncate(length.min(260));
    // SAFETY: each address is the export of that name, whose type is the alias.
    unsafe {
        Some(Hooks {
            set_thread_affinity_mask: mem::transmute::<usize, SetThreadAffinityMask>(find(
                "SetThreadAffinityMask",
            )?),
            set_thread_group_affinity: mem::transmute::<usize, SetThreadGroupAffinity>(find(
                "SetThreadGroupAffinity",
            )?),
            get_proc_address: mem::transmute::<usize, GetProcAddressFn>(find("GetProcAddress")?),
            table,
            windows_dir,
        })
    }
}

/// `mask` less the real-time vCPUs, or every Windows vCPU if it held only real-time ones.
fn restrict(mask: usize) -> usize {
    let windows = windows_cpus();
    match (windows, mask & windows) {
        (0, _) => mask,
        (_, 0) => windows,
        (_, allowed) => allowed,
    }
}

unsafe extern "system" fn set_thread_affinity_mask(thread: HANDLE, mask: usize) -> usize {
    match hooks() {
        // SAFETY: the real function, with the caller's handle.
        Some(hooks) => unsafe { (hooks.set_thread_affinity_mask)(thread, restrict(mask)) },
        None => 0,
    }
}

unsafe extern "system" fn set_thread_group_affinity(
    thread: HANDLE,
    affinity: *const GROUP_AFFINITY,
    previous: *mut GROUP_AFFINITY,
) -> BOOL {
    let Some(hooks) = hooks() else {
        return 0;
    };
    if affinity.is_null() {
        // SAFETY: the real function rejects the null argument itself.
        return unsafe { (hooks.set_thread_group_affinity)(thread, affinity, previous) };
    }
    // SAFETY: the caller passes a valid record; the VM has one processor group.
    let mut restricted = unsafe { ptr::read(affinity) };
    restricted.Mask = restrict(restricted.Mask);
    // SAFETY: the real function with a valid record.
    unsafe { (hooks.set_thread_group_affinity)(thread, &raw const restricted, previous) }
}

unsafe extern "system" fn set_process_affinity_mask(_process: HANDLE, _mask: usize) -> BOOL {
    1
}

unsafe extern "system" fn ignore_cpu_sets(_handle: HANDLE, _ids: *const u32, _count: u32) -> BOOL {
    1
}

unsafe extern "system" fn get_proc_address(module: HMODULE, name: PCSTR) -> FARPROC {
    let hooks = hooks()?;
    // SAFETY: the real function with the caller's arguments.
    let function = unsafe { (hooks.get_proc_address)(module, name) }?;
    let hook = hooks
        .table
        .iter()
        .find(|(real, _)| *real == function as usize)
        .map(|(_, hook)| *hook);
    match hook {
        // SAFETY: a hook has the signature of the export it replaces; FARPROC is opaque.
        Some(hook) => {
            Some(unsafe { mem::transmute::<usize, unsafe extern "system" fn() -> isize>(hook) })
        }
        None => Some(function),
    }
}

/// Patches the plug-in modules that are loaded now. Does nothing outside the VM.
pub fn sweep() {
    if windows_cpus() == 0 {
        return;
    }
    let Some(hooks) = hooks() else {
        return;
    };
    let mut patched = PATCHED.lock().unwrap_or_else(PoisonError::into_inner);
    let mut modules = vec![ptr::null_mut::<c_void>(); 512];
    let mut needed = 0_u32;
    let bytes = u32::try_from(modules.len() * mem::size_of::<HMODULE>()).unwrap_or(u32::MAX);
    // SAFETY: the buffer holds `bytes` bytes of module handles.
    if unsafe {
        K32EnumProcessModules(
            GetCurrentProcess(),
            modules.as_mut_ptr(),
            bytes,
            &raw mut needed,
        )
    } == 0
    {
        return;
    }
    let count = (needed as usize / mem::size_of::<HMODULE>()).min(modules.len());
    // SAFETY: a null name gives the executable's module.
    let worker = unsafe { GetModuleHandleW(ptr::null()) };
    for &module in &modules[..count] {
        if patched.contains(&(module as usize)) {
            continue;
        }
        patched.push(module as usize);
        if module != worker && !is_system(module, &hooks.windows_dir) {
            // SAFETY: a module of this process, loaded while the loader lists it. A sweep
            // runs after a control request, when its loads have ended and their imports
            // are resolved.
            unsafe { patch(module.cast::<u8>(), &hooks.table) };
        }
    }
}

/// True for a module in the Windows directory.
fn is_system(module: HMODULE, windows_dir: &[u16]) -> bool {
    let mut path = vec![0_u16; 1024];
    // SAFETY: the buffer has the length passed.
    let length = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), 1024) } as usize;
    let lower = |units: &[u16]| String::from_utf16_lossy(units).to_lowercase();
    windows_dir.is_empty() || lower(&path[..length.min(1024)]).starts_with(&lower(windows_dir))
}

/// Replaces each import address of `module` that is a hooked function with its hook.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "the loader maps PE headers and import tables aligned"
)]
///
/// # Safety
///
/// `module` must be the base of a loaded PE image in this process.
unsafe fn patch(module: *const u8, table: &[(usize, usize)]) {
    // SAFETY: a loaded image starts with its DOS header; the offsets are the image's own.
    unsafe {
        let dos = &*module.cast::<IMAGE_DOS_HEADER>();
        if dos.e_magic != 0x5A4D {
            return;
        }
        let nt = &*module
            .offset(dos.e_lfanew as isize)
            .cast::<IMAGE_NT_HEADERS64>();
        if nt.Signature != 0x4550 {
            return;
        }
        let imports = nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT as usize];
        if imports.VirtualAddress == 0 {
            return;
        }
        let mut descriptor = module
            .add(imports.VirtualAddress as usize)
            .cast::<IMAGE_IMPORT_DESCRIPTOR>();
        while (*descriptor).Name != 0 {
            let mut slot = module
                .add((*descriptor).FirstThunk as usize)
                .cast::<usize>()
                .cast_mut();
            while *slot != 0 {
                if let Some(&(_, hook)) = table.iter().find(|(real, _)| *real == *slot) {
                    write(slot, hook);
                }
                slot = slot.add(1);
            }
            descriptor = descriptor.add(1);
        }
    }
}

/// Writes one import address, opening its page for the write.
///
/// # Safety
///
/// `slot` must be an import address of a loaded image.
unsafe fn write(slot: *mut usize, value: usize) {
    let mut old: PAGE_PROTECTION_FLAGS = 0;
    // SAFETY: `slot` is in a mapped image page; the protection is restored after the write.
    unsafe {
        if VirtualProtect(
            slot.cast(),
            mem::size_of::<usize>(),
            PAGE_READWRITE,
            &raw mut old,
        ) != 0
        {
            ptr::write_volatile(slot, value);
            VirtualProtect(slot.cast(), mem::size_of::<usize>(), old, &raw mut old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hooks, patch, restrict, set_windows_cpus};
    use windows_sys::Win32::System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{GetCurrentThread, SetThreadAffinityMask},
    };

    /// The test binary's own calls go through its import table, as a plug-in's do.
    #[test]
    fn a_patched_module_cannot_move_a_thread_onto_a_real_time_vcpu() {
        set_windows_cpus(0b01);
        assert_eq!(
            restrict(0b10),
            0b01,
            "only a real-time vCPU: all Windows vCPUs"
        );
        assert_eq!(restrict(0b11), 0b01);
        let hooks = hooks().expect("kernel32 exports");
        // SAFETY: the test executable's module is loaded.
        unsafe { patch(GetModuleHandleW(std::ptr::null()).cast(), &hooks.table) };
        // SAFETY: plain calls on this thread.
        unsafe {
            SetThreadAffinityMask(GetCurrentThread(), 0b10);
            // The real function returns the mask that the hooked call set.
            let previous = (hooks.set_thread_affinity_mask)(GetCurrentThread(), 0b01);
            assert_eq!(previous, 0b01);
        }
    }
}

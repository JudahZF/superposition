//! In Lambda's Windows VM, a worker's threads keep off the real-time vCPUs (Lambda's VM plan,
//! section 06).
//!
//! The guest agent sets the worker's process mask to the Windows vCPUs and the VM port's
//! real-time vCPU, and gives the Windows vCPUs in `SP_VM_WINDOWS_CPUS` (a decimal mask). A
//! thread-attach callback moves each new thread, also each thread a plug-in starts, to the
//! Windows vCPUs. The audio thread then moves itself to the real-time vCPU
//! (`AudioThreadPolicy`). Under Wine the variable is not set and nothing changes. A plug-in
//! can still set its own affinity: this lowers the risk, it does not contain the plug-in.

#![allow(unsafe_code, reason = "a TLS callback and Win32 thread affinity")]

use std::ffi::c_void;

use sp_shared_memory_os::vm_hooks;

use windows_sys::Win32::System::{
    SystemServices::DLL_THREAD_ATTACH,
    Threading::{GetCurrentThread, SetThreadAffinityMask},
};

/// Reads `SP_VM_WINDOWS_CPUS` and moves the calling thread to those vCPUs. Call it first in
/// `main`, before any other thread starts.
pub fn confine() {
    let Some(mask) = std::env::var("SP_VM_WINDOWS_CPUS")
        .ok()
        .and_then(|mask| mask.parse::<usize>().ok())
        .filter(|mask| *mask != 0)
    else {
        return;
    };
    vm_hooks::set_windows_cpus(mask);
    // SAFETY: the pseudo-handle of this thread; a plain mask.
    unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
}

unsafe extern "system" fn on_thread(_module: *mut c_void, reason: u32, _reserved: *mut c_void) {
    if reason != DLL_THREAD_ATTACH {
        return;
    }
    let mask = vm_hooks::windows_cpus();
    if mask != 0 {
        // SAFETY: the pseudo-handle of the new thread; a plain mask.
        unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
    }
}

/// The loader calls each function in the `.CRT$XL*` sections for every thread it starts.
#[used]
#[unsafe(link_section = ".CRT$XLM")]
static ON_THREAD: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = on_thread;

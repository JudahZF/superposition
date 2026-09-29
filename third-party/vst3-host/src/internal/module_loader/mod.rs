//! Platform-specific VST3 module loading implementations

use crate::error::{Error, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, PoisonError},
};
use vst3::{ComPtr, Steinberg::IPluginFactory};

/// Trait representing a loaded VST3 module
pub trait VstModule: Send {
    /// Get the plugin factory from the module
    fn get_factory(&self) -> Result<*mut IPluginFactory>;

    /// Get the path to the module (diagnostics / config record)
    #[allow(dead_code)]
    fn path(&self) -> &Path;
}

/// Trait for platform-specific module loaders
pub trait ModuleLoader {
    /// Load a VST3 module from the given path
    fn load(path: &Path) -> Result<Box<dyn VstModule>>;
}

/// Detect if a plugin requires private namespace loading due to C symbol conflicts.
/// Only consulted by the macOS loader (private namespaces are a dyld feature).
#[cfg(target_os = "macos")]
fn requires_private_namespace(path: &Path) -> bool {
    if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
        let filename_lower = filename.to_lowercase();

        // Plugins with C symbol conflicts (not Objective-C)
        if filename_lower.contains("ozone") ||
           filename_lower.contains("rx") ||      // iZotope RX series
           filename_lower.contains("neutron") || // iZotope Neutron  
           filename_lower.contains("insight")
        // iZotope Insight
        {
            log::info!(
                "Detected plugin requiring private namespace for C symbols: {}",
                filename
            );
            return true;
        }
    }

    false
}

/// Every module loaded by this process, keyed by canonical path.
///
/// A module stays loaded until the process exits, and its `bundleExit` is never called. Many
/// plug-ins crash when one process runs `bundleEntry` → `bundleExit` → unload → reload, and a
/// scan loads each bundle several times. The executable is also shared: unloading it for one
/// instance would unmap the code of another instance of the same plug-in. JUCE hosts cache
/// modules in the same way.
///
/// A factory can keep a caller's `setHostContext` pointer after that caller releases its host
/// application. Every caller sets its own context before it creates a class, so the plug-in
/// does not use the old pointer.
static MODULES: LazyLock<Mutex<HashMap<PathBuf, LoadedModule>>> = LazyLock::new(Mutex::default);

/// A loaded module and the one factory reference it keeps.
///
/// `GetPluginFactory` is called once per module, as the SDK host does. Some plug-ins keep a
/// stale global after their factory is released, so a second call crashes.
struct LoadedModule {
    factory: ComPtr<IPluginFactory>,
    _module: Box<dyn VstModule>,
}

// SAFETY: the cache only clones the factory reference while `MODULES` is locked. Callers own
// and use their own references, as they did before the cache.
unsafe impl Send for LoadedModule {}

/// A handle to a module in [`MODULES`]. Dropping it leaves the module loaded.
struct SharedModule {
    key: PathBuf,
}

impl VstModule for SharedModule {
    /// Returns a new reference to the cached factory. The caller owns and releases it.
    fn get_factory(&self) -> Result<*mut IPluginFactory> {
        let modules = MODULES.lock().unwrap_or_else(PoisonError::into_inner);
        match modules.get(&self.key) {
            Some(module) => Ok(module.factory.clone().into_raw()),
            None => Err(Error::PluginLoadFailed(format!(
                "module {} is not loaded",
                self.key.display()
            ))),
        }
    }

    fn path(&self) -> &Path {
        &self.key
    }
}

/// Load a VST3 module once per process and return a shared handle to it.
pub fn load_module(path: &Path) -> Result<Box<dyn VstModule>> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Holding the lock across the load stops two threads from loading one bundle twice.
    let mut modules = MODULES.lock().unwrap_or_else(PoisonError::into_inner);
    if !modules.contains_key(&key) {
        let module = load_platform_module(path)?;
        // SAFETY: `get_factory` returns a non-null factory with one reference for the caller.
        let factory = unsafe { ComPtr::from_raw(module.get_factory()?) }.ok_or_else(|| {
            Error::PluginLoadFailed("GetPluginFactory returned null".to_string())
        })?;
        modules.insert(
            key.clone(),
            LoadedModule {
                factory,
                _module: module,
            },
        );
    }
    Ok(Box::new(SharedModule { key }))
}

/// Load a VST3 module using the appropriate platform-specific loader
fn load_platform_module(path: &Path) -> Result<Box<dyn VstModule>> {
    #[cfg(target_os = "macos")]
    {
        if requires_private_namespace(path) {
            // Use private namespace loading for both Objective-C and C symbol conflicts
            log::info!(
                "Using private namespace loader for symbol isolation: {}",
                path.display()
            );
            private_namespace::PrivateNamespaceModuleLoader::load(path)
        } else {
            // Use standard CFBundle loader for plugins without conflicts
            log::debug!("Using standard CFBundle loader: {}", path.display());
            macos::MacOSModuleLoader::load(path)
        }
    }

    #[cfg(target_os = "windows")]
    {
        windows::WindowsModuleLoader::load(path)
    }

    #[cfg(target_os = "linux")]
    {
        linux::LinuxModuleLoader::load(path)
    }

    #[cfg(target_os = "android")]
    {
        linux::LinuxModuleLoader::load(path)
    }
}

/// Pure, platform-independent binary-header parsing for architecture-mismatch diagnostics.
///
/// Lives here (always compiled) rather than in the cfg-gated `windows`/`linux` modules so its
/// unit tests run on every CI host, including macOS. The Windows (PE/COFF) and Linux (ELF)
/// loaders call these to turn an opaque "failed to load" into an actionable arch-mismatch
/// message, mirroring the macOS Mach-O diagnostic. Unused on macOS itself, hence the allow.
#[allow(dead_code)]
pub(crate) mod arch {
    /// The architecture this host binary was built for (the plugin must provide it too).
    #[cfg(target_arch = "aarch64")]
    pub(crate) const HOST_ARCH: &str = "arm64";
    #[cfg(target_arch = "x86_64")]
    pub(crate) const HOST_ARCH: &str = "x86_64";
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub(crate) const HOST_ARCH: &str = "unknown";

    // PE COFF machine values (always little-endian in the file).
    pub(crate) const PE_MACHINE_I386: u16 = 0x014c;
    pub(crate) const PE_MACHINE_AMD64: u16 = 0x8664;
    pub(crate) const PE_MACHINE_ARM64: u16 = 0xaa64;

    // ELF e_machine values.
    pub(crate) const ELF_MACHINE_386: u16 = 3;
    pub(crate) const ELF_MACHINE_X86_64: u16 = 62;
    pub(crate) const ELF_MACHINE_AARCH64: u16 = 183;

    /// Parse the COFF machine field of a PE image (a Windows `.dll`/`.vst3` binary).
    ///
    /// Layout: `MZ` at 0, `e_lfanew` (u32 LE) at 0x3C, `PE\0\0` at `e_lfanew`, machine (u16 LE)
    /// at `e_lfanew + 4`. Returns `None` if the magic is wrong or the data is truncated.
    pub(crate) fn detect_pe_machine(data: &[u8]) -> Option<u16> {
        if data.len() < 0x40 || data[0] != b'M' || data[1] != b'Z' {
            return None;
        }
        let e_lfanew =
            u32::from_le_bytes([data[0x3C], data[0x3D], data[0x3E], data[0x3F]]) as usize;
        let machine_off = e_lfanew.checked_add(4)?;
        if machine_off.checked_add(2)? > data.len() {
            return None;
        }
        if &data[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
            return None;
        }
        Some(u16::from_le_bytes([
            data[machine_off],
            data[machine_off + 1],
        ]))
    }

    /// Parse the `e_machine` field of an ELF image (a Linux `.so` binary).
    ///
    /// Honors `EI_DATA` (offset 5: 1 = little-endian, 2 = big-endian) when reading the u16 at
    /// offset 18. Returns `None` on bad magic or truncation.
    pub(crate) fn detect_elf_machine(data: &[u8]) -> Option<u16> {
        if data.len() < 20 || &data[0..4] != b"\x7fELF" {
            return None;
        }
        let bytes = [data[18], data[19]];
        Some(if data[5] == 2 {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    /// Human name for a PE machine value, or `None` if we don't map it.
    pub(crate) fn pe_machine_name(machine: u16) -> Option<&'static str> {
        match machine {
            PE_MACHINE_AMD64 => Some("x86_64"),
            PE_MACHINE_ARM64 => Some("arm64"),
            PE_MACHINE_I386 => Some("x86"),
            _ => None,
        }
    }

    /// Human name for an ELF machine value, or `None` if we don't map it.
    pub(crate) fn elf_machine_name(machine: u16) -> Option<&'static str> {
        match machine {
            ELF_MACHINE_X86_64 => Some("x86_64"),
            ELF_MACHINE_AARCH64 => Some("arm64"),
            ELF_MACHINE_386 => Some("x86"),
            _ => None,
        }
    }

    /// If `binary_arch` is known and differs from the host, an actionable sentence; else `None`.
    ///
    /// Returns `None` when the host arch is unknown (an exotic target we don't map) — we can't
    /// reliably claim a mismatch or suggest a "universal/unknown" build.
    pub(crate) fn mismatch_detail(kind: &str, binary_arch: Option<&str>) -> Option<String> {
        if HOST_ARCH == "unknown" {
            return None;
        }
        match binary_arch {
            Some(arch) if arch != HOST_ARCH => Some(format!(
                "Failed to load {kind}: plugin is {arch}-only but this host is {HOST_ARCH}. \
                 Use a universal/{HOST_ARCH} build of the plugin."
            )),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Minimal PE: MZ, e_lfanew=0x80, PE\0\0 header, machine at +4.
        fn pe_fixture(machine: u16) -> Vec<u8> {
            let mut data = vec![0u8; 0x88];
            data[0] = b'M';
            data[1] = b'Z';
            data[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
            data[0x80..0x84].copy_from_slice(b"PE\0\0");
            data[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
            data
        }

        /// Minimal ELF: magic, EI_DATA endianness at 5, e_machine at 18.
        fn elf_fixture(machine: u16, big_endian: bool) -> Vec<u8> {
            let mut data = vec![0u8; 20];
            data[0..4].copy_from_slice(b"\x7fELF");
            data[5] = if big_endian { 2 } else { 1 };
            let m = if big_endian {
                machine.to_be_bytes()
            } else {
                machine.to_le_bytes()
            };
            data[18..20].copy_from_slice(&m);
            data
        }

        #[test]
        fn pe_machine_detected() {
            assert_eq!(
                detect_pe_machine(&pe_fixture(PE_MACHINE_AMD64)),
                Some(PE_MACHINE_AMD64)
            );
            assert_eq!(
                detect_pe_machine(&pe_fixture(PE_MACHINE_ARM64)),
                Some(PE_MACHINE_ARM64)
            );
            assert_eq!(pe_machine_name(PE_MACHINE_AMD64), Some("x86_64"));
            assert_eq!(pe_machine_name(PE_MACHINE_ARM64), Some("arm64"));
        }

        #[test]
        fn pe_rejects_garbage_and_truncation() {
            assert_eq!(detect_pe_machine(&[0u8; 4]), None);
            assert_eq!(
                detect_pe_machine(b"not a pe file at all..............."),
                None
            );
            let mut short = pe_fixture(PE_MACHINE_AMD64);
            short.truncate(0x82); // cut off the machine field
            assert_eq!(detect_pe_machine(&short), None);
        }

        #[test]
        fn elf_machine_detected_both_endians() {
            assert_eq!(
                detect_elf_machine(&elf_fixture(ELF_MACHINE_X86_64, false)),
                Some(ELF_MACHINE_X86_64)
            );
            assert_eq!(
                detect_elf_machine(&elf_fixture(ELF_MACHINE_AARCH64, false)),
                Some(ELF_MACHINE_AARCH64)
            );
            // Big-endian EI_DATA must be honored.
            assert_eq!(
                detect_elf_machine(&elf_fixture(ELF_MACHINE_AARCH64, true)),
                Some(ELF_MACHINE_AARCH64)
            );
            assert_eq!(elf_machine_name(ELF_MACHINE_X86_64), Some("x86_64"));
            assert_eq!(elf_machine_name(ELF_MACHINE_AARCH64), Some("arm64"));
        }

        #[test]
        fn elf_rejects_garbage() {
            assert_eq!(detect_elf_machine(&[0u8; 8]), None);
            assert_eq!(detect_elf_machine(b"\x7fELF"), None); // too short
        }

        #[test]
        fn mismatch_detail_is_actionable() {
            // A binary whose arch differs from the host yields a message naming both.
            let other = if HOST_ARCH == "arm64" {
                "x86_64"
            } else {
                "arm64"
            };
            let msg =
                mismatch_detail("DLL", Some(other)).expect("mismatch should produce a detail");
            assert!(msg.contains(other), "names the binary arch: {msg}");
            assert!(msg.contains(HOST_ARCH), "names the host arch: {msg}");
            // Matching arch (or unknown) yields nothing.
            assert_eq!(mismatch_detail("DLL", Some(HOST_ARCH)), None);
            assert_eq!(mismatch_detail("DLL", None), None);
        }
    }
}

// Platform-specific modules
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod private_namespace;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod linux;

//! VST3 bundle inspection and helper-only SDK hosting for Superposition.
//!
//! The default API reads only bundle layout and Mach-O, ELF, or PE headers and never loads
//! a plug-in. Enable `--features sdk` inside isolated helper binaries to unlock the
//! [`sdk`] module, which wraps `vst3-host` without exposing SDK types to the main
//! application or engine.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// SDK-free fixed-capacity serial rack contracts and lifecycle orchestration.
pub mod adapter;
#[cfg(all(any(target_os = "linux", windows), feature = "editor-window"))]
mod editor_common;
#[cfg(feature = "editor-window")]
pub mod editor_input;
#[cfg(all(target_os = "macos", feature = "editor-window"))]
pub mod editor_window;
#[cfg(all(windows, feature = "editor-window"))]
pub mod win32_editor_window;
#[cfg(all(target_os = "linux", feature = "editor-window"))]
pub mod x11_editor_window;

#[cfg(all(any(target_os = "linux", windows), feature = "editor-window"))]
pub use editor_common::EditorWindowIdentity;

/// Filesystem location of a VST3 bundle selected for helper-side inspection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3BundlePath(PathBuf);

impl Vst3BundlePath {
    /// Creates a bundle path from a filesystem path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Borrows the underlying bundle path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Metadata discovered from a VST3 bundle without loading it into the app process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3PluginDescriptor {
    /// Vendor-assigned class identifier.
    pub class_id: String,
    /// User-visible plug-in name.
    pub name: String,
    /// User-visible vendor name.
    pub vendor: String,
}

/// CPU architecture advertised by a VST3 bundle executable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vst3Architecture {
    /// 64-bit ARM code (Apple Silicon or `aarch64-linux`) is present.
    Arm64,
    /// Intel 64-bit code is present.
    X86_64,
    /// Both Apple Silicon and Intel 64-bit code are present.
    Universal,
    /// The executable is not a recognised Mach-O, ELF, or PE file or has another CPU type.
    Unknown,
}

impl Vst3Architecture {
    /// Returns whether this architecture includes code the running host executes natively.
    #[must_use]
    pub const fn is_native(self) -> bool {
        if cfg!(target_arch = "aarch64") {
            matches!(self, Self::Arm64 | Self::Universal)
        } else if cfg!(target_arch = "x86_64") {
            matches!(self, Self::X86_64 | Self::Universal)
        } else {
            false
        }
    }

    /// Returns the stable machine-readable architecture name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X86_64 => "x86_64",
            Self::Universal => "universal",
            Self::Unknown => "unknown",
        }
    }
}

/// Validated metadata from a VST3 bundle without loading its executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vst3BundleInfo {
    /// Original bundle path.
    pub path: PathBuf,
    /// Architecture read from the executable's Mach-O, ELF, or PE header.
    pub architectures: Vst3Architecture,
    /// Name of the executable found in `Contents/MacOS`, `Contents/<arch>-linux`, or
    /// `Contents/<arch>-win`, or of a single-file Windows bundle.
    pub executable_name: String,
    /// Whether the bundle includes code this host executes natively.
    pub native: bool,
}

/// Reportable result of a scanning attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    /// The bundle layout is valid and can be scanned on this architecture.
    Supported,
    /// The bundle layout is valid but has no code this host executes natively.
    UnsupportedArchitecture,
    /// The path does not have the expected VST3 bundle layout.
    InvalidBundle,
    /// A future isolated scan did not finish within its deadline.
    TimedOut,
    /// A future isolated scan process terminated unexpectedly.
    Crashed,
}

/// Error returned when a path is not an inspectable VST3 bundle.
#[derive(Debug)]
pub struct Vst3BundleError {
    message: String,
}

impl Vst3BundleError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Vst3BundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Vst3BundleError {}

/// Validates a `.vst3` bundle and probes its executable without loading it.
///
/// # Errors
///
/// Returns an error if the path is not a VST3 bundle or has no executable in its platform
/// layout: `Contents/Info.plist` and `Contents/MacOS` on macOS, a shared object in a
/// `Contents/<arch>-linux` folder on Linux, or on Windows a DLL in a `Contents/<arch>-win`
/// folder or the `.vst3` DLL itself.
pub fn inspect_bundle_layout(path: impl AsRef<Path>) -> Result<Vst3BundleInfo, Vst3BundleError> {
    let path = path.as_ref();
    if path.extension().and_then(|extension| extension.to_str()) != Some("vst3") {
        return Err(Vst3BundleError::invalid("bundle path must end in .vst3"));
    }
    let (executable_name, architectures) = probe_bundle_executable(path)?;
    Ok(Vst3BundleInfo {
        path: path.to_path_buf(),
        architectures,
        executable_name,
        native: architectures.is_native(),
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn probe_bundle_executable(path: &Path) -> Result<(String, Vst3Architecture), Vst3BundleError> {
    let contents = path.join("Contents");
    let info_plist = contents.join("Info.plist");
    if !info_plist.is_file() {
        return Err(Vst3BundleError::invalid(format!(
            "missing {}",
            info_plist.display()
        )));
    }

    let macos = contents.join("MacOS");
    let entries = fs::read_dir(&macos).map_err(|error| {
        Vst3BundleError::invalid(format!("cannot read {}: {error}", macos.display()))
    })?;
    let mut executables = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            Vst3BundleError::invalid(format!("cannot read {}: {error}", macos.display()))
        })?
        .into_iter()
        .filter(|entry| entry.path().is_file())
        .collect::<Vec<_>>();
    executables.sort_by_key(fs::DirEntry::file_name);
    let executable = executables.into_iter().next().ok_or_else(|| {
        Vst3BundleError::invalid(format!("missing executable in {}", macos.display()))
    })?;
    let executable_name = executable.file_name().to_string_lossy().into_owned();
    let bytes = fs::read(executable.path()).map_err(|error| {
        Vst3BundleError::invalid(format!(
            "cannot read {}: {error}",
            executable.path().display()
        ))
    })?;
    Ok((executable_name, probe_macho_architecture(&bytes)))
}

/// `Contents/` folder that holds code the running Linux host loads.
#[cfg(target_os = "linux")]
const NATIVE_LINUX_FOLDER: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-linux"
} else if cfg!(target_arch = "x86") {
    "i386-linux"
} else {
    "x86_64-linux"
};

/// Finds the shared object in a `Contents/<arch>-linux` folder and reads its ELF header. The
/// native folder comes first, so a multi-architecture bundle reports the binary this host loads.
#[cfg(target_os = "linux")]
fn probe_bundle_executable(path: &Path) -> Result<(String, Vst3Architecture), Vst3BundleError> {
    use std::io::Read as _;

    let contents = path.join("Contents");
    let entries = fs::read_dir(&contents).map_err(|error| {
        Vst3BundleError::invalid(format!("cannot read {}: {error}", contents.display()))
    })?;
    let mut folders = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().ends_with("-linux") && entry.path().is_dir()
        })
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    folders.sort_by_key(|name| (name.as_os_str() != NATIVE_LINUX_FOLDER, name.clone()));
    let named = path
        .file_stem()
        .map(|stem| Path::new(stem).with_extension("so"));
    let executable = folders
        .iter()
        .find_map(|folder| bundle_binary(&contents.join(folder), named.as_deref(), "so"))
        .ok_or_else(|| {
            Vst3BundleError::invalid(format!(
                "missing shared object in {}/*-linux",
                contents.display()
            ))
        })?;
    let mut header = Vec::with_capacity(64);
    fs::File::open(&executable)
        .and_then(|file| file.take(64).read_to_end(&mut header))
        .map_err(|error| {
            Vst3BundleError::invalid(format!("cannot read {}: {error}", executable.display()))
        })?;
    let executable_name = executable
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    Ok((executable_name, probe_elf_architecture(&header)))
}

/// `Contents/` folder that holds code the running Windows host loads.
#[cfg(windows)]
const NATIVE_WINDOWS_FOLDER: &str = if cfg!(target_arch = "aarch64") {
    "arm64-win"
} else if cfg!(target_arch = "x86") {
    "x86-win"
} else {
    "x86_64-win"
};

/// Finds the DLL in a `Contents/<arch>-win` folder, native folder first, or takes a single-file
/// `.vst3` DLL as it is, then reads its PE header.
#[cfg(windows)]
fn probe_bundle_executable(path: &Path) -> Result<(String, Vst3Architecture), Vst3BundleError> {
    use std::io::Read as _;

    let executable = if path.is_file() {
        path.to_path_buf()
    } else {
        let contents = path.join("Contents");
        let entries = fs::read_dir(&contents).map_err(|error| {
            Vst3BundleError::invalid(format!("cannot read {}: {error}", contents.display()))
        })?;
        let mut folders = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_name().to_string_lossy().ends_with("-win") && entry.path().is_dir()
            })
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        folders.sort_by_key(|name| (name.as_os_str() != NATIVE_WINDOWS_FOLDER, name.clone()));
        let named = path.file_name().map(Path::new);
        folders
            .iter()
            .find_map(|folder| bundle_binary(&contents.join(folder), named, "vst3"))
            .ok_or_else(|| {
                Vst3BundleError::invalid(format!("missing DLL in {}/*-win", contents.display()))
            })?
    };
    // The PE header offset sits in the DOS header; real DLLs place the header well inside this.
    let mut header = Vec::with_capacity(4096);
    fs::File::open(&executable)
        .and_then(|file| file.take(4096).read_to_end(&mut header))
        .map_err(|error| {
            Vst3BundleError::invalid(format!("cannot read {}: {error}", executable.display()))
        })?;
    let executable_name = executable
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    Ok((executable_name, probe_pe_architecture(&header)))
}

/// Reads a PE (Windows DLL) header and classifies its machine.
///
/// This function only examines the supplied bytes; it never executes or loads them.
#[must_use]
pub fn probe_pe_architecture(bytes: &[u8]) -> Vst3Architecture {
    const AMD64: u16 = 0x8664;
    const ARM64: u16 = 0xaa64;

    if bytes.get(..2) != Some(b"MZ") {
        return Vst3Architecture::Unknown;
    }
    let Some(offset) = read_u32(bytes, 0x3c, true).and_then(|offset| usize::try_from(offset).ok())
    else {
        return Vst3Architecture::Unknown;
    };
    if bytes.get(offset..offset.saturating_add(4)) != Some(b"PE\0\0") {
        return Vst3Architecture::Unknown;
    }
    match bytes
        .get(offset + 4..offset + 6)
        .map(|machine| u16::from_le_bytes([machine[0], machine[1]]))
    {
        Some(AMD64) => Vst3Architecture::X86_64,
        Some(ARM64) => Vst3Architecture::Arm64,
        _ => Vst3Architecture::Unknown,
    }
}

/// The binary named after the bundle, as the VST3 layout requires, or else the first file with
/// `extension` by name.
#[cfg(any(target_os = "linux", windows))]
fn bundle_binary(folder: &Path, named: Option<&Path>, extension: &str) -> Option<PathBuf> {
    if let Some(named) = named.map(|name| folder.join(name))
        && named.is_file()
    {
        return Some(named);
    }
    let mut objects = fs::read_dir(folder)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|found| found.to_str())
                .is_some_and(|found| found.eq_ignore_ascii_case(extension))
                && path.is_file()
        })
        .collect::<Vec<_>>();
    objects.sort();
    objects.into_iter().next()
}

/// Reads an ELF header and classifies its machine.
///
/// This function only examines the supplied bytes; it never executes or loads them.
#[must_use]
pub fn probe_elf_architecture(bytes: &[u8]) -> Vst3Architecture {
    const X86_64: u16 = 62;
    const AARCH64: u16 = 183;

    let little_endian = match bytes.get(..6) {
        Some([0x7f, b'E', b'L', b'F', 1 | 2, 1]) => true,
        Some([0x7f, b'E', b'L', b'F', 1 | 2, 2]) => false,
        _ => return Vst3Architecture::Unknown,
    };
    let Some(&[low, high]) = bytes.get(18..20) else {
        return Vst3Architecture::Unknown;
    };
    let machine = if little_endian {
        u16::from_le_bytes([low, high])
    } else {
        u16::from_be_bytes([low, high])
    };
    match machine {
        X86_64 => Vst3Architecture::X86_64,
        AARCH64 => Vst3Architecture::Arm64,
        _ => Vst3Architecture::Unknown,
    }
}

/// Reads a Mach-O or fat Mach-O header and classifies its CPU architecture.
///
/// This function only examines the supplied bytes; it never executes or loads them.
#[must_use]
pub fn probe_macho_architecture(bytes: &[u8]) -> Vst3Architecture {
    const ARM64: u32 = 0x0100_000c;
    const X86_64: u32 = 0x0100_0007;

    let cpu = |offset, little_endian| read_u32(bytes, offset, little_endian);
    let architectures = match bytes.get(..4) {
        Some([0xcf | 0xce, 0xfa, 0xed, 0xfe]) => cpu(4, true).into_iter().collect(),
        Some([0xfe, 0xed, 0xfa, 0xcf | 0xce]) => cpu(4, false).into_iter().collect(),
        Some([0xca, 0xfe, 0xba, 0xbe]) => fat_cpu_types(bytes, false, 20),
        Some([0xbe, 0xba, 0xfe, 0xca]) => fat_cpu_types(bytes, true, 20),
        Some([0xca, 0xfe, 0xba, 0xbf]) => fat_cpu_types(bytes, false, 32),
        Some([0xbf, 0xba, 0xfe, 0xca]) => fat_cpu_types(bytes, true, 32),
        _ => Vec::new(),
    };

    let arm64 = architectures.contains(&ARM64);
    let x86_64 = architectures.contains(&X86_64);
    match (arm64, x86_64) {
        (true, true) => Vst3Architecture::Universal,
        (true, false) => Vst3Architecture::Arm64,
        (false, true) => Vst3Architecture::X86_64,
        (false, false) => Vst3Architecture::Unknown,
    }
}

fn fat_cpu_types(bytes: &[u8], little_endian: bool, entry_size: usize) -> Vec<u32> {
    // Fat Mach-O headers are untrusted bundle metadata. Never use their declared architecture
    // count as an iteration/allocation bound: a truncated header could otherwise make scanning
    // spend arbitrary time walking offsets that cannot exist.
    const MAX_FAT_ARCHITECTURES: usize = 64;
    let Some(count) = read_u32(bytes, 4, little_endian) else {
        return Vec::new();
    };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    if entry_size < 4 {
        return Vec::new();
    }
    let Some(available) = bytes.len().checked_sub(8) else {
        return Vec::new();
    };
    let complete_entries = available / entry_size;
    if count > MAX_FAT_ARCHITECTURES || count > complete_entries {
        return Vec::new();
    }
    let Some(header_end) = count
        .checked_mul(entry_size)
        .and_then(|entries| 8usize.checked_add(entries))
    else {
        return Vec::new();
    };
    if header_end > bytes.len() {
        return Vec::new();
    }

    let mut architectures = Vec::with_capacity(count);
    for index in 0..count {
        let Some(offset) = index
            .checked_mul(entry_size)
            .and_then(|entry| 8usize.checked_add(entry))
        else {
            return Vec::new();
        };
        let Some(cpu) = read_u32(bytes, offset, little_endian) else {
            return Vec::new();
        };
        architectures.push(cpu);
    }
    architectures
}

fn read_u32(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

/// Helper-side contract for reading VST3 bundle metadata.
pub trait Vst3Inspector {
    /// Inspects a bundle without exposing SDK types to callers.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle cannot be inspected safely.
    fn inspect(
        &self,
        bundle: &Vst3BundlePath,
    ) -> Result<Vec<Vst3PluginDescriptor>, Box<dyn std::error::Error + Send + Sync>>;
}

/// Deterministic inspector for tests that must not inspect a real bundle.
#[derive(Clone, Debug, Default)]
pub struct FakeVst3Inspector;

impl Vst3Inspector for FakeVst3Inspector {
    fn inspect(
        &self,
        _bundle: &Vst3BundlePath,
    ) -> Result<Vec<Vst3PluginDescriptor>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![Vst3PluginDescriptor {
            class_id: "FAKE-VST3-CLASS-ID".to_owned(),
            name: "Fake VST3".to_owned(),
            vendor: "Superposition Tests".to_owned(),
        }])
    }
}

/// Isolated VST3 SDK hosting boundary (`vst3-host`).
///
/// Enabled with `--features sdk`. Helper-only: the application process must never
/// enable this feature.
#[cfg(feature = "sdk")]
pub mod sdk;

/// Official native VST3 SDK backend with a C++ ABI containment shim.
#[cfg(feature = "native-sdk")]
pub mod native;

#[cfg(test)]
mod tests {
    use super::{
        FakeVst3Inspector, Vst3Architecture, Vst3BundlePath, Vst3Inspector, probe_elf_architecture,
        probe_macho_architecture, probe_pe_architecture,
    };

    #[test]
    fn probes_little_endian_arm64_macho() {
        let mut fixture = vec![0xcf, 0xfa, 0xed, 0xfe];
        fixture.extend(0x0100_000cu32.to_le_bytes());

        assert_eq!(probe_macho_architecture(&fixture), Vst3Architecture::Arm64);
    }

    #[test]
    fn probes_big_endian_fat_universal_macho() {
        let mut fixture = vec![0xca, 0xfe, 0xba, 0xbe];
        fixture.extend(2u32.to_be_bytes());
        fixture.extend(0x0100_0007u32.to_be_bytes());
        fixture.extend([0; 16]);
        fixture.extend(0x0100_000cu32.to_be_bytes());
        fixture.extend([0; 16]);

        assert_eq!(
            probe_macho_architecture(&fixture),
            Vst3Architecture::Universal
        );
    }

    #[test]
    fn rejects_truncated_fat_header_count_without_iterating_missing_entries() {
        let mut fixture = vec![0xca, 0xfe, 0xba, 0xbe];
        fixture.extend(2u32.to_be_bytes());
        fixture.extend(0x0100_000cu32.to_be_bytes());
        fixture.extend([0; 16]);

        assert_eq!(
            probe_macho_architecture(&fixture),
            Vst3Architecture::Unknown,
            "a declared second architecture without its complete table entry is invalid"
        );
    }

    #[test]
    fn rejects_unreasonable_fat_header_count_before_looping() {
        let mut fixture = vec![0xca, 0xfe, 0xba, 0xbe];
        fixture.extend(u32::MAX.to_be_bytes());

        assert_eq!(
            probe_macho_architecture(&fixture),
            Vst3Architecture::Unknown,
            "untrusted count must not become an unbounded iteration limit"
        );
    }

    #[test]
    fn reports_unknown_for_non_macho_bytes() {
        assert_eq!(
            probe_macho_architecture(b"not a Mach-O"),
            Vst3Architecture::Unknown
        );
    }

    /// The first 20 bytes of an ELF header: identification, type, then machine.
    fn elf_header(class: u8, little_endian: bool, machine: u16) -> Vec<u8> {
        let mut header = vec![
            0x7f,
            b'E',
            b'L',
            b'F',
            class,
            if little_endian { 1 } else { 2 },
        ];
        header.resize(16, 0);
        let (kind, machine) = if little_endian {
            (3_u16.to_le_bytes(), machine.to_le_bytes())
        } else {
            (3_u16.to_be_bytes(), machine.to_be_bytes())
        };
        header.extend(kind);
        header.extend(machine);
        header
    }

    #[test]
    fn probes_elf_machine_in_either_byte_order() {
        assert_eq!(
            probe_elf_architecture(&elf_header(2, true, 62)),
            Vst3Architecture::X86_64
        );
        assert_eq!(
            probe_elf_architecture(&elf_header(2, false, 183)),
            Vst3Architecture::Arm64
        );
        assert_eq!(
            probe_elf_architecture(&elf_header(1, true, 3)),
            Vst3Architecture::Unknown,
            "32-bit x86 is not a supported plug-in architecture"
        );
    }

    #[test]
    fn rejects_truncated_or_non_elf_headers() {
        let header = elf_header(2, true, 62);
        assert_eq!(
            probe_elf_architecture(&header[..19]),
            Vst3Architecture::Unknown
        );
        assert_eq!(
            probe_elf_architecture(b"not an ELF object at all"),
            Vst3Architecture::Unknown
        );
    }

    #[test]
    fn probes_pe_machine_and_rejects_other_files() {
        let pe = |machine: u16| {
            let mut header = vec![0; 0x48];
            header[..2].copy_from_slice(b"MZ");
            header[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
            header[0x40..0x44].copy_from_slice(b"PE\0\0");
            header[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
            header
        };
        assert_eq!(probe_pe_architecture(&pe(0x8664)), Vst3Architecture::X86_64);
        assert_eq!(probe_pe_architecture(&pe(0xaa64)), Vst3Architecture::Arm64);
        assert_eq!(
            probe_pe_architecture(&pe(0x014c)),
            Vst3Architecture::Unknown
        );
        assert_eq!(
            probe_pe_architecture(&pe(0x8664)[..0x44]),
            Vst3Architecture::Unknown
        );
        assert_eq!(
            probe_pe_architecture(&elf_header(2, true, 62)),
            Vst3Architecture::Unknown
        );
    }

    /// A bundle with code for another architecture sorted first reports the host's binary.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn linux_layout_prefers_the_native_architecture_folder() {
        let root = std::env::temp_dir().join(format!("sp-vst3-layout-{}", std::process::id()));
        let bundle = root.join("Multi.vst3");
        for (folder, machine) in [("aarch64-linux", 183), ("x86_64-linux", 62)] {
            let folder = bundle.join("Contents").join(folder);
            std::fs::create_dir_all(&folder).expect("create architecture folder");
            std::fs::write(folder.join("Multi.so"), elf_header(2, true, machine))
                .expect("write shared object");
        }

        let info = super::inspect_bundle_layout(&bundle).expect("valid Linux bundle");

        assert_eq!(info.architectures, Vst3Architecture::X86_64);
        assert_eq!(info.executable_name, "Multi.so");
        assert!(info.native);
        std::fs::remove_dir_all(root).expect("remove test bundle");
    }

    #[test]
    fn fake_inspector_returns_a_deterministic_descriptor() {
        let descriptors = FakeVst3Inspector
            .inspect(&Vst3BundlePath::new("unused.vst3"))
            .expect("fake inspection should succeed");

        assert_eq!(descriptors[0].class_id, "FAKE-VST3-CLASS-ID");
    }
}

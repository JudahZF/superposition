//! VST3 bundle inspection and helper-only SDK hosting for Superposition.
//!
//! The default API reads only bundle layout and Mach-O headers and never loads a
//! plug-in. Enable `--features sdk` inside isolated helper binaries to unlock the
//! [`sdk`] module, which wraps `vst3-host` without exposing SDK types to the main
//! application or engine.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// SDK-free fixed-capacity serial rack contracts and lifecycle orchestration.
pub mod adapter;
#[cfg(all(target_os = "macos", feature = "editor-window"))]
pub mod editor_window;

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
    /// Native Apple Silicon code is present.
    Arm64,
    /// Intel 64-bit code is present.
    X86_64,
    /// Both Apple Silicon and Intel 64-bit code are present.
    Universal,
    /// The executable is not a recognised Mach-O file or has another CPU type.
    Unknown,
}

impl Vst3Architecture {
    /// Returns whether this architecture includes native Apple Silicon code.
    #[must_use]
    pub const fn supports_apple_silicon(self) -> bool {
        matches!(self, Self::Arm64 | Self::Universal)
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
    /// Architecture read from the executable's Mach-O header.
    pub architectures: Vst3Architecture,
    /// Name of the executable found in `Contents/MacOS`.
    pub executable_name: String,
    /// Whether the bundle includes native Apple Silicon code.
    pub supported_on_apple_silicon: bool,
}

/// Reportable result of a scanning attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    /// The bundle layout is valid and can be scanned on this architecture.
    Supported,
    /// The bundle layout is valid but has no native Apple Silicon code.
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
/// Returns an error if the path is not a VST3 bundle, has no `Info.plist`, or
/// has no regular executable file in `Contents/MacOS`.
pub fn inspect_bundle_layout(path: impl AsRef<Path>) -> Result<Vst3BundleInfo, Vst3BundleError> {
    let path = path.as_ref();
    if path.extension().and_then(|extension| extension.to_str()) != Some("vst3") {
        return Err(Vst3BundleError::invalid("bundle path must end in .vst3"));
    }

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
    let architectures = probe_macho_architecture(&bytes);

    Ok(Vst3BundleInfo {
        path: path.to_path_buf(),
        architectures,
        executable_name,
        supported_on_apple_silicon: architectures.supports_apple_silicon(),
    })
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
        FakeVst3Inspector, Vst3Architecture, Vst3BundlePath, Vst3Inspector,
        probe_macho_architecture,
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

    #[test]
    fn fake_inspector_returns_a_deterministic_descriptor() {
        let descriptors = FakeVst3Inspector
            .inspect(&Vst3BundlePath::new("unused.vst3"))
            .expect("fake inspection should succeed");

        assert_eq!(descriptors[0].class_id, "FAKE-VST3-CLASS-ID");
    }
}

//! Parent-supervised isolated VST3 scanning and fingerprint invalidation.
//!
//! The parent only discovers canonical bundle paths, fingerprints bytes, and launches one
//! disposable scanner helper per bundle. Layout inspection and all SDK activity stay inside the
//! helper, so the application process never loads third-party plug-in code. On Linux, Windows
//! bundles in the shared Wine prefix are scanned the same way by the Windows build of the
//! scanner under Wine, and the catalog records which platform hosts each plug-in.

use std::{
    env,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sp_model::{
    PLUGIN_SCAN_METADATA_VERSION, PluginArchitecture, PluginClassScanMetadata, PluginScanMetadata,
    PluginScanOutcome,
};

use sp_wine::{WineEnvironment, WineLauncher};

use crate::{
    HelperKind, HelperLaunch, ProcessSupervisor, TimedHelperResult,
    catalog::PluginCatalog,
    quarantine::{PersistentQuarantine, PluginFailureKind},
};

/// The bounded scanner deadline used when a deployment does not provide one.
pub const DEFAULT_SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// A Windows scan also starts a Wine process, so it gets a longer deadline.
const WINDOWS_SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SCAN_REPORT_BYTES: u64 = 4 * 1024 * 1024;
static NEXT_REPORT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Stable content identity used for catalog invalidation and quarantine.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BundleFingerprint {
    /// Hash algorithm used for `digest`.
    pub algorithm: String,
    /// Lowercase content digest.
    pub digest: String,
}

/// Compatibility alias for SDK-free per-class scan metadata.
pub type ScanDescriptor = PluginClassScanMetadata;
/// Compatibility alias for the platform-neutral isolated scan outcome.
pub type ScanStatus = PluginScanOutcome;
/// Compatibility alias for the persistent, versioned plug-in catalog.
pub type ScanCache = PluginCatalog;

/// Which build of the worker hosts a plug-in.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginPlatform {
    /// The host's own platform: a macOS or Linux bundle.
    #[default]
    Native,
    /// A Windows bundle, hosted by the Windows worker under Wine on Linux.
    Windows,
}

/// Persisted outcome for one exact bundle fingerprint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CachedScan {
    /// Canonical bundle path.
    pub bundle: PathBuf,
    /// Fingerprint that produced this result.
    pub fingerprint: BundleFingerprint,
    /// Which worker build hosts the bundle. Catalogs from before Windows support are native.
    #[serde(default)]
    pub platform: PluginPlatform,
    /// Versioned, SDK-free metadata produced by the disposable helper.
    pub metadata: PluginScanMetadata,
}

impl CachedScan {
    /// Returns the outcome recorded by the isolated scanner.
    #[must_use]
    pub const fn outcome(&self) -> PluginScanOutcome {
        self.metadata.outcome
    }

    /// Returns whether this exact bundle has code this host runs natively.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.metadata.outcome == PluginScanOutcome::Supported
            && match self.metadata.architecture {
                PluginArchitecture::Universal => true,
                PluginArchitecture::Arm64 => cfg!(target_arch = "aarch64"),
                PluginArchitecture::X86_64 => cfg!(target_arch = "x86_64"),
                PluginArchitecture::Unknown => false,
            }
    }
}

/// Scanner executable, timeout policy, persistent catalog, and optional persistent quarantine.
#[derive(Debug)]
pub struct Scanner {
    executable: PathBuf,
    timeout: Duration,
    catalog: PluginCatalog,
    quarantine: Option<PersistentQuarantine>,
    windows: Option<WindowsScanner>,
}

/// The Windows build of the scanner and how it runs under Wine.
#[derive(Debug)]
struct WindowsScanner {
    executable: PathBuf,
    wine: WineLauncher,
    /// Plug-ins may open dialogs while they load; those go to the private display.
    #[cfg(target_os = "linux")]
    display: Option<crate::DisplayEnvironment>,
}

impl Scanner {
    /// Creates a cached isolated scanner without scanner-failure quarantine ingestion.
    ///
    /// Production application code should call [`Self::with_quarantine`] before scanning.
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>, timeout: Duration, catalog: PluginCatalog) -> Self {
        Self {
            executable: executable.into(),
            timeout,
            catalog,
            quarantine: None,
            windows: None,
        }
    }

    /// Creates a scanner with the default ten-second helper deadline.
    #[must_use]
    pub fn with_default_timeout(executable: impl Into<PathBuf>, catalog: PluginCatalog) -> Self {
        Self::new(executable, DEFAULT_SCAN_TIMEOUT, catalog)
    }

    /// Adds persistent scanner-failure quarantine ingestion.
    #[must_use]
    pub fn with_quarantine(mut self, quarantine: PersistentQuarantine) -> Self {
        self.quarantine = Some(quarantine);
        self
    }

    /// Also scans Windows bundles in `wine`'s VST3 folders, with `executable` (the Windows build
    /// of the scanner) under Wine. The caller keeps the prefix prepared while it scans.
    #[must_use]
    pub fn with_windows_scanner(
        mut self,
        executable: impl Into<PathBuf>,
        wine: WineLauncher,
    ) -> Self {
        self.windows = Some(WindowsScanner {
            executable: executable.into(),
            wine,
            #[cfg(target_os = "linux")]
            display: None,
        });
        self
    }

    /// Gives Windows scans the private display, or none.
    #[cfg(target_os = "linux")]
    pub fn set_windows_display(&mut self, display: Option<crate::DisplayEnvironment>) {
        if let Some(windows) = self.windows.as_mut() {
            windows.display = display;
        }
    }

    /// Whether Windows bundles are scanned.
    #[must_use]
    pub const fn scans_windows(&self) -> bool {
        self.windows.is_some()
    }

    /// Returns the persistent catalog.
    #[must_use]
    pub const fn catalog(&self) -> &PluginCatalog {
        &self.catalog
    }

    /// Compatibility accessor for callers that still use the older scan-cache name.
    #[must_use]
    pub const fn cache(&self) -> &ScanCache {
        &self.catalog
    }

    /// Returns persistent quarantine state when scanner fault ingestion is configured.
    #[must_use]
    pub fn quarantine(&self) -> Option<&PersistentQuarantine> {
        self.quarantine.as_ref()
    }

    /// Borrows the shared failure history for worker failures and explicit per-plug-in retries.
    pub fn quarantine_mut(&mut self) -> Option<&mut PersistentQuarantine> {
        self.quarantine.as_mut()
    }

    /// Fingerprints and scans one bundle, reusing an unchanged catalog result.
    ///
    /// The caller must use [`Self::rescan`] for an explicit user-requested retry.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle cannot be fingerprinted or persistent state cannot be
    /// updated. A scanner launch failure is cached as a `crashed` outcome instead.
    pub fn scan(&mut self, bundle: &Path) -> std::io::Result<CachedScan> {
        self.scan_inner(bundle, PluginPlatform::Native, false)
    }

    /// Like [`Self::scan`] for a Windows bundle, with the Windows scanner under Wine.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` without a Windows scanner, or the errors of [`Self::scan`].
    pub fn scan_windows(&mut self, bundle: &Path) -> std::io::Result<CachedScan> {
        self.scan_inner(bundle, PluginPlatform::Windows, false)
    }

    /// Invalidates a selected catalog entry and launches a fresh isolated scan.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle cannot be fingerprinted or state cannot be persisted.
    pub fn rescan(&mut self, bundle: &Path) -> std::io::Result<CachedScan> {
        let platform = fs::canonicalize(bundle)
            .ok()
            .and_then(|bundle| self.catalog.get_by_path(&bundle).map(|scan| scan.platform))
            .unwrap_or_default();
        self.scan_inner(bundle, platform, true)
    }

    /// Discovers standard VST3 directories, and with a Windows scanner the prefix's Windows VST3
    /// folders, and scans each bundle independently.
    ///
    /// # Errors
    ///
    /// Returns the first persistence/fingerprinting error after prior discovered bundles have
    /// already completed their own helper invocation.
    pub fn scan_standard_locations(
        &mut self,
        home: Option<&Path>,
    ) -> std::io::Result<Vec<CachedScan>> {
        let windows = self
            .windows
            .as_ref()
            .map(|windows| discover_windows_vst3_bundles(&windows.wine.vst3_dirs()))
            .unwrap_or_default();
        let native = discover_vst3_bundles(home)
            .into_iter()
            .map(|bundle| (bundle, PluginPlatform::Native));
        native
            .chain(
                windows
                    .into_iter()
                    .map(|bundle| (bundle, PluginPlatform::Windows)),
            )
            .map(|(bundle, platform)| self.scan_inner(&bundle, platform, false))
            .collect()
    }

    /// Clears every catalog result and atomically persists the empty catalog for a manual reset.
    ///
    /// # Errors
    ///
    /// Returns an error when the updated catalog cannot be persisted.
    pub fn clear_catalog(&mut self) -> std::io::Result<()> {
        self.catalog.clear_and_save()
    }

    /// Clears failure history and quarantine for one fingerprint after an explicit user review.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if this scanner was not configured with persistent quarantine, or
    /// an error when the atomic replacement cannot be persisted.
    pub fn clear_quarantine(&mut self, fingerprint: &BundleFingerprint) -> std::io::Result<()> {
        let Some(quarantine) = self.quarantine.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "scanner has no configured persistent quarantine",
            ));
        };
        quarantine.clear(fingerprint)
    }

    /// Clears all failure history and quarantines after an explicit user-requested reset.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if this scanner was not configured with persistent quarantine, or
    /// an error when the atomic replacement cannot be persisted.
    pub fn clear_all_quarantine(&mut self) -> std::io::Result<()> {
        let Some(quarantine) = self.quarantine.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "scanner has no configured persistent quarantine",
            ));
        };
        quarantine.clear_all()
    }

    fn scan_inner(
        &mut self,
        bundle: &Path,
        platform: PluginPlatform,
        force: bool,
    ) -> std::io::Result<CachedScan> {
        let bundle = fs::canonicalize(bundle)?;
        let fingerprint = fingerprint_bundle(&bundle)?;
        if !force
            && let Some(cached) = self.catalog.get(&bundle, &fingerprint)
            && cached.metadata.version == PLUGIN_SCAN_METADATA_VERSION
            && cached.platform == platform
        {
            return Ok(cached.clone());
        }

        let report_path = TemporaryScanReport::create()?;
        // The helper names the bundle in its report the way it received it.
        let path_argument = |path: &Path| match platform {
            PluginPlatform::Native => path.display().to_string(),
            PluginPlatform::Windows => WineEnvironment::windows_path(path),
        };
        let reported_bundle = PathBuf::from(path_argument(&bundle));
        let arguments = vec![
            "--bundle".to_owned(),
            path_argument(&bundle),
            "--sdk-enumerate".to_owned(),
            "--report-path".to_owned(),
            path_argument(&report_path.path()),
        ];
        #[allow(unused_mut, reason = "only Linux gives Windows scans a display")]
        let mut processes = ProcessSupervisor::new();
        let (launch, timeout) = match platform {
            PluginPlatform::Native => (
                HelperLaunch::native(HelperKind::PluginScanner, &self.executable, arguments),
                self.timeout,
            ),
            PluginPlatform::Windows => {
                let windows = self.windows.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Windows bundles need the Windows scanner and Wine",
                    )
                })?;
                #[cfg(target_os = "linux")]
                processes.set_display(windows.display.clone());
                (
                    HelperLaunch {
                        kind: HelperKind::PluginScanner,
                        executable: windows.executable.clone(),
                        arguments,
                        wine: Some(windows.wine.clone()),
                    },
                    self.timeout.max(WINDOWS_SCAN_TIMEOUT),
                )
            }
        };
        let mut scan = match processes.launch_and_wait_silenced(&launch, timeout) {
            Ok(result) => {
                let report = match &result {
                    TimedHelperResult::TimedOut { .. } => None,
                    TimedHelperResult::Exited { .. } => Some(report_path.read_bounded()),
                };
                map_scan(bundle, &reported_bundle, fingerprint, &result, report)
            }
            Err(error) => CachedScan {
                bundle,
                fingerprint,
                platform,
                metadata: metadata_with_detail(
                    PluginArchitecture::Unknown,
                    PluginScanOutcome::Crashed,
                    format!("could not launch scanner helper: {error}"),
                ),
            },
        };
        scan.platform = platform;
        self.catalog.insert(scan.clone());
        self.catalog.save()?;
        self.ingest_scan_failure(&scan)?;
        Ok(scan)
    }

    fn ingest_scan_failure(&mut self, scan: &CachedScan) -> std::io::Result<()> {
        let Some(quarantine) = self.quarantine.as_mut() else {
            return Ok(());
        };
        let kind = match scan.metadata.outcome {
            PluginScanOutcome::TimedOut => Some(PluginFailureKind::ScannerTimeout),
            PluginScanOutcome::Crashed => Some(PluginFailureKind::ScannerCrash),
            PluginScanOutcome::Supported
            | PluginScanOutcome::UnsupportedArchitecture
            | PluginScanOutcome::InvalidBundle
            | PluginScanOutcome::SdkError
            | PluginScanOutcome::InvalidReport => None,
        };
        if let Some(kind) = kind {
            quarantine.record_failure(scan.fingerprint.clone(), kind)?;
        }
        Ok(())
    }
}

/// A unique per-scan directory keeps helper output separate from plug-in stdout.
struct TemporaryScanReport {
    directory: PathBuf,
}

impl TemporaryScanReport {
    fn create() -> io::Result<Self> {
        for _ in 0..16 {
            let sequence = NEXT_REPORT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let directory = env::temp_dir().join(format!(
                "sp-plugin-scan-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&directory) {
                Ok(()) => return Ok(Self { directory }),
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                Err(_) => {}
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique scanner report directory",
        ))
    }

    fn path(&self) -> PathBuf {
        self.directory.join("report.json")
    }

    fn read_bounded(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(self.path())?
            .take(MAX_SCAN_REPORT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SCAN_REPORT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner report exceeds size limit",
            ));
        }
        Ok(bytes)
    }
}

impl Drop for TemporaryScanReport {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path());
        let _ = fs::remove_dir(&self.directory);
    }
}

/// Computes a deterministic SHA-256 over canonical bundle metadata and regular-file content.
///
/// Symlinked descendants are not followed, preventing bundle-local cycles or content outside the
/// canonical bundle root from affecting identity. Every regular file contributes relative path,
/// size, modification time, and bytes, including `Info.plist` and executable content.
///
/// # Errors
///
/// Returns an error when the bundle cannot be traversed or read.
pub fn fingerprint_bundle(bundle: &Path) -> std::io::Result<BundleFingerprint> {
    let bundle = fs::canonicalize(bundle)?;
    let mut files = Vec::new();
    collect_regular_files(&bundle, &mut files)?;
    files.sort();

    let root_metadata = fs::metadata(&bundle)?;
    let mut hasher = Sha256::new();
    hasher.update(bundle.as_os_str().as_encoded_bytes());
    hasher.update(root_metadata.len().to_le_bytes());
    update_modified_time(&mut hasher, &root_metadata);
    for path in files {
        let relative = path.strip_prefix(&bundle).unwrap_or(&path);
        let metadata = fs::metadata(&path)?;
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(metadata.len().to_le_bytes());
        update_modified_time(&mut hasher, &metadata);
        let mut file = File::open(path)?;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }

    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(BundleFingerprint {
        algorithm: "sha256".to_owned(),
        digest: encoded,
    })
}

/// The per-user VST3 folder, relative to `HOME`.
#[cfg(not(target_os = "linux"))]
const USER_VST3_DIRECTORY: &str = "Library/Audio/Plug-Ins/VST3";
#[cfg(target_os = "linux")]
const USER_VST3_DIRECTORY: &str = ".vst3";

/// Discovers direct Windows VST3 children of `roots`: bundle folders, and the single-file
/// `.vst3` DLLs that older Windows plug-ins install. Discovery never opens a bundle.
#[must_use]
pub fn discover_windows_vst3_bundles(roots: &[PathBuf]) -> Vec<PathBuf> {
    vst3_children(roots.iter().cloned(), true)
}

/// Discovers direct VST3 bundle children in the standard user and system locations.
///
/// macOS searches `/Library/Audio/Plug-Ins/VST3` and `~/Library/Audio/Plug-Ins/VST3`. Linux
/// searches each absolute folder in the colon-separated `VST3_PATH` first (`NixOS` profiles keep
/// plug-ins outside the standard folders), then `~/.vst3`, `/usr/lib/vst3`, and
/// `/usr/local/lib/vst3`. Symbolic links to bundles are followed.
///
/// Passing `Some(home)` makes user-directory discovery deterministic for functional tests. With
/// `None`, the current process's `HOME` is used when available. Discovery never opens a bundle.
#[must_use]
pub fn discover_vst3_bundles(home: Option<&Path>) -> Vec<PathBuf> {
    let user_home = home
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("HOME").map(PathBuf::from));
    #[cfg(target_os = "linux")]
    let roots = linux_vst3_roots(env::var_os("VST3_PATH").as_deref(), user_home.as_deref());
    #[cfg(not(target_os = "linux"))]
    let roots = {
        let mut roots = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
        if let Some(home) = user_home {
            roots.push(home.join(USER_VST3_DIRECTORY));
        }
        roots
    };
    vst3_children(roots, false)
}

/// The `.vst3` children of `roots`, canonical, sorted, and deduplicated. Single files count
/// only when `files` is set.
fn vst3_children(roots: impl IntoIterator<Item = PathBuf>, files: bool) -> Vec<PathBuf> {
    let mut bundles = roots
        .into_iter()
        .filter_map(|root| fs::read_dir(root).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            let extension_is_vst3 = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("vst3"));
            (extension_is_vst3 && (path.is_dir() || files && path.is_file())).then_some(path)
        })
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<Vec<_>>();
    bundles.sort();
    bundles.dedup();
    bundles
}

/// Linux VST3 roots in search order. Relative `VST3_PATH` entries are ignored.
#[cfg(target_os = "linux")]
fn linux_vst3_roots(vst3_path: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vst3_path
        .map(|paths| {
            env::split_paths(paths)
                .filter(|path| path.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    roots.extend(home.map(|home| home.join(USER_VST3_DIRECTORY)));
    roots.extend(["/usr/lib/vst3", "/usr/local/lib/vst3"].map(PathBuf::from));
    roots
}

fn collect_regular_files(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_regular_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn update_modified_time(hasher: &mut Sha256, metadata: &fs::Metadata) {
    if let Ok(modified) = metadata.modified()
        && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
    {
        hasher.update(duration.as_secs().to_le_bytes());
        hasher.update(duration.subsec_nanos().to_le_bytes());
    }
}

/// `reported_bundle` is the bundle path as the helper received it.
fn map_scan(
    bundle: PathBuf,
    reported_bundle: &Path,
    fingerprint: BundleFingerprint,
    result: &TimedHelperResult,
    report: Option<io::Result<Vec<u8>>>,
) -> CachedScan {
    let metadata = match result {
        TimedHelperResult::TimedOut { .. } => metadata_with_detail(
            PluginArchitecture::Unknown,
            PluginScanOutcome::TimedOut,
            "scanner exceeded its deadline".to_owned(),
        ),
        TimedHelperResult::Exited { code, .. } => match report {
            None => metadata_with_detail(
                PluginArchitecture::Unknown,
                PluginScanOutcome::Crashed,
                format!("scanner exited with code {code} without a report"),
            ),
            Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => metadata_with_detail(
                PluginArchitecture::Unknown,
                PluginScanOutcome::Crashed,
                format!("scanner exited with code {code} without a report"),
            ),
            Some(Err(error)) => metadata_with_detail(
                PluginArchitecture::Unknown,
                PluginScanOutcome::InvalidReport,
                format!("could not read scanner report: {error}"),
            ),
            Some(Ok(bytes)) => match parse_child_report(reported_bundle, &bytes) {
                Err(detail) => metadata_with_detail(
                    PluginArchitecture::Unknown,
                    PluginScanOutcome::InvalidReport,
                    detail,
                ),
                Ok(mut metadata)
                    if *code != 0 && metadata.outcome == PluginScanOutcome::Supported =>
                {
                    metadata.outcome = PluginScanOutcome::Crashed;
                    metadata.detail = Some(format!(
                        "scanner exited with code {code} after reporting success"
                    ));
                    metadata
                }
                Ok(metadata) => metadata,
            },
        },
    };
    CachedScan {
        bundle,
        fingerprint,
        platform: PluginPlatform::Native,
        metadata,
    }
}

fn parse_child_report(bundle: &Path, bytes: &[u8]) -> Result<PluginScanMetadata, String> {
    let report = serde_json::from_slice::<ChildReport>(bytes)
        .map_err(|error| format!("scanner returned malformed metadata: {error}"))?;
    if report.bundle != bundle {
        return Err("scanner report bundle does not match the requested bundle".to_owned());
    }
    if report.metadata.version != PLUGIN_SCAN_METADATA_VERSION {
        return Err(format!(
            "scanner report metadata version {} is unsupported",
            report.metadata.version
        ));
    }
    Ok(report.metadata)
}

fn metadata_with_detail(
    architecture: PluginArchitecture,
    outcome: PluginScanOutcome,
    detail: String,
) -> PluginScanMetadata {
    let mut metadata = PluginScanMetadata::new(architecture, outcome);
    metadata.detail = Some(detail);
    metadata
}

#[derive(Deserialize)]
struct ChildReport {
    bundle: PathBuf,
    #[serde(flatten)]
    metadata: PluginScanMetadata,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use sp_model::{PluginArchitecture, PluginScanMetadata, PluginScanOutcome};

    use super::{
        BundleFingerprint, CachedScan, ScanCache, Scanner, TemporaryScanReport,
        USER_VST3_DIRECTORY, discover_vst3_bundles, fingerprint_bundle, map_scan,
    };
    use crate::TimedHelperResult;

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let sequence = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "sp-scan-catalog-{}-{unique}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn fingerprint_changes_with_bundle_content() {
        let root = temporary_root();
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(bundle.join("Contents/MacOS")).expect("create bundle");
        let executable = bundle.join("Contents/MacOS/Example");
        fs::write(&executable, b"first").expect("write first content");
        let first = fingerprint_bundle(&bundle).expect("fingerprint first content");
        fs::write(executable, b"second").expect("write second content");
        let second = fingerprint_bundle(&bundle).expect("fingerprint second content");
        assert_ne!(first, second);
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    #[test]
    fn catalog_reuses_only_an_exact_fingerprint() {
        let root = temporary_root();
        let catalog_path = root.join("scan-catalog.json");
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(&bundle).expect("create bundle");
        let fingerprint = fingerprint_bundle(&bundle).expect("fingerprint bundle");
        let canonical = fs::canonicalize(&bundle).expect("canonical bundle");
        let mut catalog = ScanCache::open(&catalog_path).expect("open catalog");
        catalog.insert(CachedScan {
            bundle: canonical.clone(),
            fingerprint: fingerprint.clone(),
            platform: super::PluginPlatform::Native,
            metadata: PluginScanMetadata::new(
                PluginArchitecture::Arm64,
                PluginScanOutcome::Supported,
            ),
        });
        catalog.save().expect("save catalog");
        let reopened = ScanCache::open(catalog_path).expect("reopen catalog");
        assert!(reopened.get(&canonical, &fingerprint).is_some());
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    #[test]
    fn stale_metadata_version_rescans_one_bundle_without_clearing_catalog() {
        let root = temporary_root();
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(&bundle).expect("create bundle");
        let bundle = fs::canonicalize(bundle).expect("canonical bundle");
        let fingerprint = fingerprint_bundle(&bundle).expect("fingerprint bundle");
        let mut catalog = ScanCache::open(root.join("catalog.json")).expect("open catalog");
        let mut stale =
            PluginScanMetadata::new(PluginArchitecture::Arm64, PluginScanOutcome::Supported);
        stale.version = sp_model::PLUGIN_SCAN_METADATA_VERSION - 1;
        catalog.insert(CachedScan {
            bundle: bundle.clone(),
            fingerprint: fingerprint.clone(),
            platform: super::PluginPlatform::Native,
            metadata: stale,
        });
        catalog.save().expect("save stale catalog");

        // A missing helper makes a fresh scan observable without loading third-party code.
        let mut scanner = Scanner::new(
            root.join("missing-scanner"),
            Duration::from_secs(1),
            catalog,
        );
        let refreshed = scanner.scan(&bundle).expect("attempt fresh scan");
        assert_eq!(refreshed.outcome(), PluginScanOutcome::Crashed);
        assert_eq!(
            refreshed.metadata.version,
            sp_model::PLUGIN_SCAN_METADATA_VERSION
        );
        assert_eq!(refreshed.fingerprint, fingerprint);
        assert_eq!(scanner.catalog().entries().count(), 1);
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn cached_failures_do_not_count_again_and_retry_clears_only_one_plugin() {
        use crate::{PersistentQuarantine, PluginFailureKind};

        let root = temporary_root();
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(&bundle).expect("create bundle");
        let quarantine_path = root.join("quarantine.json");
        let quarantine = PersistentQuarantine::open(&quarantine_path, 2).expect("open quarantine");
        let catalog = ScanCache::open(root.join("catalog.json")).expect("open catalog");
        let mut scanner =
            Scanner::new(root.join("missing-helper"), Duration::from_secs(1), catalog)
                .with_quarantine(quarantine);
        let first = scanner.scan(&bundle).expect("record failed launch");
        assert_eq!(first.outcome(), PluginScanOutcome::Crashed);
        scanner.scan(&bundle).expect("reuse cached failure");
        assert_eq!(
            scanner
                .quarantine()
                .unwrap()
                .table()
                .get(&first.fingerprint)
                .unwrap()
                .failure_count,
            1
        );
        scanner
            .rescan(&bundle)
            .expect("explicit rescan counts a fresh failure");
        assert!(
            scanner
                .quarantine()
                .unwrap()
                .ensure_launch_permitted(&first.fingerprint)
                .is_err()
        );

        let other = BundleFingerprint {
            algorithm: "sha256".to_owned(),
            digest: "other-bundle".to_owned(),
        };
        for _ in 0..2 {
            scanner
                .quarantine_mut()
                .unwrap()
                .record_failure(other.clone(), PluginFailureKind::WorkerHang)
                .unwrap();
        }
        scanner
            .clear_quarantine(&first.fingerprint)
            .expect("allow selected retry");
        let reopened = PersistentQuarantine::open(quarantine_path, 2).expect("reopen quarantine");
        assert!(reopened.ensure_launch_permitted(&first.fingerprint).is_ok());
        assert!(reopened.ensure_launch_permitted(&other).is_err());
        assert_eq!(reopened.table().get(&other).unwrap().failure_count, 2);
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn discovers_user_directory_without_scanning_a_bundle() {
        let root = temporary_root();
        let bundle = root.join(USER_VST3_DIRECTORY).join("Fixture.vst3");
        fs::create_dir_all(&bundle).expect("create test bundle");
        let discovered = discover_vst3_bundles(Some(&root));
        assert!(discovered.contains(&fs::canonicalize(&bundle).expect("canonical bundle")));
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_roots_put_vst3_path_overrides_before_standard_folders() {
        let roots = super::linux_vst3_roots(
            Some("/nix/profile/lib/vst3:relative/vst3::/opt/vst3".as_ref()),
            Some("/home/user".as_ref()),
        );
        assert_eq!(
            roots,
            [
                "/nix/profile/lib/vst3",
                "/opt/vst3",
                "/home/user/.vst3",
                "/usr/lib/vst3",
                "/usr/local/lib/vst3",
            ]
            .map(PathBuf::from)
        );
    }

    #[cfg(unix)]
    #[test]
    fn plugin_stdout_does_not_corrupt_a_valid_report() {
        let root = temporary_root();
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(&bundle).expect("create bundle");
        let bundle = fs::canonicalize(bundle).expect("canonical bundle");
        let metadata =
            PluginScanMetadata::new(PluginArchitecture::Arm64, PluginScanOutcome::Supported);
        let report = serde_json::json!({
            "bundle": bundle,
            "version": metadata.version,
            "architecture": metadata.architecture,
            "outcome": metadata.outcome,
            "classes": metadata.classes,
        });
        fs::write(
            root.join("fixture.json"),
            serde_json::to_vec(&report).expect("serialize report"),
        )
        .expect("write report fixture");
        let helper = root.join("mock-scanner.sh");
        fs::write(
            &helper,
            "#!/bin/sh\nprintf '%s\\n' 'UA: loading plugin' 'more diagnostic output'\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --report-path) report_path=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\ncp \"$(dirname \"$0\")/fixture.json\" \"$report_path\"\n",
        )
        .expect("write mock helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .expect("make mock helper executable");
        let catalog = ScanCache::open(root.join("catalog.json")).expect("open catalog");
        let mut scanner = Scanner::new(helper, Duration::from_secs(2), catalog);
        let scan = scanner.scan(&bundle).expect("scan bundle");
        assert_eq!(scan.outcome(), PluginScanOutcome::Supported);
        fs::remove_dir_all(root).expect("remove temporary root");
    }

    /// A Windows scanner that dies is caught, cataloged as Windows, and quarantined exactly like
    /// a native one. `/bin/sh` stands in for Wine here, so no Windows code runs.
    #[cfg(unix)]
    #[test]
    fn a_crashing_windows_scan_is_quarantined_and_cataloged_as_windows() {
        use crate::{PersistentQuarantine, PluginPlatform};
        use sp_wine::WineLauncher;

        let root = temporary_root();
        let vst3 = root.join("prefix").join(sp_wine::VST3_DIR_IN_PREFIX);
        let bundle = vst3.join("Crashy.vst3");
        fs::create_dir_all(bundle.join("Contents/x86_64-win")).expect("create bundle");
        fs::write(vst3.join("Legacy.vst3"), b"MZ").expect("single-file bundle");
        let scanner_exe = root.join("sp-plugin-scanner.exe");
        fs::write(
            &scanner_exe,
            "case \"$2\" in Z:\\\\*) exit 3;; *) exit 4;; esac\n",
        )
        .expect("fake scanner");
        let wine = WineLauncher::new("/bin/sh".into(), root.join("prefix"), root.clone());
        assert_eq!(
            super::discover_windows_vst3_bundles(&wine.vst3_dirs()).len(),
            2,
            "bundle folders and single-file DLLs are both Windows plug-ins"
        );
        let catalog = ScanCache::open(root.join("catalog.json")).expect("open catalog");
        let quarantine =
            PersistentQuarantine::open(root.join("quarantine.json"), 1).expect("open quarantine");
        let mut scanner = Scanner::new(root.join("missing"), Duration::from_secs(5), catalog)
            .with_quarantine(quarantine)
            .with_windows_scanner(&scanner_exe, wine);
        let scan = scanner.scan_windows(&bundle).expect("scan under fake Wine");
        assert_eq!(scan.platform, PluginPlatform::Windows);
        assert_eq!(scan.outcome(), PluginScanOutcome::Crashed);
        assert!(
            scan.metadata
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("code 3"),
            "the scanner received a Z: path: {:?}",
            scan.metadata.detail
        );
        assert!(
            scanner
                .quarantine()
                .unwrap()
                .ensure_launch_permitted(&scan.fingerprint)
                .is_err()
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn malformed_report_is_rejected() {
        let result = TimedHelperResult::Exited {
            process_id: 1,
            code: 0,
        };
        let scan = map_scan(
            PathBuf::from("/tmp/Example.vst3"),
            &PathBuf::from("/tmp/Example.vst3"),
            BundleFingerprint {
                algorithm: "sha256".to_owned(),
                digest: "example".to_owned(),
            },
            &result,
            Some(Ok(b"not json".to_vec())),
        );
        assert_eq!(scan.outcome(), PluginScanOutcome::InvalidReport);
    }

    #[test]
    fn report_input_has_a_size_limit_and_is_cleaned_up() {
        let report = TemporaryScanReport::create().expect("create report location");
        let directory = report.directory.clone();
        fs::write(
            report.path(),
            vec![b'x'; usize::try_from(super::MAX_SCAN_REPORT_BYTES).expect("size fits usize") + 1],
        )
        .expect("write oversized report");
        assert_eq!(
            report
                .read_bounded()
                .expect_err("oversized report must fail")
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        drop(report);
        assert!(!directory.exists());
    }
}

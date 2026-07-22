//! Parent-supervised isolated VST3 scanning and fingerprint invalidation.
//!
//! The parent only discovers canonical bundle paths, fingerprints bytes, and launches one
//! disposable scanner helper per bundle. Layout inspection and all SDK activity stay inside the
//! helper, so the application process never loads third-party plug-in code.

use std::{
    env,
    fmt::Write as _,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sp_model::{
    PLUGIN_SCAN_METADATA_VERSION, PluginArchitecture, PluginClassScanMetadata, PluginScanMetadata,
    PluginScanOutcome,
};

use crate::{
    CapturedHelperOutput, HelperKind, HelperLaunch, ProcessSupervisor, TimedHelperResult,
    catalog::PluginCatalog,
    quarantine::{PersistentQuarantine, PluginFailureKind},
};

/// The bounded scanner deadline used when a deployment does not provide one.
pub const DEFAULT_SCAN_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Persisted outcome for one exact bundle fingerprint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CachedScan {
    /// Canonical bundle path.
    pub bundle: PathBuf,
    /// Fingerprint that produced this result.
    pub fingerprint: BundleFingerprint,
    /// Versioned, SDK-free metadata produced by the disposable helper.
    pub metadata: PluginScanMetadata,
}

impl CachedScan {
    /// Returns the outcome recorded by the isolated scanner.
    #[must_use]
    pub const fn outcome(&self) -> PluginScanOutcome {
        self.metadata.outcome
    }

    /// Returns whether this exact bundle can launch on Apple Silicon.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.metadata.outcome == PluginScanOutcome::Supported
            && matches!(
                self.metadata.architecture,
                PluginArchitecture::Arm64 | PluginArchitecture::Universal
            )
    }
}

/// Scanner executable, timeout policy, persistent catalog, and optional persistent quarantine.
#[derive(Debug)]
pub struct Scanner {
    executable: PathBuf,
    timeout: Duration,
    catalog: PluginCatalog,
    quarantine: Option<PersistentQuarantine>,
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
        }
    }

    /// Creates a scanner with the Phase 2 ten-second helper deadline.
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

    /// Returns the persistent catalog.
    #[must_use]
    pub const fn catalog(&self) -> &PluginCatalog {
        &self.catalog
    }

    /// Compatibility accessor for callers migrating from the Phase 2 scan-cache name.
    #[must_use]
    pub const fn cache(&self) -> &ScanCache {
        &self.catalog
    }

    /// Returns persistent quarantine state when scanner fault ingestion is configured.
    #[must_use]
    pub fn quarantine(&self) -> Option<&PersistentQuarantine> {
        self.quarantine.as_ref()
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
        self.scan_inner(bundle, false)
    }

    /// Invalidates a selected catalog entry and launches a fresh isolated scan.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle cannot be fingerprinted or state cannot be persisted.
    pub fn rescan(&mut self, bundle: &Path) -> std::io::Result<CachedScan> {
        self.scan_inner(bundle, true)
    }

    /// Discovers standard VST3 directories and scans each bundle independently.
    ///
    /// # Errors
    ///
    /// Returns the first persistence/fingerprinting error after prior discovered bundles have
    /// already completed their own helper invocation.
    pub fn scan_standard_locations(
        &mut self,
        home: Option<&Path>,
    ) -> std::io::Result<Vec<CachedScan>> {
        discover_vst3_bundles(home)
            .into_iter()
            .map(|bundle| self.scan(&bundle))
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

    fn scan_inner(&mut self, bundle: &Path, force: bool) -> std::io::Result<CachedScan> {
        let bundle = fs::canonicalize(bundle)?;
        let fingerprint = fingerprint_bundle(&bundle)?;
        if !force && let Some(cached) = self.catalog.get(&bundle, &fingerprint) {
            return Ok(cached.clone());
        }

        let launch = HelperLaunch {
            kind: HelperKind::PluginScanner,
            executable: self.executable.clone(),
            arguments: vec![
                "--bundle".to_owned(),
                bundle.display().to_string(),
                "--json".to_owned(),
                "--sdk-enumerate".to_owned(),
            ],
        };
        let scan = match ProcessSupervisor::new().launch_and_wait_capturing(&launch, self.timeout) {
            Ok(captured) => map_scan(bundle, fingerprint, &captured),
            Err(error) => CachedScan {
                bundle,
                fingerprint,
                metadata: metadata_with_detail(
                    PluginArchitecture::Unknown,
                    PluginScanOutcome::Crashed,
                    format!("could not launch scanner helper: {error}"),
                ),
            },
        };
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

/// Discovers direct VST3 bundle children in the standard user and system macOS locations.
///
/// Passing `Some(home)` makes user-directory discovery deterministic for functional tests. With
/// `None`, the current process's `HOME` is used when available. Discovery never opens a bundle.
#[must_use]
pub fn discover_vst3_bundles(home: Option<&Path>) -> Vec<PathBuf> {
    let user_home = home
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("HOME").map(PathBuf::from));
    let mut roots = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
    if let Some(home) = user_home {
        roots.push(home.join("Library/Audio/Plug-Ins/VST3"));
    }
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
            (extension_is_vst3 && entry.file_type().ok().is_some_and(|kind| kind.is_dir()))
                .then_some(path)
        })
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<Vec<_>>();
    bundles.sort();
    bundles.dedup();
    bundles
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

fn map_scan(
    bundle: PathBuf,
    fingerprint: BundleFingerprint,
    captured: &CapturedHelperOutput,
) -> CachedScan {
    let parsed = serde_json::from_slice::<ChildReport>(&captured.stdout)
        .ok()
        .filter(|report| report.metadata.version == PLUGIN_SCAN_METADATA_VERSION)
        .map(|report| report.metadata);
    let metadata = match &captured.result {
        TimedHelperResult::TimedOut { .. } => metadata_with_detail(
            PluginArchitecture::Unknown,
            PluginScanOutcome::TimedOut,
            "scanner exceeded its deadline".to_owned(),
        ),
        TimedHelperResult::Exited { code, .. } => match parsed {
            None => metadata_with_detail(
                PluginArchitecture::Unknown,
                PluginScanOutcome::Crashed,
                format!("scanner exited with code {code} without valid metadata"),
            ),
            Some(mut metadata)
                if *code != 0 && metadata.outcome == PluginScanOutcome::Supported =>
            {
                metadata.outcome = PluginScanOutcome::Crashed;
                metadata.detail = Some(format!(
                    "scanner exited with code {code} after reporting success"
                ));
                metadata
            }
            Some(metadata) => metadata,
        },
    };
    CachedScan {
        bundle,
        fingerprint,
        metadata,
    }
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
    #[serde(flatten)]
    metadata: PluginScanMetadata,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use sp_model::{PluginArchitecture, PluginScanMetadata, PluginScanOutcome};

    use super::{CachedScan, ScanCache, discover_vst3_bundles, fingerprint_bundle};

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sp-scan-catalog-{}-{unique}", std::process::id()))
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
    fn discovers_user_directory_without_scanning_a_bundle() {
        let root = temporary_root();
        let bundle = root.join("Library/Audio/Plug-Ins/VST3/Fixture.vst3");
        fs::create_dir_all(&bundle).expect("create test bundle");
        let discovered = discover_vst3_bundles(Some(&root));
        assert!(discovered.contains(&fs::canonicalize(&bundle).expect("canonical bundle")));
        fs::remove_dir_all(root).expect("remove temporary root");
    }
}

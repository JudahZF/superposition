//! Cached, parent-supervised VST3 bundle scanning.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CapturedHelperOutput, HelperKind, HelperLaunch, ProcessSupervisor, TimedHelperResult};

const CACHE_VERSION: u32 = 1;

/// Stable content identity used for scan-cache invalidation and quarantine.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BundleFingerprint {
    /// Hash algorithm used for `digest`.
    pub algorithm: String,
    /// Lowercase content digest.
    pub digest: String,
}

/// One VST3 class exposed by a scanned bundle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScanDescriptor {
    /// Vendor-assigned VST3 class identifier.
    pub class_id: String,
    /// User-visible plug-in name.
    pub name: String,
    /// User-visible vendor name.
    pub vendor: String,
}

/// Stable scanner outcome stored in the cache.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    /// Native bundle and factory enumeration succeeded.
    Supported,
    /// Bundle does not contain arm64 code.
    UnsupportedArchitecture,
    /// Bundle layout is malformed.
    InvalidBundle,
    /// Scanner exceeded its deadline.
    TimedOut,
    /// Scanner exited without a valid report.
    Crashed,
    /// VST3 SDK enumeration returned an error.
    SdkError,
}

/// Persisted outcome for one exact bundle fingerprint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedScan {
    /// Canonical bundle path.
    pub bundle: PathBuf,
    /// Fingerprint that produced this result.
    pub fingerprint: BundleFingerprint,
    /// Native architecture label from the helper.
    pub architectures: String,
    /// Scan outcome.
    pub status: ScanStatus,
    /// VST3 classes discovered by the helper.
    pub descriptors: Vec<ScanDescriptor>,
    /// Optional diagnostic detail.
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<PathBuf, CachedScan>,
}

/// File-backed scan cache keyed by canonical bundle path and content fingerprint.
#[derive(Debug)]
pub struct ScanCache {
    path: PathBuf,
    entries: BTreeMap<PathBuf, CachedScan>,
}

impl ScanCache {
    /// Opens an existing cache or creates an empty in-memory cache when absent.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable, malformed, or unsupported cache files.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let entries = match fs::read(&path) {
            Ok(bytes) => {
                let cache: CacheFile = serde_json::from_slice(&bytes).map_err(invalid_data)?;
                if cache.version != CACHE_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unsupported scan cache version {}", cache.version),
                    ));
                }
                cache.entries
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, entries })
    }

    /// Returns a cached result only when its content fingerprint still matches.
    #[must_use]
    pub fn get(&self, bundle: &Path, fingerprint: &BundleFingerprint) -> Option<&CachedScan> {
        self.entries
            .get(bundle)
            .filter(|entry| &entry.fingerprint == fingerprint)
    }

    /// Inserts or replaces one canonical bundle result.
    pub fn insert(&mut self, scan: CachedScan) {
        self.entries.insert(scan.bundle.clone(), scan);
    }

    /// Persists the cache with a temporary-file replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization or file replacement fails.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&CacheFile {
            version: CACHE_VERSION,
            entries: self.entries.clone(),
        })
        .map_err(invalid_data)?;
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(temporary, &self.path)
    }
}

/// Scanner executable plus timeout policy and persistent cache.
#[derive(Debug)]
pub struct Scanner {
    executable: PathBuf,
    timeout: Duration,
    cache: ScanCache,
}

impl Scanner {
    /// Creates a cached isolated scanner.
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>, timeout: Duration, cache: ScanCache) -> Self {
        Self {
            executable: executable.into(),
            timeout,
            cache,
        }
    }

    /// Returns the scan cache.
    #[must_use]
    pub const fn cache(&self) -> &ScanCache {
        &self.cache
    }

    /// Fingerprints and scans one bundle, reusing an unchanged cached result.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle cannot be fingerprinted, the helper cannot run,
    /// or the updated cache cannot be persisted.
    pub fn scan(&mut self, bundle: &Path) -> std::io::Result<CachedScan> {
        let bundle = fs::canonicalize(bundle)?;
        let fingerprint = fingerprint_bundle(&bundle)?;
        if let Some(cached) = self.cache.get(&bundle, &fingerprint) {
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
        let captured = ProcessSupervisor::new().launch_and_wait_capturing(&launch, self.timeout)?;
        let scan = map_scan(bundle, fingerprint, &captured);
        self.cache.insert(scan.clone());
        self.cache.save()?;
        Ok(scan)
    }
}

/// Computes a deterministic SHA-256 over the canonical bundle path, entry metadata, and files.
///
/// # Errors
///
/// Returns an error when the bundle cannot be traversed or read.
pub fn fingerprint_bundle(bundle: &Path) -> std::io::Result<BundleFingerprint> {
    let bundle = fs::canonicalize(bundle)?;
    let mut files = Vec::new();
    collect_files(&bundle, &mut files)?;
    files.sort();

    let mut hasher = Sha256::new();
    hasher.update(bundle.as_os_str().as_encoded_bytes());
    for path in files {
        let relative = path.strip_prefix(&bundle).unwrap_or(&path);
        let metadata = fs::metadata(&path)?;
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(metadata.len().to_le_bytes());
        if let Ok(modified) = metadata.modified()
            && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
        {
            hasher.update(duration.as_secs().to_le_bytes());
            hasher.update(duration.subsec_nanos().to_le_bytes());
        }
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

/// Discovers direct child bundles in the two standard macOS VST3 locations.
#[must_use]
pub fn discover_vst3_bundles(home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
    if let Some(home) = home {
        roots.push(home.join("Library/Audio/Plug-Ins/VST3"));
    }
    let mut bundles = roots
        .into_iter()
        .filter_map(|root| fs::read_dir(root).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "vst3")
        })
        .collect::<Vec<_>>();
    bundles.sort();
    bundles.dedup();
    bundles
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ChildReport {
    architectures: String,
    outcome: ScanStatus,
    #[serde(default)]
    descriptors: Vec<ScanDescriptor>,
    detail: Option<String>,
}

fn map_scan(
    bundle: PathBuf,
    fingerprint: BundleFingerprint,
    captured: &CapturedHelperOutput,
) -> CachedScan {
    let parsed = serde_json::from_slice::<ChildReport>(&captured.stdout).ok();
    match &captured.result {
        TimedHelperResult::TimedOut { .. } => CachedScan {
            bundle,
            fingerprint,
            architectures: "unknown".to_owned(),
            status: ScanStatus::TimedOut,
            descriptors: Vec::new(),
            detail: Some("scanner exceeded its deadline".to_owned()),
        },
        TimedHelperResult::Exited { code, .. } => match parsed {
            None => CachedScan {
                bundle,
                fingerprint,
                architectures: "unknown".to_owned(),
                status: ScanStatus::Crashed,
                descriptors: Vec::new(),
                detail: Some(format!(
                    "scanner exited with code {code} without valid JSON"
                )),
            },
            Some(report) => CachedScan {
                bundle,
                fingerprint,
                architectures: report.architectures,
                status: if *code == 0 || report.outcome != ScanStatus::Supported {
                    report.outcome
                } else {
                    ScanStatus::Crashed
                },
                descriptors: report.descriptors,
                detail: report.detail,
            },
        },
    }
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{CachedScan, ScanCache, ScanStatus, fingerprint_bundle};

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let thread = std::thread::current().id();
        std::env::temp_dir().join(format!("sp-scan-cache-{unique}-{thread:?}"))
    }

    use std::path::PathBuf;

    #[test]
    fn fingerprint_changes_with_bundle_content() {
        let root = temporary_root();
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        let executable = bundle.join("Contents/MacOS/Example");
        fs::write(&executable, b"first").unwrap();
        let first = fingerprint_bundle(&bundle).unwrap();
        fs::write(executable, b"second").unwrap();
        let second = fingerprint_bundle(&bundle).unwrap();
        assert_ne!(first, second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_reuses_only_an_exact_fingerprint() {
        let root = temporary_root();
        let cache_path = root.join("scan-cache.json");
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(&bundle).unwrap();
        let fingerprint = fingerprint_bundle(&bundle).unwrap();
        let canonical = fs::canonicalize(&bundle).unwrap();
        let mut cache = ScanCache::open(&cache_path).unwrap();
        cache.insert(CachedScan {
            bundle: canonical.clone(),
            fingerprint: fingerprint.clone(),
            architectures: "arm64".to_owned(),
            status: ScanStatus::Supported,
            descriptors: Vec::new(),
            detail: None,
        });
        cache.save().unwrap();
        let reopened = ScanCache::open(cache_path).unwrap();
        assert!(reopened.get(&canonical, &fingerprint).is_some());
        fs::remove_dir_all(root).unwrap();
    }
}

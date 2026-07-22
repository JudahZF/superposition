//! Persistent VST3 scan catalog keyed by canonical bundle path and fingerprint.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::scanner::{BundleFingerprint, CachedScan};

/// Version of the on-disk plug-in catalog schema.
pub const CATALOG_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Deserialize, Serialize)]
struct CatalogFile {
    version: u32,
    entries: BTreeMap<PathBuf, CachedScan>,
}

/// Atomically persisted catalog of isolated bundle scan results.
///
/// Entries remain addressable by canonical path for discovery and are reused only when the
/// stored fingerprint exactly matches the current canonical bundle fingerprint.
#[derive(Debug)]
pub struct PluginCatalog {
    path: PathBuf,
    entries: BTreeMap<PathBuf, CachedScan>,
}

impl PluginCatalog {
    /// Opens a catalog, or creates an empty in-memory catalog when `path` does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog is unreadable, malformed, or uses another schema.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let entries = match fs::read(&path) {
            Ok(bytes) => {
                let catalog: CatalogFile = serde_json::from_slice(&bytes).map_err(invalid_data)?;
                if catalog.version != CATALOG_SCHEMA_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "unsupported plug-in catalog version {}; expected {CATALOG_SCHEMA_VERSION}",
                            catalog.version
                        ),
                    ));
                }
                catalog.entries
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, entries })
    }

    /// Returns the persisted catalog location.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a cached result only when its content fingerprint still matches.
    #[must_use]
    pub fn get(&self, bundle: &Path, fingerprint: &BundleFingerprint) -> Option<&CachedScan> {
        self.entries
            .get(bundle)
            .filter(|entry| &entry.fingerprint == fingerprint)
    }

    /// Returns the result currently stored for one canonical bundle path.
    #[must_use]
    pub fn get_by_path(&self, bundle: &Path) -> Option<&CachedScan> {
        self.entries.get(bundle)
    }

    /// Returns every cataloged result in canonical-path order.
    pub fn entries(&self) -> impl Iterator<Item = &CachedScan> {
        self.entries.values()
    }

    /// Inserts or replaces one canonical bundle result.
    pub fn insert(&mut self, scan: CachedScan) {
        self.entries.insert(scan.bundle.clone(), scan);
    }

    /// Removes one canonical bundle result and returns it when present.
    pub fn remove(&mut self, bundle: &Path) -> Option<CachedScan> {
        self.entries.remove(bundle)
    }

    /// Removes all cataloged results without writing the replacement automatically.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Persists the catalog by syncing a temporary file and atomically replacing the old file.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization, synchronization, or replacement fails.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_path(&self.path);
        let bytes = serde_json::to_vec_pretty(&CatalogFile {
            version: CATALOG_SCHEMA_VERSION,
            entries: self.entries.clone(),
        })
        .map_err(invalid_data)?;
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        sync_parent_directory(self.path.parent())
    }

    /// Clears and immediately persists the catalog for a user-requested full rescan.
    ///
    /// # Errors
    ///
    /// Returns an error when the empty catalog cannot be persisted.
    pub fn clear_and_save(&mut self) -> std::io::Result<()> {
        self.clear();
        self.save()
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let extension = path.extension().map_or_else(
        || "tmp".to_owned(),
        |extension| format!("{}.tmp", extension.to_string_lossy()),
    );
    path.with_extension(extension)
}

fn sync_parent_directory(parent: Option<&Path>) -> std::io::Result<()> {
    if let Some(parent) = parent {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use sp_model::{PluginArchitecture, PluginScanMetadata, PluginScanOutcome};

    use super::PluginCatalog;
    use crate::scanner::{BundleFingerprint, CachedScan};

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sp-plugin-catalog-{}-{unique}", std::process::id()))
    }

    #[test]
    fn persists_and_invalidates_by_exact_fingerprint() {
        let root = temporary_root();
        let path = root.join("catalog.json");
        let bundle = root.join("Example.vst3");
        fs::create_dir_all(&bundle).expect("create bundle");
        let canonical = fs::canonicalize(&bundle).expect("canonical bundle");
        let fingerprint = BundleFingerprint {
            algorithm: "sha256".to_owned(),
            digest: "first".to_owned(),
        };
        let mut catalog = PluginCatalog::open(&path).expect("open catalog");
        catalog.insert(CachedScan {
            bundle: canonical.clone(),
            fingerprint: fingerprint.clone(),
            metadata: PluginScanMetadata::new(
                PluginArchitecture::Arm64,
                PluginScanOutcome::Supported,
            ),
        });
        catalog.save().expect("save catalog");

        let reopened = PluginCatalog::open(path).expect("reopen catalog");
        assert!(reopened.get(&canonical, &fingerprint).is_some());
        assert!(
            reopened
                .get(
                    &canonical,
                    &BundleFingerprint {
                        algorithm: "sha256".to_owned(),
                        digest: "second".to_owned(),
                    },
                )
                .is_none()
        );
        fs::remove_dir_all(root).expect("remove temporary root");
    }
}

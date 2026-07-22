//! Persistent fingerprint-keyed plug-in failure and quarantine state.
//!
//! This module deliberately uses only scanner data types and filesystem persistence. It never
//! loads a bundle or depends on VST3 SDK bindings, so it is safe for the application control
//! plane to use before launching a worker.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::BundleFingerprint;

const QUARANTINE_SCHEMA_VERSION: u32 = 1;

/// The control-plane event that contributed to a plug-in quarantine decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginFailureKind {
    /// The isolated scanner was killed after its deadline elapsed.
    ScannerTimeout,
    /// The isolated scanner terminated without a usable result.
    ScannerCrash,
    /// A rack worker exited while loading or processing the plug-in.
    WorkerCrash,
    /// A rack worker missed a bounded operation deadline.
    WorkerTimeout,
    /// A rack worker remained unresponsive after a bounded health check.
    WorkerHang,
}

/// Failure counters and the last observed failure for one immutable bundle fingerprint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuarantineRecord {
    /// Number of relevant scanner or worker failures seen for this fingerprint.
    pub failure_count: u32,
    /// Whether the failure threshold has been reached.
    pub quarantined: bool,
    /// Most recent failure class recorded for this fingerprint.
    pub last_failure: PluginFailureKind,
}

/// In-memory fingerprint-keyed quarantine policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineTable {
    records: BTreeMap<BundleFingerprint, QuarantineRecord>,
    threshold: u32,
}

impl QuarantineTable {
    /// Creates an empty table that quarantines after `threshold` failures.
    #[must_use]
    pub fn with_threshold(threshold: u32) -> Self {
        Self {
            records: BTreeMap::new(),
            threshold: threshold.max(1),
        }
    }

    /// Creates an empty table with the default threshold of three failures.
    #[must_use]
    pub fn new() -> Self {
        Self::with_threshold(3)
    }

    /// Records a scan or worker failure and returns whether the fingerprint is quarantined.
    pub fn record_failure(
        &mut self,
        fingerprint: BundleFingerprint,
        kind: PluginFailureKind,
    ) -> bool {
        let record = self.records.entry(fingerprint).or_insert(QuarantineRecord {
            failure_count: 0,
            quarantined: false,
            last_failure: kind,
        });
        record.failure_count = record.failure_count.saturating_add(1);
        record.last_failure = kind;
        record.quarantined |= record.failure_count >= self.threshold;
        record.quarantined
    }

    /// Returns whether the fingerprint must be blocked before a worker launch.
    #[must_use]
    pub fn is_quarantined(&self, fingerprint: &BundleFingerprint) -> bool {
        self.records
            .get(fingerprint)
            .is_some_and(|record| record.quarantined)
    }

    /// Returns the persisted failure record for a fingerprint.
    #[must_use]
    pub fn get(&self, fingerprint: &BundleFingerprint) -> Option<&QuarantineRecord> {
        self.records.get(fingerprint)
    }

    /// Returns all records in reproducible fingerprint order.
    #[must_use]
    pub fn records(&self) -> &BTreeMap<BundleFingerprint, QuarantineRecord> {
        &self.records
    }

    /// Clears all failure and quarantine state for `fingerprint`.
    pub fn clear(&mut self, fingerprint: &BundleFingerprint) -> Option<QuarantineRecord> {
        self.records.remove(fingerprint)
    }

    /// Clears all failure and quarantine state.
    pub fn clear_all(&mut self) {
        self.records.clear();
    }

    /// Returns the configured failure threshold.
    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }
}

impl Default for QuarantineTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct QuarantineFile {
    version: u32,
    threshold: u32,
    records: Vec<(BundleFingerprint, QuarantineRecord)>,
}

/// Rejection returned before a worker is launched for a quarantined fingerprint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantinedPlugin {
    fingerprint: BundleFingerprint,
    failure_count: u32,
}

impl QuarantinedPlugin {
    /// Returns the fingerprint blocked from worker launch.
    #[must_use]
    pub const fn fingerprint(&self) -> &BundleFingerprint {
        &self.fingerprint
    }

    /// Returns the failure count that triggered quarantine.
    #[must_use]
    pub const fn failure_count(&self) -> u32 {
        self.failure_count
    }
}

impl fmt::Display for QuarantinedPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "plug-in {}:{} is quarantined after {} failures",
            self.fingerprint.algorithm, self.fingerprint.digest, self.failure_count
        )
    }
}

impl std::error::Error for QuarantinedPlugin {}

/// Atomically persisted [`QuarantineTable`] state.
#[derive(Debug)]
pub struct PersistentQuarantine {
    path: PathBuf,
    table: QuarantineTable,
}

impl PersistentQuarantine {
    /// Opens an existing quarantine file or creates an empty in-memory store when absent.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable, malformed, or incompatible on-disk state.
    pub fn open(path: impl Into<PathBuf>, threshold: u32) -> std::io::Result<Self> {
        let path = path.into();
        let threshold = threshold.max(1);
        let table = match fs::read(&path) {
            Ok(bytes) => {
                let persisted: QuarantineFile =
                    serde_json::from_slice(&bytes).map_err(invalid_data)?;
                if persisted.version != QUARANTINE_SCHEMA_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "unsupported quarantine schema version {}",
                            persisted.version
                        ),
                    ));
                }
                if persisted.threshold != threshold {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "quarantine threshold {} does not match configured threshold {threshold}",
                            persisted.threshold
                        ),
                    ));
                }
                QuarantineTable {
                    records: persisted.records.into_iter().collect(),
                    threshold: persisted.threshold,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                QuarantineTable::with_threshold(threshold)
            }
            Err(error) => return Err(error),
        };
        Ok(Self { path, table })
    }

    /// Returns the backing file path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns the in-memory quarantine state.
    #[must_use]
    pub const fn table(&self) -> &QuarantineTable {
        &self.table
    }

    /// Rejects a worker launch before the helper process is spawned when its fingerprint is
    /// quarantined. Clearing the exact fingerprint through [`Self::clear`] permits a later retry.
    ///
    /// # Errors
    ///
    /// Returns [`QuarantinedPlugin`] when the fingerprint has reached the configured threshold.
    pub fn ensure_launch_permitted(
        &self,
        fingerprint: &BundleFingerprint,
    ) -> Result<(), QuarantinedPlugin> {
        let Some(record) = self.table.get(fingerprint) else {
            return Ok(());
        };
        if !record.quarantined {
            return Ok(());
        }
        Err(QuarantinedPlugin {
            fingerprint: fingerprint.clone(),
            failure_count: record.failure_count,
        })
    }

    /// Records a failure and atomically persists the updated state.
    ///
    /// # Errors
    ///
    /// Returns an error if the updated state cannot be serialized or atomically stored.
    pub fn record_failure(
        &mut self,
        fingerprint: BundleFingerprint,
        kind: PluginFailureKind,
    ) -> std::io::Result<bool> {
        let quarantined = self.table.record_failure(fingerprint, kind);
        self.save()?;
        Ok(quarantined)
    }

    /// Clears one fingerprint and atomically persists the updated state.
    ///
    /// # Errors
    ///
    /// Returns an error if the updated state cannot be stored.
    pub fn clear(&mut self, fingerprint: &BundleFingerprint) -> std::io::Result<()> {
        self.table.clear(fingerprint);
        self.save()
    }

    /// Clears every fingerprint and atomically persists the updated state.
    ///
    /// # Errors
    ///
    /// Returns an error if the updated state cannot be stored.
    pub fn clear_all(&mut self) -> std::io::Result<()> {
        self.table.clear_all();
        self.save()
    }

    /// Atomically writes the current state to its backing path.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory cannot be created, data cannot be serialized, or the
    /// temporary file cannot be replaced.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&QuarantineFile {
            version: QUARANTINE_SCHEMA_VERSION,
            threshold: self.table.threshold,
            records: self
                .table
                .records
                .iter()
                .map(|(fingerprint, record)| (fingerprint.clone(), record.clone()))
                .collect(),
        })
        .map_err(invalid_data)?;
        atomic_replace(&self.path, &bytes)
    }
}

fn atomic_replace(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
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
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{PersistentQuarantine, PluginFailureKind};
    use crate::BundleFingerprint;

    fn temporary_path() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sp-quarantine-{unique}.json"))
    }

    fn fingerprint() -> BundleFingerprint {
        BundleFingerprint {
            algorithm: "sha256".to_owned(),
            digest: "abcdef".to_owned(),
        }
    }

    #[test]
    fn persists_failure_counts_and_manual_clear() {
        let path = temporary_path();
        let fingerprint = fingerprint();
        let mut store = PersistentQuarantine::open(&path, 2).expect("open store");
        assert!(
            !store
                .record_failure(fingerprint.clone(), PluginFailureKind::ScannerTimeout)
                .expect("persist first failure")
        );
        assert!(
            store
                .record_failure(fingerprint.clone(), PluginFailureKind::WorkerCrash)
                .expect("persist second failure")
        );
        drop(store);

        let mut reopened = PersistentQuarantine::open(&path, 2).expect("reopen store");
        assert!(reopened.table().is_quarantined(&fingerprint));
        assert_eq!(
            reopened
                .table()
                .get(&fingerprint)
                .map(|record| record.failure_count),
            Some(2)
        );
        assert!(reopened.ensure_launch_permitted(&fingerprint).is_err());
        reopened.clear(&fingerprint).expect("clear fingerprint");
        assert!(reopened.ensure_launch_permitted(&fingerprint).is_ok());
        assert!(!reopened.table().is_quarantined(&fingerprint));
        fs::remove_file(path).expect("remove store");
    }
}

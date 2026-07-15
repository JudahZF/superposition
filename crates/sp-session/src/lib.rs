//! Serializable session packages and their crash-recovery metadata.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sp_model::{PluginSlot, Session};
use thiserror::Error;

/// Current session-file schema version supported by this crate.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// A serializable project session independent of audio backends and plug-in SDKs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionDocument {
    /// Version used to decode this document.
    pub schema_version: u32,
    /// Host-owned topology, scenes, and MIDI mappings.
    #[serde(default)]
    pub model: Session,
}

impl SessionDocument {
    /// Returns a new empty document using the current schema.
    pub fn empty() -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            model: Session::new(),
        }
    }

    /// Validates the embedded model against Phase 0 capacity contracts.
    ///
    /// # Errors
    ///
    /// Returns [`sp_model::ValidationError`] when the model is inconsistent.
    pub fn validate(&self) -> Result<(), sp_model::ValidationError> {
        self.model.validate()
    }
}

impl Default for SessionDocument {
    fn default() -> Self {
        Self::empty()
    }
}

/// The JSON payload stored at the root of a session package.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionPackage {
    /// Package schema version.
    pub schema_version: u32,
    /// Session data written to the package's `session.json` file.
    pub session_json: SessionDocument,
}

impl SessionPackage {
    /// Creates a package for a session document.
    pub fn new(session_json: SessionDocument) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            session_json,
        }
    }
}

/// Crash-recovery state stored beside `session.json`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryMarker {
    /// Whether the last session shutdown completed normally.
    pub clean_shutdown: bool,
}

/// Well-known paths inside a package directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageLayout {
    root: PathBuf,
}

impl PackageLayout {
    /// Creates paths rooted at a session-package directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the package directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the current session JSON path.
    pub fn session_json(&self) -> PathBuf {
        self.root.join("session.json")
    }

    /// Returns the last complete session JSON path retained during an atomic save.
    pub fn previous_session_json(&self) -> PathBuf {
        self.root.join("session.json.previous")
    }

    /// Returns the recovery-marker path.
    pub fn recovery_json(&self) -> PathBuf {
        self.root.join("recovery.json")
    }

    /// Returns the directory for opaque state of one plug-in.
    ///
    /// # Errors
    ///
    /// Returns an error when the plug-in identifier is not a single path component.
    pub fn plugin_dir(&self, plugin_id: &str) -> Result<PathBuf, SessionError> {
        validate_plugin_id(plugin_id)?;
        Ok(self.root.join("plugins").join(plugin_id))
    }

    /// Returns the opaque component-state path for one plug-in.
    ///
    /// # Errors
    ///
    /// Returns an error when the plug-in identifier is invalid.
    pub fn component_bin(&self, plugin_id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.plugin_dir(plugin_id)?.join("component.bin"))
    }

    /// Returns the opaque controller-state path for one plug-in.
    ///
    /// # Errors
    ///
    /// Returns an error when the plug-in identifier is invalid.
    pub fn controller_bin(&self, plugin_id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.plugin_dir(plugin_id)?.join("controller.bin"))
    }
}

/// Errors returned by session-package operations.
#[derive(Debug, Error)]
pub enum SessionError {
    /// A file operation failed.
    #[error("session package I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON in a package file could not be decoded.
    #[error("session package JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    /// Both the current and retained package payloads were unavailable or invalid.
    #[error("no recoverable session payload")]
    NoRecoverableSession,
    /// A plug-in identifier could escape its package directory.
    #[error("invalid plug-in identifier: {0}")]
    InvalidPluginId(String),
}

/// File-backed store that keeps the preceding complete JSON payload for recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtomicFileSessionStore {
    layout: PackageLayout,
}

impl AtomicFileSessionStore {
    /// Creates a store rooted at a package directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            layout: PackageLayout::new(root),
        }
    }

    /// Returns the paths used by this store.
    pub fn layout(&self) -> &PackageLayout {
        &self.layout
    }

    /// Writes a package through a temporary file and retains the prior payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the package cannot be encoded or written.
    pub fn save_atomic(&self, package: &SessionPackage) -> Result<(), SessionError> {
        fs::create_dir_all(self.layout.root())?;
        self.mark_clean_shutdown(false)?;
        let temporary = self.layout.root().join("session.json.tmp");
        let encoded = serde_json::to_vec_pretty(package)?;
        let mut file = File::create(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);

        let session = self.layout.session_json();
        if session.exists() {
            fs::rename(&session, self.layout.previous_session_json())?;
        }
        fs::rename(temporary, session)?;
        Ok(())
    }

    /// Loads the current payload, falling back to the prior complete payload after a torn write.
    ///
    /// # Errors
    ///
    /// Returns an error when neither payload can be loaded.
    pub fn load(&self) -> Result<SessionPackage, SessionError> {
        match read_package(&self.layout.session_json()) {
            Ok(package) => Ok(package),
            Err(_) => read_package(&self.layout.previous_session_json())
                .map_err(|_| SessionError::NoRecoverableSession),
        }
    }

    /// Records whether this package was closed normally.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker cannot be encoded or written.
    pub fn mark_clean_shutdown(&self, clean_shutdown: bool) -> Result<(), SessionError> {
        fs::create_dir_all(self.layout.root())?;
        let marker = serde_json::to_vec_pretty(&RecoveryMarker { clean_shutdown })?;
        fs::write(self.layout.recovery_json(), marker)?;
        Ok(())
    }

    /// Returns whether a previous unclean shutdown should be offered for recovery.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing marker cannot be read or decoded.
    pub fn offer_recovery(&self) -> Result<bool, SessionError> {
        match fs::read(self.layout.recovery_json()) {
            Ok(bytes) => Ok(!serde_json::from_slice::<RecoveryMarker>(&bytes)?.clean_shutdown),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Writes opaque component and controller state for a plug-in.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is invalid or either state file cannot be written.
    pub fn save_plugin_state(
        &self,
        plugin_id: &str,
        component: &[u8],
        controller: &[u8],
    ) -> Result<(), SessionError> {
        let directory = self.layout.plugin_dir(plugin_id)?;
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("component.bin"), component)?;
        fs::write(directory.join("controller.bin"), controller)?;
        Ok(())
    }

    /// Loads opaque component and controller state for a plug-in.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is invalid or either state file cannot be read.
    pub fn load_plugin_state(&self, plugin_id: &str) -> Result<(Vec<u8>, Vec<u8>), SessionError> {
        Ok((
            fs::read(self.layout.component_bin(plugin_id)?)?,
            fs::read(self.layout.controller_bin(plugin_id)?)?,
        ))
    }
}

/// Storage adapter for simple session documents.
pub trait SessionStore {
    /// Loads a document from its storage location.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be read or decoded.
    fn load(&self) -> Result<SessionDocument, Box<dyn std::error::Error + Send + Sync>>;
    /// Persists a document to its storage location.
    ///
    /// # Errors
    ///
    /// Returns an error when the document cannot be encoded or written.
    fn save(
        &self,
        document: &SessionDocument,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

impl SessionStore for AtomicFileSessionStore {
    fn load(&self) -> Result<SessionDocument, Box<dyn std::error::Error + Send + Sync>> {
        Ok(AtomicFileSessionStore::load(self)?.session_json)
    }

    fn save(
        &self,
        document: &SessionDocument,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.save_atomic(&SessionPackage::new(document.clone()))?;
        Ok(())
    }
}

fn read_package(path: &Path) -> Result<SessionPackage, SessionError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn validate_plugin_id(plugin_id: &str) -> Result<(), SessionError> {
    if plugin_id.is_empty() || Path::new(plugin_id).components().count() != 1 {
        return Err(SessionError::InvalidPluginId(plugin_id.to_owned()));
    }
    Ok(())
}

/// Why a plug-in slot cannot be restored onto a live worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaceholderReason {
    /// Fingerprint is absent from the available plug-in set.
    MissingPlugin,
    /// Fingerprint is quarantined until the user clears it.
    Quarantined,
}

/// A slot retained in the session model but not launched on a worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingPluginPlaceholder {
    /// Rack index in the session model.
    pub rack_index: usize,
    /// Slot index within the rack.
    pub slot_index: usize,
    /// Plug-in instance identifier.
    pub instance_id: String,
    /// Fingerprint digest expected by the session.
    pub fingerprint: String,
    /// Why the slot is inert.
    pub reason: PlaceholderReason,
}

/// Application-facing session controller for save, autosave, and recovery offers.
#[derive(Debug)]
pub struct SessionController {
    store: AtomicFileSessionStore,
    document: SessionDocument,
    dirty: bool,
    recovery_offered: bool,
}

impl SessionController {
    /// Opens a package directory, offering recovery when the previous shutdown was unclean.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing package cannot be read.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let store = AtomicFileSessionStore::new(root);
        let recovery_offered = store.offer_recovery()?;
        let document = match store.load() {
            Ok(package) => package.session_json,
            Err(SessionError::NoRecoverableSession) => SessionDocument::empty(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            store,
            document,
            dirty: false,
            recovery_offered,
        })
    }

    /// Creates a controller around an in-memory empty document rooted at `root`.
    #[must_use]
    pub fn empty(root: impl Into<PathBuf>) -> Self {
        Self {
            store: AtomicFileSessionStore::new(root),
            document: SessionDocument::empty(),
            dirty: false,
            recovery_offered: false,
        }
    }

    /// Returns whether boot should present a recovery offer to the user.
    #[must_use]
    pub const fn recovery_offered(&self) -> bool {
        self.recovery_offered
    }

    /// Clears the recovery-offer flag after the user accepts or discards it.
    pub fn acknowledge_recovery_offer(&mut self) {
        self.recovery_offered = false;
    }

    /// Borrows the live session document.
    #[must_use]
    pub const fn document(&self) -> &SessionDocument {
        &self.document
    }

    /// Borrows the live session document mutably and marks it dirty.
    pub fn document_mut(&mut self) -> &mut SessionDocument {
        self.dirty = true;
        &mut self.document
    }

    /// Returns whether unsaved edits exist.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Atomically saves the current document and marks a clean shutdown checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the package cannot be written.
    pub fn save(&mut self) -> Result<(), SessionError> {
        self.document.validate().map_err(|error| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            ))
        })?;
        self.store
            .save_atomic(&SessionPackage::new(self.document.clone()))?;
        self.store.mark_clean_shutdown(true)?;
        self.dirty = false;
        Ok(())
    }

    /// Autosave that persists host model/parameter snapshots without claiming a clean exit.
    ///
    /// Opaque plug-in bytes are left untouched; scenes must never restore them.
    ///
    /// # Errors
    ///
    /// Returns an error when the package cannot be written.
    pub fn autosave(&mut self) -> Result<(), SessionError> {
        self.document.validate().map_err(|error| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            ))
        })?;
        self.store
            .save_atomic(&SessionPackage::new(self.document.clone()))?;
        self.store.mark_clean_shutdown(false)?;
        self.dirty = false;
        Ok(())
    }

    /// Marks an intentional clean shutdown without rewriting session JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the recovery marker cannot be written.
    pub fn mark_clean_exit(&self) -> Result<(), SessionError> {
        self.store.mark_clean_shutdown(true)
    }

    /// Scans the session for plug-ins absent from `available_fingerprints`.
    ///
    /// Missing slots remain in the model as bypassed placeholders; callers must not launch
    /// workers for them and must not restore opaque state onto live scenes.
    #[must_use]
    pub fn missing_plugin_placeholders(
        &self,
        available_fingerprints: &BTreeSet<String>,
        quarantined_fingerprints: &BTreeSet<String>,
    ) -> Vec<MissingPluginPlaceholder> {
        let mut placeholders = Vec::new();
        for (rack_index, rack) in self.document.model.racks.iter().enumerate() {
            for (slot_index, slot) in rack.slots.iter().enumerate() {
                let fingerprint = slot.plugin.fingerprint.digest.clone();
                let reason = if quarantined_fingerprints.contains(&fingerprint) {
                    Some(PlaceholderReason::Quarantined)
                } else if !available_fingerprints.contains(&fingerprint) {
                    Some(PlaceholderReason::MissingPlugin)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    placeholders.push(MissingPluginPlaceholder {
                        rack_index,
                        slot_index,
                        instance_id: slot.id.0.clone(),
                        fingerprint,
                        reason,
                    });
                }
            }
        }
        placeholders
    }
}

/// Returns whether a scene recall may touch opaque plug-in state (always `false`).
#[must_use]
pub const fn scene_may_restore_opaque_state() -> bool {
    false
}

/// Helper for tests and UI: treat a slot as bypassed when it is a placeholder.
#[must_use]
pub fn slot_is_placeholder(slot: &PluginSlot, available_fingerprints: &BTreeSet<String>) -> bool {
    !available_fingerprints.contains(&slot.plugin.fingerprint.digest)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use sp_model::Session;

    use super::{
        AtomicFileSessionStore, RecoveryMarker, SESSION_SCHEMA_VERSION, SessionController,
        SessionDocument, SessionPackage, scene_may_restore_opaque_state,
    };

    fn temporary_package() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let thread = std::thread::current().id();
        std::env::temp_dir().join(format!("sp-session-{unique}-{thread:?}"))
    }

    #[test]
    fn package_round_trips_and_retains_plugin_bytes() {
        let root = temporary_package();
        let store = AtomicFileSessionStore::new(&root);
        let document = SessionDocument {
            schema_version: SESSION_SCHEMA_VERSION,
            model: Session::new(),
        };
        let package = SessionPackage::new(document);

        store.save_atomic(&package).expect("save package");
        store
            .save_plugin_state("plugin.example", &[1, 2], &[3, 4])
            .expect("save state");
        assert_eq!(store.load().expect("load package"), package);
        assert_eq!(
            store
                .load_plugin_state("plugin.example")
                .expect("load state"),
            (vec![1, 2], vec![3, 4])
        );

        fs::remove_dir_all(root).expect("remove package");
    }

    #[test]
    fn load_recovers_previous_payload_after_torn_write() {
        let root = temporary_package();
        let store = AtomicFileSessionStore::new(&root);
        let first = SessionPackage::new(SessionDocument::empty());
        let mut second_model = Session::new();
        second_model.version = SESSION_SCHEMA_VERSION;
        let second = SessionPackage::new(SessionDocument {
            schema_version: SESSION_SCHEMA_VERSION,
            model: second_model,
        });

        store.save_atomic(&first).expect("save first");
        store.save_atomic(&second).expect("save second");
        fs::write(store.layout().session_json(), b"{").expect("tear current payload");
        assert_eq!(store.load().expect("recover prior payload"), first);

        fs::remove_dir_all(root).expect("remove package");
    }

    #[test]
    fn recovery_marker_offers_unclean_sessions() {
        let root = temporary_package();
        let store = AtomicFileSessionStore::new(&root);
        store.mark_clean_shutdown(false).expect("write marker");
        assert!(store.offer_recovery().expect("read marker"));
        store.mark_clean_shutdown(true).expect("write marker");
        assert!(!store.offer_recovery().expect("read marker"));
        assert!(
            RecoveryMarker {
                clean_shutdown: true
            }
            .clean_shutdown
        );

        fs::remove_dir_all(root).expect("remove package");
    }

    #[test]
    fn controller_offers_recovery_after_unclean_autosave() {
        let root = temporary_package();
        {
            let mut controller = SessionController::empty(&root);
            controller.autosave().expect("autosave");
        }
        let controller = SessionController::open(&root).expect("reopen");
        assert!(controller.recovery_offered());
        assert!(!scene_may_restore_opaque_state());
        fs::remove_dir_all(root).expect("remove package");
    }
}

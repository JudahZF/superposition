//! Versioned, crash-recoverable session packages.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sp_model::{PluginSlot, Session};
use thiserror::Error;

/// Current `session.json` document schema version.
pub const SESSION_DOCUMENT_SCHEMA_VERSION: u32 = 1;
/// Current `manifest.json` package schema version.
pub const SESSION_PACKAGE_SCHEMA_VERSION: u32 = 1;
/// Compatibility alias for callers that predate separate schemas.
pub const SESSION_SCHEMA_VERSION: u32 = SESSION_DOCUMENT_SCHEMA_VERSION;

/// Decode limits at the package trust boundary. Limits include JSON syntax before serde allocates
/// nested application values.
pub const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum JSON nesting depth accepted before serde decodes a value.
pub const MAX_JSON_NESTING: usize = 64;
/// Maximum bytes accepted for one JSON string literal.
pub const MAX_JSON_STRING_BYTES: usize = 64 * 1024;
/// Maximum entries accepted for one JSON array or object.
pub const MAX_JSON_COLLECTION_ITEMS: usize = 16 * 1024;
/// Maximum bytes accepted for one opaque component or controller stream.
pub const MAX_OPAQUE_STATE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum bytes accepted for one plug-in instance identifier.
pub const MAX_INSTANCE_ID_BYTES: usize = 128;

/// The versioned `session.json` document holding the complete host-owned model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionDocument {
    /// Version used to decode this document.
    pub schema_version: u32,
    /// Host-owned topology, scenes, and MIDI mappings.
    pub model: Session,
}

impl SessionDocument {
    #[must_use]
    /// Returns an empty document at the current schema version.
    pub fn empty() -> Self {
        Self {
            schema_version: SESSION_DOCUMENT_SCHEMA_VERSION,
            model: Session::new(),
        }
    }

    /// Validates the document schema version and embedded model.
    /// # Errors
    /// Returns [`SessionError`] when the schema version is unsupported or the model is inconsistent.
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.schema_version != SESSION_DOCUMENT_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedDocumentVersion {
                found: self.schema_version,
                supported: SESSION_DOCUMENT_SCHEMA_VERSION,
            });
        }
        self.model.validate_for_alpha()?;
        for rack in &self.model.racks {
            for slot in &rack.slots {
                validate_instance_id(&slot.id.0)?;
            }
        }
        Ok(())
    }
}

impl Default for SessionDocument {
    fn default() -> Self {
        Self::empty()
    }
}

/// A declaration of opaque data captured for an instance. It is descriptive only: activation is
/// decided by the app/supervisor against its current scanner and quarantine facts.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PluginStateMetadata {
    /// Stable session instance identifier owning the streams.
    pub instance_id: String,
    /// Content fingerprint of the plug-in bundle at capture time.
    pub fingerprint: String,
    #[serde(default)]
    /// Exact byte length of the stored component stream.
    pub component_bytes: u64,
    #[serde(default)]
    /// Exact byte length of the stored controller stream.
    pub controller_bytes: u64,
    #[serde(default)]
    /// Version of the capture layout used to write the streams.
    pub capture_schema_version: u32,
    #[serde(default)]
    /// Compatibility facts recorded when the capture was made.
    pub activation: PluginActivationMetadata,
}

/// Component stream, controller stream, and metadata loaded for one plug-in instance.
pub type LoadedPluginState = (Vec<u8>, Vec<u8>, PluginStateMetadata);

/// Fresh opaque state captured for one plug-in instance during an explicit save.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedPluginState {
    /// Stable session instance identifier.
    pub instance_id: String,
    /// Component state stream captured while the plug-in was inactive.
    pub component: Vec<u8>,
    /// Controller-specific state stream captured after component state.
    pub controller: Vec<u8>,
    /// Fingerprint and activation facts stored beside the streams.
    pub metadata: PluginStateMetadata,
}

/// Activation facts recorded beside a capture for later restore decisions.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PluginActivationMetadata {
    #[serde(default)]
    /// The fingerprint used when bytes were captured; an empty value means legacy/unknown.
    pub captured_fingerprint: String,
    #[serde(default)]
    /// The last activation classification recorded for the instance.
    pub last_known_state: PluginActivationState,
    #[serde(default)]
    /// Optional operator-facing detail for a failed or blocked activation.
    pub diagnostic: Option<String>,
}

/// Classification of whether saved opaque state may be restored.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActivationState {
    /// No compatibility result has been recorded.
    #[default]
    Unknown,
    /// The installed plug-in matches the saved state and may be restored.
    Ready,
    /// No matching installed plug-in was found.
    Missing,
    /// A plug-in was found, but its fingerprint differs from the saved state.
    Changed,
    /// The matching plug-in is currently quarantined.
    Quarantined,
    /// Opaque-state restoration failed for an otherwise compatible plug-in.
    RestoreFailed,
}

/// Root `manifest.json`. The revision is the atomic commit record for every payload below it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManifest {
    /// Version used to decode this manifest.
    pub schema_version: u32,
    /// Atomic commit record naming the staged state this manifest publishes.
    pub revision: String,
    #[serde(default)]
    /// Free-form writer identification for diagnostics.
    pub writer: BTreeMap<String, String>,
    #[serde(default)]
    /// Declared opaque captures keyed by instance id.
    pub plugin_state: BTreeMap<String, PluginStateMetadata>,
}

impl PackageManifest {
    #[must_use]
    /// Creates a manifest at the current schema version with a fresh revision.
    pub fn new() -> Self {
        Self {
            schema_version: SESSION_PACKAGE_SCHEMA_VERSION,
            revision: revision_id(),
            writer: BTreeMap::new(),
            plugin_state: BTreeMap::new(),
        }
    }
    fn validate(&self) -> Result<(), SessionError> {
        if self.schema_version != SESSION_PACKAGE_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedPackageVersion {
                found: self.schema_version,
                supported: SESSION_PACKAGE_SCHEMA_VERSION,
            });
        }
        validate_safe_id(&self.revision, "revision")?;
        for (id, state) in &self.plugin_state {
            validate_instance_id(id)?;
            validate_instance_id(&state.instance_id)?;
            if id != &state.instance_id {
                return Err(SessionError::InvalidMetadata(
                    "plugin-state key differs from instance id".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Default for PackageManifest {
    fn default() -> Self {
        Self::new()
    }
}

/// In-memory package retained for source compatibility. On disk, its fields are split into
/// `manifest.json` and `session.json`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionPackage {
    /// Version used to decode the package manifest.
    pub schema_version: u32,
    /// The embedded versioned document.
    pub session_json: SessionDocument,
    #[serde(default)]
    /// The manifest describing every committed payload.
    pub manifest: PackageManifest,
}

impl SessionPackage {
    #[must_use]
    /// Wraps a document in a package with a default manifest.
    pub fn new(session_json: SessionDocument) -> Self {
        Self {
            schema_version: SESSION_PACKAGE_SCHEMA_VERSION,
            session_json,
            manifest: PackageManifest::new(),
        }
    }
    /// Validates the package schema version and embedded document.
    /// # Errors
    /// Returns [`SessionError`] when a schema version is unsupported or the model is inconsistent.
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.schema_version != SESSION_PACKAGE_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedPackageVersion {
                found: self.schema_version,
                supported: SESSION_PACKAGE_SCHEMA_VERSION,
            });
        }
        self.manifest.validate()?;
        self.session_json.validate()
    }
}

/// On-disk marker distinguishing clean shutdowns from crashes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryMarker {
    /// Whether the previous session exited cleanly.
    pub clean_shutdown: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TransactionMarker {
    revision: String,
}

/// Fixed on-disk layout of one session package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageLayout {
    root: PathBuf,
}

impl PackageLayout {
    #[must_use]
    /// Creates a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    #[must_use]
    /// Returns the package root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
    #[must_use]
    /// Returns the committed manifest path.
    pub fn manifest_json(&self) -> PathBuf {
        self.root.join("manifest.json")
    }
    #[must_use]
    /// Returns the committed document path.
    pub fn session_json(&self) -> PathBuf {
        self.root.join("session.json")
    }
    /// Legacy alias; recovery now retains a complete package, not one JSON file.
    #[must_use]
    /// Returns the rollback copy of the previous document.
    pub fn previous_session_json(&self) -> PathBuf {
        self.rollback_dir().join("session.json")
    }
    #[must_use]
    /// Returns the clean-shutdown recovery marker path.
    pub fn recovery_json(&self) -> PathBuf {
        self.root.join("recovery").join("latest.json")
    }
    #[must_use]
    /// Returns the recovery metadata directory.
    pub fn recovery_dir(&self) -> PathBuf {
        self.root.join("recovery")
    }
    #[must_use]
    /// Returns the committed opaque plug-in-state directory.
    pub fn plugin_state_dir(&self) -> PathBuf {
        self.root.join("plugin-state")
    }
    #[must_use]
    /// Returns the complete rollback package directory.
    pub fn rollback_dir(&self) -> PathBuf {
        self.recovery_dir().join("previous")
    }
    #[must_use]
    fn staging_dir(&self) -> PathBuf {
        self.root.join(".staging")
    }
    /// Returns the per-instance state directory for a validated instance id.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is unsafe.
    pub fn plugin_dir(&self, id: &str) -> Result<PathBuf, SessionError> {
        validate_instance_id(id)?;
        Ok(self.plugin_state_dir().join(id))
    }
    /// Returns the opaque component stream path for a validated instance id.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is unsafe.
    pub fn component_bin(&self, id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.plugin_dir(id)?.join("component.bin"))
    }
    /// Returns the opaque controller stream path for a validated instance id.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is unsafe.
    pub fn controller_bin(&self, id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.plugin_dir(id)?.join("controller.bin"))
    }
    /// Returns the metadata path for a validated instance id.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is unsafe.
    pub fn plugin_metadata_json(&self, id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.plugin_dir(id)?.join("metadata.json"))
    }
}

/// Failures surfaced by session persistence and validation.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error(
        "unsupported session package schema version {found}; newest supported version is {supported}"
    )]
    /// The package schema is newer than this build supports.
    UnsupportedPackageVersion {
        /// Schema version found in the package manifest.
        found: u32,
        /// Newest package schema version this build supports.
        supported: u32,
    },
    #[error(
        "unsupported session document schema version {found}; newest supported version is {supported}"
    )]
    /// The session document schema is newer than this build supports.
    UnsupportedDocumentVersion {
        /// Schema version found in `session.json`.
        found: u32,
        /// Newest document schema version this build supports.
        supported: u32,
    },
    #[error("invalid session model: {0}")]
    /// The host-owned session model violates the alpha contract.
    ModelValidation(#[from] sp_model::ValidationError),
    #[error("session package I/O failed: {0}")]
    /// A package filesystem operation failed.
    Io(#[from] std::io::Error),
    #[error("session package JSON failed: {0}")]
    /// A JSON package payload could not be decoded.
    Json(#[from] serde_json::Error),
    #[error("package input exceeds {limit}: {actual}")]
    /// A persisted input exceeded a bounded decode limit.
    LimitExceeded {
        /// Name of the enforced limit.
        limit: &'static str,
        /// Observed value that exceeded the limit.
        actual: u64,
    },
    #[error("invalid package identifier ({kind}): {value}")]
    /// A package identifier was not a safe path component.
    InvalidId {
        /// Identifier category being validated.
        kind: &'static str,
        /// Invalid identifier value.
        value: String,
    },
    #[error("invalid plug-in identifier: {0}")]
    /// A plug-in instance identifier was invalid.
    InvalidPluginId(String),
    #[error("invalid plug-in-state metadata: {0}")]
    /// Persisted plug-in-state metadata was inconsistent or malformed.
    InvalidMetadata(String),
    #[error("no recoverable session package")]
    /// Neither the current package nor rollback package was usable.
    NoRecoverableSession,
}

/// File store with a staged full-package transaction. `manifest.json` is renamed last; if a
/// process dies before that point, `recovery/previous` remains the authoritative rollback.
/// Transactional file-based store publishing complete packages atomically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtomicFileSessionStore {
    layout: PackageLayout,
}

impl AtomicFileSessionStore {
    #[must_use]
    /// Creates a store over the package layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            layout: PackageLayout::new(root),
        }
    }
    #[must_use]
    /// Returns the fixed on-disk layout.
    pub fn layout(&self) -> &PackageLayout {
        &self.layout
    }

    /// Atomically publishes the package with rollback and recovery markers.
    /// # Errors
    /// Returns [`SessionError`] when validation or any staged filesystem step fails.
    pub fn save_atomic(&self, package: &SessionPackage) -> Result<(), SessionError> {
        self.save_with_states(package, &[])
    }

    fn save_with_states(
        &self,
        package: &SessionPackage,
        replacements: &[CapturedPluginState],
    ) -> Result<(), SessionError> {
        package.validate()?;
        fs::create_dir_all(self.layout.root())?;
        let stage = self.layout.staging_dir().join(revision_id());
        let result = (|| {
            fs::create_dir_all(&stage)?;
            copy_dir_if_exists(&self.layout.plugin_state_dir(), &stage.join("plugin-state"))?;
            let mut manifest = package.manifest.clone();
            // Document-only callers (including autosave) retain the last successful opaque
            // capture and its activation metadata instead of silently dropping that reference.
            if manifest.plugin_state.is_empty()
                && let Ok(previous) = decode_manifest(&self.layout.manifest_json())
            {
                manifest.plugin_state = previous.plugin_state;
            }
            manifest.schema_version = SESSION_PACKAGE_SCHEMA_VERSION;
            manifest.revision = revision_id();
            for replacement in replacements {
                let id = replacement.instance_id.as_str();
                let component = replacement.component.as_slice();
                let controller = replacement.controller.as_slice();
                let mut metadata = replacement.metadata.clone();
                validate_instance_id(id)?;
                validate_opaque(component.len() as u64)?;
                validate_opaque(controller.len() as u64)?;
                id.clone_into(&mut metadata.instance_id);
                metadata.component_bytes = component.len() as u64;
                metadata.controller_bytes = controller.len() as u64;
                if metadata.fingerprint.is_empty() {
                    metadata
                        .fingerprint
                        .clone_from(&metadata.activation.captured_fingerprint);
                }
                write_file(
                    &stage.join("plugin-state").join(id).join("component.bin"),
                    component,
                )?;
                write_file(
                    &stage.join("plugin-state").join(id).join("controller.bin"),
                    controller,
                )?;
                write_json(
                    &stage.join("plugin-state").join(id).join("metadata.json"),
                    &metadata,
                )?;
                manifest.plugin_state.insert(id.to_owned(), metadata);
            }
            manifest.validate()?;
            write_json(&stage.join("session.json"), &package.session_json)?;
            write_json(&stage.join("manifest.json"), &manifest)?;
            sync_tree(&stage)?;
            self.publish(&stage)
        })();
        let _ = fs::remove_dir_all(&stage);
        result
    }

    fn publish(&self, stage: &Path) -> Result<(), SessionError> {
        let recovery = self.layout.recovery_dir();
        fs::create_dir_all(&recovery)?;
        let rollback = self.layout.rollback_dir();
        let rollback_tmp = recovery.join("previous.tmp");
        let _ = fs::remove_dir_all(&rollback_tmp);
        fs::create_dir_all(&rollback_tmp)?;
        copy_file_if_exists(
            &self.layout.manifest_json(),
            &rollback_tmp.join("manifest.json"),
        )?;
        copy_file_if_exists(
            &self.layout.session_json(),
            &rollback_tmp.join("session.json"),
        )?;
        copy_dir_if_exists(
            &self.layout.plugin_state_dir(),
            &rollback_tmp.join("plugin-state"),
        )?;
        sync_tree(&rollback_tmp)?;
        let _ = fs::remove_dir_all(&rollback);
        fs::rename(&rollback_tmp, &rollback)?;
        let staged_manifest = decode_manifest(&stage.join("manifest.json"))?;
        write_json(
            &recovery.join("transaction.json"),
            &TransactionMarker {
                revision: staged_manifest.revision,
            },
        )?;
        replace_dir(&stage.join("plugin-state"), &self.layout.plugin_state_dir())?;
        replace_file(&stage.join("session.json"), &self.layout.session_json())?;
        // Commit point: it names the exact staged state metadata and follows all payloads.
        replace_file(&stage.join("manifest.json"), &self.layout.manifest_json())?;
        let _ = fs::remove_file(recovery.join("transaction.json"));
        self.mark_clean_shutdown(false)
    }

    /// Loads the current package, honoring transaction markers and the rollback copy.
    /// # Errors
    /// Returns [`SessionError`] when no complete package can be decoded and validated.
    pub fn load(&self) -> Result<SessionPackage, SessionError> {
        let transaction = self.layout.recovery_dir().join("transaction.json");
        if transaction.exists() {
            let committed = self.transaction_committed(&transaction);
            return if committed {
                self.load_from(self.layout.root())
            } else {
                // A prepared rollback is authoritative for an uncommitted transaction. If the
                // rollback itself is unreadable (it never captured a previous state, or it is
                // torn), the still-uncommitted root is the best remaining complete state.
                self.load_from(&self.layout.rollback_dir())
                    .or_else(|error| {
                        if is_recoverable(&error) {
                            self.load_from(self.layout.root())
                        } else {
                            Err(error)
                        }
                    })
            };
        }
        self.load_from(self.layout.root()).or_else(|current| {
            if !is_recoverable(&current) {
                return Err(current);
            }
            match self.load_from(&self.layout.rollback_dir()) {
                Err(SessionError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(SessionError::NoRecoverableSession)
                }
                result => result,
            }
        })
    }

    #[allow(
        clippy::unused_self,
        reason = "reads follow the store's layout discipline"
    )]
    fn load_from(&self, root: &Path) -> Result<SessionPackage, SessionError> {
        let manifest = decode_manifest(&root.join("manifest.json"))?;
        let session_json = decode_document(&root.join("session.json"))?;
        validate_state_files(root, &manifest)?;
        let package = SessionPackage {
            schema_version: manifest.schema_version,
            session_json,
            manifest,
        };
        package.validate()?;
        Ok(package)
    }

    fn transaction_committed(&self, transaction: &Path) -> bool {
        read_json::<TransactionMarker>(transaction)
            .ok()
            .and_then(|marker| {
                decode_manifest(&self.layout.manifest_json())
                    .ok()
                    .map(|manifest| manifest.revision == marker.revision)
            })
            .unwrap_or(false)
    }

    fn readable_root(&self) -> PathBuf {
        let transaction = self.layout.recovery_dir().join("transaction.json");
        if transaction.exists() && !self.transaction_committed(&transaction) {
            self.layout.rollback_dir()
        } else {
            self.layout.root().to_path_buf()
        }
    }

    /// Records whether the session is shutting down cleanly.
    /// # Errors
    /// Returns [`SessionError`] when the recovery marker cannot be written.
    pub fn mark_clean_shutdown(&self, clean_shutdown: bool) -> Result<(), SessionError> {
        fs::create_dir_all(self.layout.recovery_dir())?;
        write_json(
            &self.layout.recovery_json(),
            &RecoveryMarker { clean_shutdown },
        )
    }
    /// Returns whether an unclean shutdown marker offers crash recovery.
    /// # Errors
    /// Returns [`SessionError`] when a present marker cannot be decoded.
    pub fn offer_recovery(&self) -> Result<bool, SessionError> {
        match read_json::<RecoveryMarker>(&self.layout.recovery_json()) {
            Ok(marker) => Ok(!marker.clean_shutdown),
            Err(SessionError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Compatible convenience API. It captures bytes through the same full-package transaction.
    /// # Errors
    /// Returns [`SessionError`] when validation, capture limits, or the atomic publish fail.
    pub fn save_plugin_state(
        &self,
        id: &str,
        component: &[u8],
        controller: &[u8],
    ) -> Result<(), SessionError> {
        self.save_plugin_state_with_metadata(
            id,
            component,
            controller,
            PluginStateMetadata {
                instance_id: id.into(),
                ..PluginStateMetadata::default()
            },
        )
    }
    /// Captures one plug-in state through the same full-package transaction.
    /// # Errors
    /// Returns [`SessionError`] when validation, capture limits, or the atomic publish fail.
    pub fn save_plugin_state_with_metadata(
        &self,
        id: &str,
        component: &[u8],
        controller: &[u8],
        metadata: PluginStateMetadata,
    ) -> Result<(), SessionError> {
        let package = self.load()?;
        self.save_with_states(
            &package,
            &[CapturedPluginState {
                instance_id: id.to_owned(),
                component: component.to_vec(),
                controller: controller.to_vec(),
                metadata,
            }],
        )
    }
    /// Loads the persisted component and controller streams for one plug-in instance.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is invalid or a stream cannot be read.
    pub fn load_plugin_state(&self, id: &str) -> Result<(Vec<u8>, Vec<u8>), SessionError> {
        validate_instance_id(id)?;
        let root = self.readable_root();
        let component = read_opaque(&root.join("plugin-state").join(id).join("component.bin"))?;
        let controller = read_opaque(&root.join("plugin-state").join(id).join("controller.bin"))?;
        Ok((component, controller))
    }
    /// Loads the persisted activation metadata for one plug-in instance.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is invalid or metadata cannot be decoded.
    pub fn load_plugin_state_metadata(
        &self,
        id: &str,
    ) -> Result<PluginStateMetadata, SessionError> {
        validate_instance_id(id)?;
        let root = self.readable_root();
        read_json(&root.join("plugin-state").join(id).join("metadata.json"))
    }
}

/// Session persistence storage interface.
pub trait SessionStore {
    /// Loads a persisted session document.
    /// # Errors
    /// Returns an error when no document can be decoded.
    fn load(&self) -> Result<SessionDocument, Box<dyn std::error::Error + Send + Sync>>;
    /// Persists a session document.
    /// # Errors
    /// Returns an error when the document cannot be persisted.
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

/// App/supervisor-facing inert outcome. Callers must bypass these slots and display the reason.
/// Why a saved slot is shown as a placeholder instead of a live plug-in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaceholderReason {
    /// No installed plug-in matches the session fingerprint.
    MissingPlugin,
    /// An installed plug-in exists but its fingerprint changed.
    ChangedPlugin,
    /// The matching plug-in is quarantined.
    Quarantined,
}
/// A saved slot preserved verbatim while its plug-in cannot be loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingPluginPlaceholder {
    /// Rack owning the placeholder slot.
    pub rack_index: usize,
    /// Slot position within the rack.
    pub slot_index: usize,
    /// Stable session instance identifier.
    pub instance_id: String,
    /// Fingerprint the session expects for this slot.
    pub fingerprint: String,
    /// Why the slot is currently a placeholder.
    pub reason: PlaceholderReason,
}

/// High-level controller pairing a loaded document with its atomic store.
#[derive(Debug)]
pub struct SessionController {
    store: AtomicFileSessionStore,
    document: SessionDocument,
    dirty: bool,
    recovery_offered: bool,
}
impl SessionController {
    /// Opens the package at `root`, loading the current document and recovery state.
    /// # Errors
    /// Returns [`SessionError`] when no recoverable package exists or decoding fails.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let store = AtomicFileSessionStore::new(root);
        let recovery_offered = store.offer_recovery()?;
        let document = match store.load() {
            Ok(p) => p.session_json,
            Err(SessionError::NoRecoverableSession) => SessionDocument::empty(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            store,
            document,
            dirty: false,
            recovery_offered,
        })
    }
    #[must_use]
    /// Creates a controller over a fresh empty document without touching the disk.
    pub fn empty(root: impl Into<PathBuf>) -> Self {
        Self {
            store: AtomicFileSessionStore::new(root),
            document: SessionDocument::empty(),
            dirty: false,
            recovery_offered: false,
        }
    }
    #[must_use]
    /// Returns whether an unclean shutdown offer is pending a user decision.
    pub const fn recovery_offered(&self) -> bool {
        self.recovery_offered
    }
    /// Clears the pending recovery offer after the user has decided.
    pub fn acknowledge_recovery_offer(&mut self) {
        self.recovery_offered = false;
    }
    #[must_use]
    /// Returns the current document.
    pub const fn document(&self) -> &SessionDocument {
        &self.document
    }
    /// Returns the mutable document and marks the session dirty.
    pub fn document_mut(&mut self) -> &mut SessionDocument {
        self.dirty = true;
        &mut self.document
    }
    #[must_use]
    /// Returns whether unsaved changes exist.
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }
    /// Atomically saves the current document and clears the dirty flag.
    /// # Errors
    /// Returns [`SessionError`] when validation or the atomic publish fails.
    pub fn save(&mut self) -> Result<(), SessionError> {
        self.save_with_plugin_states(&[])
    }
    /// Atomically saves the model and every freshly captured opaque plug-in state.
    /// # Errors
    /// Returns [`SessionError`] when validation, capture limits, or the atomic publish fail.
    pub fn save_with_plugin_states(
        &mut self,
        states: &[CapturedPluginState],
    ) -> Result<(), SessionError> {
        self.store
            .save_with_states(&SessionPackage::new(self.document.clone()), states)?;
        self.store.mark_clean_shutdown(true)?;
        self.dirty = false;
        Ok(())
    }
    /// Persists the current document in place as a periodic autosave.
    /// # Errors
    /// Returns [`SessionError`] when validation or the atomic publish fails.
    pub fn autosave(&mut self) -> Result<(), SessionError> {
        self.store
            .save_atomic(&SessionPackage::new(self.document.clone()))?;
        self.store.mark_clean_shutdown(false)?;
        self.dirty = false;
        Ok(())
    }
    /// Records a clean shutdown in the recovery marker.
    /// # Errors
    /// Returns [`SessionError`] when the marker cannot be written.
    pub fn mark_clean_exit(&self) -> Result<(), SessionError> {
        self.store.mark_clean_shutdown(true)
    }
    /// Loads saved opaque streams for an instance, returning `None` when none were captured.
    /// # Errors
    /// Returns [`SessionError`] when the instance id is invalid or a present stream is unreadable.
    pub fn load_plugin_state(&self, id: &str) -> Result<Option<LoadedPluginState>, SessionError> {
        match self.store.load_plugin_state(id) {
            Ok((component, controller)) => Ok(Some((
                component,
                controller,
                self.store.load_plugin_state_metadata(id)?,
            ))),
            Err(SessionError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    #[must_use]
    /// Lists slots whose plug-ins are not installed, preserving their saved configuration.
    pub fn missing_plugin_placeholders(
        &self,
        available: &BTreeSet<String>,
        quarantined: &BTreeSet<String>,
    ) -> Vec<MissingPluginPlaceholder> {
        self.document
            .model
            .racks
            .iter()
            .enumerate()
            .flat_map(|(rack_index, rack)| {
                rack.slots
                    .iter()
                    .enumerate()
                    .filter_map(move |(slot_index, slot)| {
                        let fingerprint = slot.plugin.fingerprint.digest.clone();
                        let reason = if quarantined.contains(&fingerprint) {
                            Some(PlaceholderReason::Quarantined)
                        } else if !available.contains(&fingerprint) {
                            Some(PlaceholderReason::MissingPlugin)
                        } else {
                            None
                        }?;
                        Some(MissingPluginPlaceholder {
                            rack_index,
                            slot_index,
                            instance_id: slot.id.0.clone(),
                            fingerprint,
                            reason,
                        })
                    })
            })
            .collect()
    }
}

#[must_use]
/// Scenes never restore opaque plug-in state; recall stays parameter-oriented.
pub const fn scene_may_restore_opaque_state() -> bool {
    false
}
#[must_use]
/// Returns whether a slot references a plug-in that is not currently installed.
pub fn slot_is_placeholder(slot: &PluginSlot, available: &BTreeSet<String>) -> bool {
    !available.contains(&slot.plugin.fingerprint.digest)
}
/// Classifies a scanned candidate without activating it. The app/supervisor must bypass every
/// result except `Ready` and surface the returned state to the user.
#[must_use]
/// Classifies whether saved opaque state may be restored for an instance.
pub fn plugin_activation_state(
    expected_fingerprint: &str,
    discovered_fingerprint: Option<&str>,
    quarantined: bool,
) -> PluginActivationState {
    if quarantined {
        PluginActivationState::Quarantined
    } else if discovered_fingerprint.is_none() {
        PluginActivationState::Missing
    } else if discovered_fingerprint != Some(expected_fingerprint) {
        PluginActivationState::Changed
    } else {
        PluginActivationState::Ready
    }
}

fn decode_manifest(path: &Path) -> Result<PackageManifest, SessionError> {
    let value = bounded_json(path)?;
    let migrated = migrate_package(value)?;
    let manifest: PackageManifest = serde_json::from_value(migrated)?;
    manifest.validate()?;
    Ok(manifest)
}
fn decode_document(path: &Path) -> Result<SessionDocument, SessionError> {
    let value = bounded_json(path)?;
    let migrated = migrate_document(value)?;
    let document: SessionDocument = serde_json::from_value(migrated)?;
    document.validate()?;
    Ok(document)
}

/// Directional migration dispatcher. A newer version is never rewritten or guessed.
fn migrate_package(mut value: Value) -> Result<Value, SessionError> {
    loop {
        let version = schema_version(&value, "package")?;
        if version > SESSION_PACKAGE_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedPackageVersion {
                found: version,
                supported: SESSION_PACKAGE_SCHEMA_VERSION,
            });
        }
        if version == SESSION_PACKAGE_SCHEMA_VERSION {
            return Ok(value);
        }
        value = match version {
            0 => migrate_package_v0_to_v1(value),
            _ => unreachable!(),
        };
    }
}
fn migrate_document(mut value: Value) -> Result<Value, SessionError> {
    loop {
        let version = schema_version(&value, "document")?;
        if version > SESSION_DOCUMENT_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedDocumentVersion {
                found: version,
                supported: SESSION_DOCUMENT_SCHEMA_VERSION,
            });
        }
        if version == SESSION_DOCUMENT_SCHEMA_VERSION {
            return Ok(value);
        }
        value = match version {
            0 => migrate_document_v0_to_v1(value),
            _ => unreachable!(),
        };
    }
}
fn migrate_package_v0_to_v1(mut value: Value) -> Value {
    value["schema_version"] = Value::from(1);
    value
        .as_object_mut()
        .expect("version checked")
        .entry("revision")
        .or_insert_with(|| Value::from("migrated-v0"));
    value
}
fn migrate_document_v0_to_v1(mut value: Value) -> Value {
    value["schema_version"] = Value::from(1);
    value
}
fn schema_version(value: &Value, kind: &'static str) -> Result<u32, SessionError> {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            SessionError::InvalidMetadata(format!("{kind} schema_version is missing or invalid"))
        })
}

fn bounded_json(path: &Path) -> Result<Value, SessionError> {
    let bytes = read_limited(path, MAX_JSON_BYTES, "JSON file bytes")?;
    validate_json_shape(&bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, SessionError> {
    Ok(serde_json::from_value(bounded_json(path)?)?)
}
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), SessionError> {
    write_file(path, &serde_json::to_vec_pretty(value)?)
}
fn read_limited(path: &Path, maximum: u64, limit: &'static str) -> Result<Vec<u8>, SessionError> {
    let length = fs::metadata(path)?.len();
    if length > maximum {
        return Err(SessionError::LimitExceeded {
            limit,
            actual: length,
        });
    }
    Ok(fs::read(path)?)
}
fn read_opaque(path: &Path) -> Result<Vec<u8>, SessionError> {
    read_limited(path, MAX_OPAQUE_STATE_BYTES, "opaque state bytes")
}
fn validate_opaque(length: u64) -> Result<(), SessionError> {
    if length > MAX_OPAQUE_STATE_BYTES {
        Err(SessionError::LimitExceeded {
            limit: "opaque state bytes",
            actual: length,
        })
    } else {
        Ok(())
    }
}
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn copy_file_if_exists(from: &Path, to: &Path) -> Result<(), SessionError> {
    if from.exists() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to)?;
    }
    Ok(())
}
fn copy_dir_if_exists(from: &Path, to: &Path) -> Result<(), SessionError> {
    if !from.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_if_exists(&path, &target)?;
        } else {
            copy_file_if_exists(&path, &target)?;
        }
    }
    Ok(())
}
fn replace_file(from: &Path, to: &Path) -> Result<(), SessionError> {
    let tmp = to.with_extension("next");
    copy_file_if_exists(from, &tmp)?;
    fs::rename(tmp, to)?;
    Ok(())
}
fn replace_dir(from: &Path, to: &Path) -> Result<(), SessionError> {
    let tmp = to.with_extension("next");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    copy_dir_if_exists(from, &tmp)?;
    let _ = fs::remove_dir_all(to);
    fs::rename(tmp, to)?;
    Ok(())
}
fn sync_tree(root: &Path) -> Result<(), SessionError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}
fn validate_state_files(root: &Path, manifest: &PackageManifest) -> Result<(), SessionError> {
    for (id, expected) in &manifest.plugin_state {
        let base = root.join("plugin-state").join(id);
        let component = read_opaque(&base.join("component.bin"))?;
        let controller = read_opaque(&base.join("controller.bin"))?;
        if component.len() as u64 != expected.component_bytes
            || controller.len() as u64 != expected.controller_bytes
        {
            return Err(SessionError::InvalidMetadata(format!(
                "opaque state size mismatch for {id}"
            )));
        }
        let metadata: PluginStateMetadata = read_json(&base.join("metadata.json"))?;
        if &metadata != expected {
            return Err(SessionError::InvalidMetadata(format!(
                "opaque metadata mismatch for {id}"
            )));
        }
    }
    Ok(())
}
fn is_recoverable(error: &SessionError) -> bool {
    matches!(error, SessionError::Io(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::Interrupted | std::io::ErrorKind::UnexpectedEof))
        || matches!(
            error,
            SessionError::Json(_) | SessionError::InvalidMetadata(_)
        )
}
fn validate_json_shape(bytes: &[u8]) -> Result<(), SessionError> {
    let mut depth = 0usize;
    let mut string = 0usize;
    let mut quoted = false;
    let mut escape = false;
    let mut items = 0usize;
    for &byte in bytes {
        if quoted {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                quoted = false;
                string = 0;
            } else {
                string += 1;
                if string > MAX_JSON_STRING_BYTES {
                    return Err(SessionError::LimitExceeded {
                        limit: "JSON string bytes",
                        actual: string as u64,
                    });
                }
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_JSON_NESTING {
                    return Err(SessionError::LimitExceeded {
                        limit: "JSON nesting",
                        actual: depth as u64,
                    });
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b',' => {
                items += 1;
                if items > MAX_JSON_COLLECTION_ITEMS {
                    return Err(SessionError::LimitExceeded {
                        limit: "JSON collection items",
                        actual: items as u64,
                    });
                }
            }
            _ => {}
        }
    }
    if quoted || depth != 0 {
        return Err(SessionError::InvalidMetadata(
            "unterminated JSON structure".into(),
        ));
    }
    Ok(())
}
fn validate_instance_id(id: &str) -> Result<(), SessionError> {
    validate_safe_id(id, "plug-in instance")
        .map_err(|_| SessionError::InvalidPluginId(id.to_owned()))
}
fn validate_safe_id(id: &str, kind: &'static str) -> Result<(), SessionError> {
    if id.is_empty()
        || id.len() > MAX_INSTANCE_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || id == "."
        || id == ".."
        || Path::new(id)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(SessionError::InvalidId {
            kind,
            value: id.into(),
        });
    }
    Ok(())
}
fn revision_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos());
    format!("r-{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    fn root() -> PathBuf {
        // Parallel tests can observe the same clock reading, so a per-process counter keeps
        // every test root unique.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "sp-session-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    #[test]
    fn state_capture_round_trips_with_its_metadata() {
        let root = root();
        let store = AtomicFileSessionStore::new(&root);
        store
            .save_atomic(&SessionPackage::new(SessionDocument::empty()))
            .expect("save");
        let metadata = PluginStateMetadata {
            fingerprint: "fingerprint".into(),
            activation: PluginActivationMetadata {
                captured_fingerprint: "fingerprint".into(),
                last_known_state: PluginActivationState::Ready,
                diagnostic: None,
            },
            ..PluginStateMetadata::default()
        };
        store
            .save_plugin_state_with_metadata("slot-1", &[1, 2], &[3], metadata.clone())
            .expect("state");
        assert_eq!(
            store.load_plugin_state("slot-1").expect("read"),
            (vec![1, 2], vec![3])
        );
        assert_eq!(
            store
                .load_plugin_state_metadata("slot-1")
                .expect("metadata"),
            PluginStateMetadata {
                instance_id: "slot-1".into(),
                component_bytes: 2,
                controller_bytes: 1,
                ..metadata
            }
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn explicit_save_publishes_all_captured_states_together() {
        let root = root();
        let store = AtomicFileSessionStore::new(&root);
        let captures = [
            CapturedPluginState {
                instance_id: "slot-1".into(),
                component: vec![1],
                controller: vec![2],
                metadata: PluginStateMetadata::default(),
            },
            CapturedPluginState {
                instance_id: "slot-2".into(),
                component: vec![3],
                controller: vec![4],
                metadata: PluginStateMetadata::default(),
            },
        ];
        store
            .save_with_states(&SessionPackage::new(SessionDocument::empty()), &captures)
            .expect("atomic state save");

        let package = store.load().expect("package");
        assert_eq!(package.manifest.plugin_state.len(), 2);
        assert_eq!(
            store.load_plugin_state("slot-1").expect("slot 1"),
            (vec![1], vec![2])
        );
        assert_eq!(
            store.load_plugin_state("slot-2").expect("slot 2"),
            (vec![3], vec![4])
        );
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn interrupted_transaction_loads_complete_rollback() {
        let root = root();
        let store = AtomicFileSessionStore::new(&root);
        store
            .save_atomic(&SessionPackage::new(SessionDocument::empty()))
            .expect("save");
        fs::write(
            store.layout().recovery_dir().join("transaction.json"),
            b"{\"revision\":\"not-the-current-revision\"}",
        )
        .expect("marker");
        assert_eq!(
            store.load().expect("rollback").session_json,
            SessionDocument::empty()
        );
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn forward_versions_are_not_migrated() {
        let root = root();
        fs::create_dir_all(&root).expect("dir");
        fs::write(
            root.join("manifest.json"),
            br#"{"schema_version":2,"revision":"r"}"#,
        )
        .expect("manifest");
        fs::write(
            root.join("session.json"),
            serde_json::to_vec(&SessionDocument::empty()).expect("session"),
        )
        .expect("write");
        assert!(matches!(
            AtomicFileSessionStore::new(&root).load(),
            Err(SessionError::UnsupportedPackageVersion { .. })
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }
    #[test]
    fn unsafe_ids_and_oversized_opaque_bytes_are_rejected() {
        assert!(
            AtomicFileSessionStore::new(root())
                .layout()
                .plugin_dir("../escape")
                .is_err()
        );
        assert!(matches!(
            validate_opaque(MAX_OPAQUE_STATE_BYTES + 1),
            Err(SessionError::LimitExceeded { .. })
        ));
        assert_eq!(
            plugin_activation_state("expected", Some("changed"), false),
            PluginActivationState::Changed
        );
    }
}

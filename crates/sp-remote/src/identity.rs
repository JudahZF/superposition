//! This installation's TLS identity and the peers it has paired with.

use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};

const CERTIFICATE_FILE: &str = "identity.der";
const KEY_FILE: &str = "identity.key";
const TRUST_FILE: &str = "trusted.json";

/// SHA-256 of a peer's certificate. Pairing pins this value.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Fingerprints a DER certificate.
    #[must_use]
    pub fn of(certificate: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, certificate);
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        Self(bytes)
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    /// Lowercase hex in groups of four bytes, short enough to compare by eye.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 && index % 4 == 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Fingerprint({self})")
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let hex: String = text.chars().filter(|c| *c != ':').collect();
        let mut bytes = [0; 32];
        if hex.len() != 64 {
            return Err(serde::de::Error::custom("a fingerprint has 32 bytes"));
        }
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(Self(bytes))
    }
}

/// A self-signed certificate and key, created once per installation.
pub struct Identity {
    pub(crate) certificate: CertificateDer<'static>,
    pub(crate) key: PrivatePkcs8KeyDer<'static>,
    fingerprint: Fingerprint,
}

impl Identity {
    /// Loads the identity from `directory`, creating it on first use (key file mode 0600).
    ///
    /// # Errors
    /// Returns an I/O error when the files cannot be read or written, or are corrupt.
    pub fn load_or_create(directory: &Path) -> io::Result<Self> {
        let certificate_path = directory.join(CERTIFICATE_FILE);
        let key_path = directory.join(KEY_FILE);
        if !certificate_path.exists() || !key_path.exists() {
            std::fs::create_dir_all(directory)?;
            let generated = rcgen::generate_simple_self_signed(vec!["superposition".to_owned()])
                .map_err(io::Error::other)?;
            write_private(&key_path, &generated.key_pair.serialize_der())?;
            std::fs::write(&certificate_path, generated.cert.der())?;
        }
        let certificate = CertificateDer::from(std::fs::read(certificate_path)?);
        let key = PrivatePkcs8KeyDer::from(std::fs::read(key_path)?);
        Ok(Self {
            fingerprint: Fingerprint::of(&certificate),
            certificate,
            key,
        })
    }

    /// This installation's fingerprint, shown when pairing.
    #[must_use]
    pub const fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub(crate) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

/// A paired peer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerRecord {
    /// The name the peer gave when it paired.
    pub name: String,
    /// The pinned certificate fingerprint.
    pub fingerprint: Fingerprint,
    /// When pairing happened, in Unix seconds.
    pub paired_at_unix: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct TrustFile {
    /// Clients an engine accepts.
    #[serde(default)]
    clients: Vec<PeerRecord>,
    /// Engines a client accepts, by the address it dials.
    #[serde(default)]
    servers: BTreeMap<String, PeerRecord>,
}

/// Pinned peers, persisted as JSON beside the identity.
pub struct TrustStore {
    path: PathBuf,
    file: TrustFile,
}

impl TrustStore {
    /// Loads the store from `directory`; a missing file is an empty store.
    ///
    /// # Errors
    /// Returns an I/O error for an unreadable or malformed file.
    pub fn load(directory: &Path) -> io::Result<Self> {
        let path = directory.join(TRUST_FILE);
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => TrustFile::default(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, file })
    }

    /// The paired client with this fingerprint.
    #[must_use]
    pub fn client(&self, fingerprint: Fingerprint) -> Option<&PeerRecord> {
        self.file
            .clients
            .iter()
            .find(|peer| peer.fingerprint == fingerprint)
    }

    /// The pinned engine for an address.
    #[must_use]
    pub fn server(&self, address: &str) -> Option<&PeerRecord> {
        self.file.servers.get(address)
    }

    /// Every paired client.
    #[must_use]
    pub fn clients(&self) -> &[PeerRecord] {
        &self.file.clients
    }

    pub(crate) fn pin_client(&mut self, name: String, fingerprint: Fingerprint) -> io::Result<()> {
        self.file
            .clients
            .retain(|peer| peer.fingerprint != fingerprint);
        self.file.clients.push(record(name, fingerprint));
        self.save()
    }

    pub(crate) fn pin_server(
        &mut self,
        address: String,
        name: String,
        fingerprint: Fingerprint,
    ) -> io::Result<()> {
        self.file.servers.insert(address, record(name, fingerprint));
        self.save()
    }

    /// Forgets a client; it must pair again.
    ///
    /// # Errors
    /// Returns an I/O error when the store cannot be written.
    pub fn forget_client(&mut self, fingerprint: Fingerprint) -> io::Result<()> {
        self.file
            .clients
            .retain(|peer| peer.fingerprint != fingerprint);
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.file).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(temporary, &self.path)
    }
}

fn record(name: String, fingerprint: Fingerprint) -> PeerRecord {
    PeerRecord {
        name,
        fingerprint,
        paired_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Fingerprint, Identity, TrustStore};

    #[test]
    fn identity_persists_and_trust_round_trips() {
        let directory = std::env::temp_dir().join(format!("sp-remote-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let first = Identity::load_or_create(&directory).unwrap();
        let second = Identity::load_or_create(&directory).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());

        let mut trust = TrustStore::load(&directory).unwrap();
        trust
            .pin_client("Laptop".into(), first.fingerprint())
            .unwrap();
        trust
            .pin_server("engine:4433".into(), "Engine".into(), Fingerprint([7; 32]))
            .unwrap();
        let trust = TrustStore::load(&directory).unwrap();
        assert_eq!(trust.client(first.fingerprint()).unwrap().name, "Laptop");
        assert_eq!(
            trust.server("engine:4433").unwrap().fingerprint,
            Fingerprint([7; 32])
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

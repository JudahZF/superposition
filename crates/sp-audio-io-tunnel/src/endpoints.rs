//! The IDs of the two Superposition endpoints, made once and kept on disk.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use tunnel_types::{EndpointDecl, EndpointId, EndpointKind};

use crate::TunnelChannels;

/// The IDs of "Superposition inputs" and "Superposition outputs". They stay the same for the
/// life of the Tunnel directory, so the master's routes to them survive restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Endpoints {
    /// "Superposition inputs": the sink that feeds the renderer.
    pub inputs: EndpointId,
    /// "Superposition outputs": the source that the renderer feeds.
    pub outputs: EndpointId,
}

impl Endpoints {
    /// Reads the IDs from `path`, or makes new ones (`UUIDv7`) and writes them there
    /// atomically. The file holds the inputs ID, then the outputs ID, 16 bytes each.
    ///
    /// # Errors
    ///
    /// The file cannot be read or written, or it does not hold two IDs. A damaged file is not
    /// replaced: routes may name the old IDs.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => match bytes.as_chunks::<16>() {
                ([inputs, outputs], []) => Ok(Self {
                    inputs: EndpointId(*inputs),
                    outputs: EndpointId(*outputs),
                }),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} does not hold two endpoint IDs", path.display()),
                )),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let endpoints = Self::generate();
                endpoints.save(path)?;
                Ok(endpoints)
            }
            Err(error) => Err(error),
        }
    }

    /// Two new IDs that are not kept anywhere, for tests and simulations.
    pub fn generate() -> Self {
        Self {
            inputs: EndpointId(uuid::Uuid::now_v7().into_bytes()),
            outputs: EndpointId(uuid::Uuid::now_v7().into_bytes()),
        }
    }

    /// Writes the IDs: temp file, fsync, rename.
    fn save(&self, path: &Path) -> io::Result<()> {
        let temporary = path.with_extension("tmp");
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&self.inputs.0)?;
        file.write_all(&self.outputs.0)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    }

    /// The device's endpoint declarations: "Superposition inputs" at position
    /// [`INPUTS`](crate::INPUTS) and "Superposition outputs" at
    /// [`OUTPUTS`](crate::OUTPUTS), with `channels`.
    pub fn decls(&self, channels: TunnelChannels) -> Vec<EndpointDecl> {
        vec![
            EndpointDecl {
                id: self.inputs,
                kind: EndpointKind::Sink,
                channel_count: channels.inputs,
                label: "Superposition inputs".into(),
                channel_labels: None,
            },
            EndpointDecl {
                id: self.outputs,
                kind: EndpointKind::Source,
                channel_count: channels.outputs,
                label: "Superposition outputs".into(),
                channel_labels: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Routes name endpoint IDs, so a restart must keep them, and a damaged file must not be
    /// replaced by new IDs.
    #[test]
    fn ids_are_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!(
            "sp-tunnel-endpoints-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("endpoints");
        let first = Endpoints::load_or_create(&path).unwrap();
        assert_ne!(first.inputs, first.outputs);
        assert_eq!(Endpoints::load_or_create(&path).unwrap(), first);

        fs::write(&path, [0; 31]).unwrap();
        let error = Endpoints::load_or_create(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap().len(), 31);
        fs::remove_dir_all(&dir).unwrap();
    }
}

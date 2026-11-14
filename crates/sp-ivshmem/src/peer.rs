use crate::{Doorbell, Interrupt, MAX_VECTORS, PROTOCOL_VERSION, wire};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use std::{collections::BTreeMap, io, os::unix::net::UnixStream, path::Path, time::Duration};

/// The longest time the server may take to send the set-up messages.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A change in the other peers of a port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerEvent {
    /// A peer connected; its doorbells are available.
    Joined(u16),
    /// A peer disconnected; its doorbells are gone.
    Left(u16),
}

/// One peer of a port server: the shared memory, this peer's interrupts and the doorbells
/// of the other peers.
#[derive(Debug)]
pub struct PortPeer {
    stream: UnixStream,
    id: u16,
    memory: OwnedFd,
    vectors: u16,
    own: Vec<OwnedFd>,
    peers: BTreeMap<u16, Vec<OwnedFd>>,
}

impl PortPeer {
    /// Connects to the server at `socket` and receives the set-up messages.
    ///
    /// `vectors` is this peer's vector count. Extra vectors from the server are closed.
    ///
    /// # Errors
    ///
    /// Returns `InvalidData` for a protocol error, `TimedOut` if the set-up takes longer
    /// than 5 seconds, and the operating-system error if the connection fails.
    pub fn connect(socket: &Path, vectors: u16) -> io::Result<Self> {
        if !(1..=MAX_VECTORS).contains(&vectors) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ivshmem vector count is out of range",
            ));
        }
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(SETUP_TIMEOUT))?;
        match next(&stream)? {
            (PROTOCOL_VERSION, None) => {}
            (version, _) => {
                return Err(protocol_error(format!(
                    "unsupported ivshmem protocol version {version}"
                )));
            }
        }
        let id = match next(&stream)? {
            (id, None) => u16::try_from(id)
                .map_err(|_| protocol_error(format!("ivshmem peer ID {id} is out of range")))?,
            (_, Some(_)) => return Err(protocol_error("ivshmem peer ID came with a descriptor")),
        };
        let (-1, Some(memory)) = next(&stream)? else {
            return Err(protocol_error("ivshmem server sent no shared memory"));
        };
        let mut peer = Self {
            stream,
            id,
            memory,
            vectors,
            own: Vec::with_capacity(usize::from(vectors)),
            peers: BTreeMap::new(),
        };
        while peer.own.len() < usize::from(vectors) {
            let (value, fd) = next(&peer.stream)?;
            peer.apply(value, fd)?;
        }
        peer.stream.set_read_timeout(None)?;
        peer.stream.set_nonblocking(true)?;
        Ok(peer)
    }

    /// This peer's ID.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// The port's shared memory.
    pub fn memory(&self) -> BorrowedFd<'_> {
        self.memory.as_fd()
    }

    /// The IDs of the other peers, in order.
    pub fn peer_ids(&self) -> impl Iterator<Item = u16> + '_ {
        self.peers.keys().copied()
    }

    /// The other peer if there is exactly one. On a VM port this is the guest.
    pub fn only_other_peer(&self) -> Option<u16> {
        let mut ids = self.peer_ids();
        match (ids.next(), ids.next()) {
            (Some(id), None) => Some(id),
            _ => None,
        }
    }

    /// A copy of the doorbell that interrupts `peer` on `vector`.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` if the peer or vector is not connected, or the operating-system
    /// error of a failed duplicate.
    pub fn doorbell(&self, peer: u16, vector: u16) -> io::Result<Doorbell> {
        let fd = self
            .peers
            .get(&peer)
            .and_then(|fds| fds.get(usize::from(vector)))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "ivshmem peer or vector is not connected",
                )
            })?;
        Ok(Doorbell(fd.try_clone()?))
    }

    /// A copy of this peer's interrupt on `vector`.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a vector outside this peer's count, or the operating-system
    /// error of a failed duplicate.
    pub fn interrupt(&self, vector: u16) -> io::Result<Interrupt> {
        let fd = self.own.get(usize::from(vector)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "ivshmem vector is out of range")
        })?;
        Ok(Interrupt(fd.try_clone()?))
    }

    /// Reads the connection and disconnection messages that have arrived, without waiting.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionAborted` if the server has gone, and `InvalidData` for a protocol
    /// error.
    pub fn poll_events(&mut self) -> io::Result<Vec<PeerEvent>> {
        let mut events = Vec::new();
        loop {
            match wire::recv(&self.stream) {
                Ok(Some((value, fd))) => events.extend(self.apply(value, fd)?),
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "ivshmem server closed",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(events),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Applies one message after the set-up header.
    fn apply(&mut self, value: i64, fd: Option<OwnedFd>) -> io::Result<Option<PeerEvent>> {
        let id = u16::try_from(value)
            .map_err(|_| protocol_error(format!("ivshmem peer ID {value} is out of range")))?;
        let Some(fd) = fd else {
            return Ok(self.peers.remove(&id).map(|_| PeerEvent::Left(id)));
        };
        if id == self.id {
            if self.own.len() < usize::from(self.vectors) {
                self.own.push(fd);
            }
            return Ok(None);
        }
        let fds = self.peers.entry(id).or_default();
        if fds.len() == usize::from(self.vectors) {
            return Ok(None);
        }
        fds.push(fd);
        // A peer is announced once, when its last vector arrives.
        Ok((fds.len() == usize::from(self.vectors)).then_some(PeerEvent::Joined(id)))
    }
}

/// Receives one set-up message; the end of the stream is an error.
fn next(stream: &UnixStream) -> io::Result<(i64, Option<OwnedFd>)> {
    wire::recv(stream)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "ivshmem server closed"))
}

fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

use crate::{MAX_VECTORS, PROTOCOL_VERSION, wire};
use rustix::{
    event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll},
    fd::{AsFd, OwnedFd},
    fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create},
};
use std::{
    io::{self, Read},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::Duration,
};

/// The server of one VM port: the shared memory and the eventfds of every peer.
///
/// The memory is a sealed memfd: no peer can shrink or grow it. A peer that sends data or
/// fails a send is disconnected; one bad peer never stops the server.
///
/// Peer IDs count up and are not reused until they wrap at 65 536, as in QEMU's own server.
/// QEMU's `ivshmem` device frees a departed peer's eventfd array without clearing it, and
/// writes into that freed memory if a new peer arrives with the same ID (QEMU 10.2,
/// `close_peer_eventfds` and `resize_peers`): a host that connects again must get a new ID.
pub struct PortServer {
    listener: UnixListener,
    memory: OwnedFd,
    size: u64,
    vectors: u16,
    peers: Vec<ServerPeer>,
    /// The next ID to try.
    next_id: u16,
}

struct ServerPeer {
    id: u16,
    stream: UnixStream,
    eventfds: Vec<OwnedFd>,
}

impl PortServer {
    /// Creates `size` bytes of shared memory, rounded up to a power of two (a PCI BAR size),
    /// and listens on `socket`.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a zero size or a vector count outside `1..=MAX_VECTORS`,
    /// and the operating-system error if the memory or the socket cannot be made. The socket
    /// path must not exist.
    pub fn bind(socket: &Path, size: u64, vectors: u16) -> io::Result<Self> {
        if !(1..=MAX_VECTORS).contains(&vectors) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ivshmem vector count is out of range",
            ));
        }
        let size = size
            .checked_next_power_of_two()
            .filter(|&size| size > 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "ivshmem size is out of range")
            })?;
        let memory = memfd_create(
            "sp-ivshmem",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        ftruncate(&memory, size)?;
        fcntl_add_seals(
            &memory,
            SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL,
        )?;
        Ok(Self {
            listener: UnixListener::bind(socket)?,
            memory,
            size,
            vectors,
            peers: Vec::new(),
            next_id: 0,
        })
    }

    /// The size of the shared memory in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The IDs of the connected peers, in connection order.
    pub fn peer_ids(&self) -> impl Iterator<Item = u16> + '_ {
        self.peers.iter().map(|peer| peer.id)
    }

    /// Serves peers until `poll` or `accept` fails, and returns that error.
    pub fn run(&mut self) -> io::Error {
        loop {
            if let Err(error) = self.serve_once(None) {
                return error;
            }
        }
    }

    /// Waits up to `timeout` (forever for `None`) for connections and disconnections, and
    /// handles them.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error of a failed `poll` or `accept`.
    pub fn serve_once(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let timeout = timeout
            .map(Timespec::try_from)
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "timeout is too long"))?;
        let (accept, readable) = {
            let mut fds = Vec::with_capacity(self.peers.len() + 1);
            fds.push(PollFd::new(&self.listener, PollFlags::IN));
            fds.extend(
                self.peers
                    .iter()
                    .map(|peer| PollFd::new(&peer.stream, PollFlags::IN)),
            );
            if poll(&mut fds, timeout.as_ref())? == 0 {
                return Ok(());
            }
            let readable: Vec<u16> = fds[1..]
                .iter()
                .zip(&self.peers)
                .filter(|(fd, _)| !fd.revents().is_empty())
                .map(|(_, peer)| peer.id)
                .collect();
            (!fds[0].revents().is_empty(), readable)
        };
        for id in readable {
            self.check_peer(id);
        }
        if accept {
            let (stream, _) = self.listener.accept()?;
            self.add_peer(stream)?;
        }
        Ok(())
    }

    /// A peer never sends: a readable stream is a disconnection or a protocol error.
    fn check_peer(&mut self, id: u16) {
        let Some(peer) = self.peers.iter_mut().find(|peer| peer.id == id) else {
            return;
        };
        let mut byte = [0_u8; 1];
        match peer.stream.read(&mut byte) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => self.remove_peer(id),
        }
    }

    fn add_peer(&mut self, stream: UnixStream) -> io::Result<()> {
        let Some(id) = (0..=u16::MAX)
            .map(|offset| self.next_id.wrapping_add(offset))
            .find(|id| self.peers.iter().all(|peer| peer.id != *id))
        else {
            // 65 536 peers: refuse this one. The stream closes on drop.
            return Ok(());
        };
        self.next_id = id.wrapping_add(1);
        let eventfds = (0..self.vectors)
            .map(|_| eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK))
            .collect::<Result<Vec<_>, _>>()?;
        if self.send_setup(&stream, id, &eventfds).is_err() {
            return Ok(());
        }
        let failed: Vec<u16> = self
            .peers
            .iter()
            .filter(|peer| {
                eventfds
                    .iter()
                    .any(|fd| wire::send(&peer.stream, i64::from(id), Some(fd.as_fd())).is_err())
            })
            .map(|peer| peer.id)
            .collect();
        self.peers.push(ServerPeer {
            id,
            stream,
            eventfds,
        });
        for id in failed {
            self.remove_peer(id);
        }
        Ok(())
    }

    fn send_setup(&self, stream: &UnixStream, id: u16, eventfds: &[OwnedFd]) -> io::Result<()> {
        wire::send(stream, PROTOCOL_VERSION, None)?;
        wire::send(stream, i64::from(id), None)?;
        wire::send(stream, -1, Some(self.memory.as_fd()))?;
        for peer in &self.peers {
            for fd in &peer.eventfds {
                wire::send(stream, i64::from(peer.id), Some(fd.as_fd()))?;
            }
        }
        for fd in eventfds {
            wire::send(stream, i64::from(id), Some(fd.as_fd()))?;
        }
        Ok(())
    }

    /// Removes a peer and tells the others. A peer that fails this notice is removed too.
    fn remove_peer(&mut self, id: u16) {
        let mut gone = vec![id];
        while let Some(id) = gone.pop() {
            let Some(index) = self.peers.iter().position(|peer| peer.id == id) else {
                continue;
            };
            self.peers.remove(index);
            gone.extend(
                self.peers
                    .iter()
                    .filter(|peer| wire::send(&peer.stream, i64::from(id), None).is_err())
                    .map(|peer| peer.id),
            );
        }
    }
}

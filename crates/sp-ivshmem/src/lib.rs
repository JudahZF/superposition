//! The ivshmem doorbell protocol of QEMU (`docs/specs/ivshmem-spec.rst`), version 0.
//!
//! A VM port is one `ivshmem-doorbell` PCI device. Its [`PortServer`] owns the shared memory
//! and one eventfd for each peer and vector. QEMU and the host connect to it as peers
//! ([`PortPeer`]). A peer interrupts another peer with one write to that peer's eventfd
//! ([`Doorbell`]); KVM delivers QEMU's eventfds to the guest as MSIs through irqfds.
//!
//! The server must outlive its peers: QEMU's ivshmem device cannot connect again to a new
//! server. Linux only.

#![cfg(target_os = "linux")]

mod peer;
mod server;
mod wire;

pub use peer::{PeerEvent, PortPeer};
pub use server::PortServer;

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::{AsFd, BorrowedFd, OwnedFd},
};
use std::{io, time::Duration};

/// The only protocol version this crate speaks.
pub const PROTOCOL_VERSION: i64 = 0;

/// The most interrupt vectors a port may have. QEMU's device allows more; Lambda uses one.
pub const MAX_VECTORS: u16 = 64;

/// One peer's interrupt for one vector: the write side of its eventfd.
///
/// [`Self::ring`] is one non-blocking `write` and is safe on a real-time thread. The value
/// owns its own copy of the descriptor, so a supervisor that drops the peer never closes a
/// descriptor that a real-time thread still writes.
#[derive(Debug)]
pub struct Doorbell(OwnedFd);

impl Doorbell {
    /// Interrupts the peer. Returns `false` only if the eventfd counter is full or closed.
    pub fn ring(&self) -> bool {
        rustix::io::write(&self.0, &1_u64.to_ne_bytes()) == Ok(8)
    }

    /// Makes a second, independent copy of this doorbell.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if the descriptor cannot be duplicated.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}

impl AsFd for Doorbell {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<Doorbell> for OwnedFd {
    fn from(doorbell: Doorbell) -> Self {
        doorbell.0
    }
}

/// This peer's own interrupt for one vector: the read side of its eventfd.
#[derive(Debug)]
pub struct Interrupt(OwnedFd);

impl Interrupt {
    /// Waits until the vector fires or `timeout` passes, then clears it.
    ///
    /// Returns the count of rings since the last clear, or zero on a timeout.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error of a failed `poll` or `read`.
    pub fn wait(&self, timeout: Duration) -> io::Result<u64> {
        let timeout = Timespec::try_from(timeout).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "interrupt timeout is too long")
        })?;
        let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
        if poll(&mut fds, Some(&timeout))? == 0 {
            return Ok(0);
        }
        self.clear()
    }

    /// Clears the vector without waiting. Returns the count of rings since the last clear.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error of a failed `read`.
    pub fn clear(&self) -> io::Result<u64> {
        let mut count = [0_u8; 8];
        match rustix::io::read(&self.0, &mut count) {
            Ok(8) => Ok(u64::from_ne_bytes(count)),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short eventfd read",
            )),
            Err(rustix::io::Errno::AGAIN) => Ok(0),
            Err(error) => Err(error.into()),
        }
    }
}

impl AsFd for Interrupt {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<Interrupt> for OwnedFd {
    fn from(interrupt: Interrupt) -> Self {
        interrupt.0
    }
}

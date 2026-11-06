//! One protocol message: an 8-byte little-endian signed number, with at most one descriptor.

use rustix::{
    fd::{BorrowedFd, OwnedFd},
    net::{
        RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
    },
};
use std::{
    io::{self, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::unix::net::UnixStream,
};

pub(crate) fn send(stream: &UnixStream, value: i64, fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let bytes = value.to_le_bytes();
    let fds = fd.map(|fd| [fd]);
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if let Some(fds) = &fds {
        control.push(SendAncillaryMessage::ScmRights(fds));
    }
    let sent = sendmsg(
        stream,
        &[IoSlice::new(&bytes)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    if sent != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short ivshmem message",
        ));
    }
    Ok(())
}

/// Receives one message, or `None` at end of stream.
///
/// The buffer is exactly one message long, so one call never joins two messages and each
/// descriptor stays with the number it was sent with.
pub(crate) fn recv(stream: &UnixStream) -> io::Result<Option<(i64, Option<OwnedFd>)>> {
    let mut bytes = [0_u8; 8];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let message = recvmsg(
        stream,
        &mut [IoSliceMut::new(&mut bytes)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    if message.bytes == 0 {
        return Ok(None);
    }
    if message.bytes != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short ivshmem message",
        ));
    }
    // More than one descriptor breaks the protocol; the extra ones close on drop.
    let fd = control
        .drain()
        .filter_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .next();
    Ok(Some((i64::from_le_bytes(bytes), fd)))
}
